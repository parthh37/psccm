//! Console side of the agent protocol: the outbound endpoints agents call
//! (`/api/agent/enroll`, `/checkin`, `/job-result`) and their own
//! authentication. These sit *outside* the console's bearer-token auth — an
//! agent proves itself with the enrollment token (to enroll) and thereafter
//! with its `agent_id:agent_key` pair.
//!
//! The router is assembled in [`crate::web`]; this module only provides the
//! handlers and the [`agent_auth`] middleware.

use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Json, extract::Request};
use serde_json::json;
use std::time::Duration;
use tokio::time::Instant;

use super::{CHECKIN_HOLD_SECS, CheckinReq, CheckinResp, EnrollReq, EnrollResp, JobResult, next_poll_ms};
use crate::app::App;
use crate::db::NewComputer;
use crate::web::ApiError;

/// The authenticated agent id, injected into the request by [`agent_auth`].
#[derive(Clone)]
pub struct AgentId(pub String);

type ApiResult = Result<Json<serde_json::Value>, ApiError>;

/// Validate `Authorization: Agent <id>:<key>` against the stored agent key.
pub async fn agent_auth(State(app): State<App>, headers: HeaderMap, mut req: Request, next: Next) -> Response {
    let unauth = || (StatusCode::UNAUTHORIZED, Json(json!({ "error": "agent not recognized" }))).into_response();
    let Some((id, key)) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Agent "))
        .and_then(|v| v.split_once(':'))
    else {
        return unauth();
    };
    match app.db.agent_key(id) {
        Ok(Some(stored)) if stored == key => {
            req.extensions_mut().insert(AgentId(id.to_string()));
            next.run(req).await
        }
        _ => unauth(),
    }
}

/// Register (or re-register) an agent. Validates the shared enrollment token,
/// creates the matching computer record and returns the agent's credentials.
pub async fn enroll(State(app): State<App>, Json(req): Json<EnrollReq>) -> ApiResult {
    let settings = app.agent_settings();
    if !settings.enabled {
        return Err(anyhow::anyhow!("agent enrollment is disabled on this console").into());
    }
    let expected = app.enroll_token()?;
    if req.token.is_empty() || req.token != expected {
        return Err(anyhow::anyhow!("invalid enrollment token").into());
    }
    if req.hostname.trim().is_empty() {
        return Err(anyhow::anyhow!("hostname is required").into());
    }
    let os = if req.os.eq_ignore_ascii_case("linux") { "linux" } else { "windows" };

    // Keep (or refresh) the computer record; agents never delete computers.
    app.db.upsert_computer(&NewComputer {
        name: req.hostname.clone(),
        os: Some(os.to_string()),
        os_detail: (!req.os_version.is_empty()).then(|| req.os_version.clone()),
        source: Some("agent".to_string()),
        address: req.address.clone(),
        ..Default::default()
    })?;

    let key = uuid::Uuid::new_v4().simple().to_string();
    let agent_id = match app.db.agent_id_for_computer(&req.hostname)? {
        Some(id) => {
            app.db.reenroll_agent(&id, &key, &req, req.address.as_deref())?;
            id
        }
        None => {
            let id = uuid::Uuid::new_v4().simple().to_string();
            app.db.create_agent(&id, &key, &req, req.address.as_deref())?;
            id
        }
    };
    tracing::info!("agent enrolled: {} ({os}) -> {agent_id}", req.hostname);
    Ok(Json(json!(EnrollResp { agent_id, agent_key: key })))
}

/// Long-poll check-in: records metrics and terminal output, then waits briefly
/// for queued jobs or terminal input to hand back.
pub async fn checkin(
    State(app): State<App>,
    Extension(AgentId(id)): Extension<AgentId>,
    Json(req): Json<CheckinReq>,
) -> Result<Json<CheckinResp>, ApiError> {
    app.db.agent_checkin(&id, &req.agent_version, req.metrics.as_ref())?;
    for chunk in &req.terminal {
        app.agents.push_output(chunk);
    }

    let notifier = app.agents.notifier(&id);
    let deadline = Instant::now() + Duration::from_secs(CHECKIN_HOLD_SECS);
    loop {
        let jobs = app.db.claim_jobs(&id)?;
        let terminal = app.agents.take_agent_ctls(&id);
        let has_work = !jobs.is_empty() || !terminal.is_empty();
        let remaining = deadline.saturating_duration_since(Instant::now());
        if has_work || remaining.is_zero() {
            let poll_ms = next_poll_ms(app.agents.agent_has_terminal(&id), !jobs.is_empty());
            return Ok(Json(CheckinResp { jobs, terminal, poll_ms }));
        }
        tokio::select! {
            _ = notifier.notified() => {}
            _ = tokio::time::sleep(remaining) => {}
        }
    }
}

/// Receive a finished job's result; cache inventory results for the UI.
pub async fn job_result(
    State(app): State<App>,
    Extension(AgentId(id)): Extension<AgentId>,
    Json(result): Json<JobResult>,
) -> ApiResult {
    // Only accept results for jobs that belong to this agent.
    let kind = match app.db.get_job(result.job_id)? {
        Some(j) if j.agent_id == id => j.kind,
        _ => return Err(anyhow::anyhow!("job not found for this agent").into()),
    };
    app.db.complete_job(&result)?;
    if result.status == "success"
        && let Some(inv) = inventory_kind(&kind)
    {
        app.db.set_inventory(&id, inv, &result.data)?;
    }
    Ok(Json(json!({ "ok": true })))
}

/// Map a job kind to the inventory bucket its result should update.
fn inventory_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "tasks_list" => Some("tasks"),
        "firewall_list" => Some("firewall"),
        "users_list" => Some("users"),
        "eventlog" => Some("eventlog"),
        _ => None,
    }
}
