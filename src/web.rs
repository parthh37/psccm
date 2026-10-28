//! Web console: JSON API plus the embedded single-page UI.

use anyhow::{anyhow, bail};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::net::SocketAddr;

use crate::ad::{self, AdComputer, AdSettings};
use crate::app::{App, WinRmSettings};
use crate::db::{ComputerUpdate, DeployOptions, Group, GroupRules, NewComputer, NewCredential, Schedule, TargetSpec};
use crate::{catalog, gpo, update};
use crate::engine;
use crate::package::{Package, Shell};
use crate::scheduler;

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE: &str = include_str!("../web/style.css");

pub struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = format!("{:#}", self.0);
        let code = if msg.ends_with("not found") { StatusCode::NOT_FOUND } else { StatusCode::BAD_REQUEST };
        (code, Json(json!({ "error": msg }))).into_response()
    }
}

type ApiResult<T = Value> = Result<Json<T>, ApiError>;

#[derive(Clone)]
struct St {
    app: App,
    token: Option<String>,
}

pub async fn serve(
    app: App,
    addr: SocketAddr,
    token: Option<String>,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    if !addr.ip().is_loopback() && token.is_none() {
        bail!("listening on {addr} needs --token (or PSCCM_TOKEN) so others on the network cannot deploy");
    }
    let st = St { app, token };
    let api = Router::new()
        .route("/summary", get(summary))
        .route("/computers", get(list_computers).post(add_computers))
        .route("/computers/{id}", put(update_computer))
        .route("/computers/delete", post(delete_computers))
        .route("/computers/check", post(check_computers))
        .route("/groups", get(list_groups).post(save_group))
        .route("/groups/preview", post(preview_group))
        .route("/groups/{id}", delete(delete_group))
        .route("/groups/{id}/members", post(group_members))
        .route("/gpos", get(list_gpos))
        .route("/gpos/imported", get(list_imported_gpos))
        .route("/gpos/import-folder", post(import_gpo_folder))
        .route("/gpos/{guid}", get(get_gpo).delete(delete_gpo))
        .route("/gpos/{guid}/import", post(import_gpo))
        .route("/update", get(update_status))
        .route("/update/check", post(update_check))
        .route("/update/install", post(update_install))
        .route("/settings/update", put(save_update_settings))
        .route("/packages", get(list_packages).post(create_package))
        .route("/packages/validate", post(validate_package))
        .route("/packages/{id}", get(get_package).put(save_package).delete(delete_package))
        .route("/repository", get(list_repository))
        .route("/catalog", get(catalog_list))
        .route("/catalog/check", post(catalog_check))
        .route("/catalog/settings", put(catalog_settings))
        .route("/catalog/{id}", put(catalog_save).delete(catalog_remove))
        .route("/catalog/{id}/add", post(catalog_add))
        .route("/catalog/{id}/update", post(catalog_update))
        .route("/credentials", get(list_credentials).post(add_credential))
        .route("/credentials/{id}", delete(delete_credential))
        .route("/settings", get(get_settings))
        .route("/settings/ad", put(save_ad_settings))
        .route("/settings/winrm", put(save_winrm_settings))
        .route("/known-hosts/{host}", delete(forget_host))
        .route("/ad/preview", post(ad_preview))
        .route("/ad/import", post(ad_import))
        .route("/deployments", get(list_deployments).post(create_deployment))
        .route("/deployments/{id}", get(get_deployment).delete(delete_deployment))
        .route("/deployments/{id}/cancel", post(cancel_deployment))
        .route("/deployments/{id}/redeploy", post(redeploy))
        .route("/targets/{id}/log", get(target_log))
        .route("/schedules", get(list_schedules).post(save_schedule))
        .route("/schedules/{id}", delete(delete_schedule))
        .layer(middleware::from_fn_with_state(st.clone(), auth));
    let router = Router::new()
        .route("/", get(|| async { ([(header::CACHE_CONTROL, "no-cache")], Html(INDEX)) }))
        .route("/app.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript"), (header::CACHE_CONTROL, "no-cache")], APP_JS) }))
        .route("/style.css", get(|| async { ([(header::CONTENT_TYPE, "text/css"), (header::CACHE_CONTROL, "no-cache")], STYLE) }))
        .nest("/api", api)
        .with_state(st);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).with_graceful_shutdown(shutdown).await?;
    Ok(())
}

async fn auth(State(st): State<St>, headers: HeaderMap, req: Request, next: Next) -> Response {
    if let Some(token) = &st.token {
        let given = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if given != Some(token.as_str()) {
            return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" }))).into_response();
        }
    }
    next.run(req).await
}

// ------------------------------------------------------------------ summary

async fn summary(State(st): State<St>) -> ApiResult {
    let app = &st.app;
    let computers = app.db.list_computers()?;
    let lib = app.library()?;
    let deployments = app.db.list_deployments(8)?;
    let running = deployments.iter().filter(|d| d.status == "running").count();
    Ok(Json(json!({
        "computers": computers.len(),
        "online": computers.iter().filter(|c| c.online == Some(true)).count(),
        "offline": computers.iter().filter(|c| c.online == Some(false)).count(),
        "windows": computers.iter().filter(|c| c.os == "windows").count(),
        "linux": computers.iter().filter(|c| c.os == "linux").count(),
        "packages": lib.packages.len(),
        "package_errors": lib.errors,
        "running": running,
        "recent": deployments,
        "credentials": app.db.list_credentials()?.len(),
    })))
}

// ---------------------------------------------------------------- computers

async fn list_computers(State(st): State<St>) -> ApiResult {
    Ok(Json(json!(st.app.db.list_computers()?)))
}

#[derive(Deserialize)]
struct AddComputers {
    /// One per line, or separated by commas/spaces.
    names: String,
    #[serde(default)]
    os: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    credential_id: Option<i64>,
}

async fn add_computers(State(st): State<St>, Json(req): Json<AddComputers>) -> ApiResult {
    let names: Vec<&str> = req
        .names
        .split(|c: char| c == ',' || c == ';' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .collect();
    if names.is_empty() {
        return Err(anyhow!("enter at least one computer name").into());
    }
    let os = req.os.filter(|o| o == "windows" || o == "linux");
    for name in &names {
        let existing = st.app.db.get_computer(name)?.map(|c| c.tags).unwrap_or_default();
        let tags: Vec<String> = existing.into_iter().chain(req.tags.iter().cloned()).collect();
        st.app.db.upsert_computer(&NewComputer {
            name: name.to_string(),
            os: os.clone(),
            tags: Some(tags),
            credential_id: req.credential_id,
            ..Default::default()
        })?;
    }
    Ok(Json(json!({ "added": names.len() })))
}

async fn update_computer(State(st): State<St>, Path(id): Path<i64>, Json(u): Json<ComputerUpdate>) -> ApiResult {
    st.app.db.update_computer(id, &u)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct Ids {
    ids: Vec<i64>,
}

async fn delete_computers(State(st): State<St>, Json(req): Json<Ids>) -> ApiResult {
    st.app.db.delete_computers(&req.ids)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct Names {
    #[serde(default)]
    names: Option<Vec<String>>,
}

async fn check_computers(State(st): State<St>, Json(req): Json<Names>) -> ApiResult {
    let res = engine::check_online(&st.app, req.names).await?;
    let online = res.iter().filter(|(_, o)| *o).count();
    Ok(Json(json!({ "checked": res.len(), "online": online })))
}

// ------------------------------------------------------------------- groups

async fn list_groups(State(st): State<St>) -> ApiResult {
    let all = st.app.db.list_computers()?;
    let groups: Vec<Value> = st
        .app
        .db
        .list_groups()?
        .into_iter()
        .map(|g| {
            let members: Vec<&str> = crate::groups::members(&g, &all).into_iter().map(|c| c.name.as_str()).collect();
            let mut v = json!(g);
            v["members"] = json!(members);
            v
        })
        .collect();
    let mut ad_groups: Vec<String> = all.iter().flat_map(|c| c.ad_groups.clone()).collect();
    ad_groups.sort_by_key(|g| g.to_lowercase());
    ad_groups.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    Ok(Json(json!({ "groups": groups, "ad_groups": ad_groups })))
}

async fn save_group(State(st): State<St>, Json(g): Json<Group>) -> ApiResult {
    if g.name.trim().is_empty() {
        return Err(anyhow!("group name is required").into());
    }
    if !matches!(g.kind.as_str(), "static" | "dynamic") {
        return Err(anyhow!("kind must be static or dynamic").into());
    }
    if g.kind == "dynamic" && g.rules.is_empty() {
        return Err(anyhow!("a dynamic group needs at least one rule").into());
    }
    let id = st.app.db.save_group(&g)?;
    if g.kind == "static" && g.id == 0 && !g.members.is_empty() {
        st.app.db.set_group_members(id, &g.members, true)?;
    }
    Ok(Json(json!({ "id": id })))
}

async fn preview_group(State(st): State<St>, Json(rules): Json<GroupRules>) -> ApiResult {
    let all = st.app.db.list_computers()?;
    let names: Vec<&str> = all.iter().filter(|c| rules.matches(c)).map(|c| c.name.as_str()).collect();
    Ok(Json(json!({ "members": names })))
}

async fn delete_group(State(st): State<St>, Path(id): Path<i64>) -> ApiResult {
    st.app.db.delete_group(id)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct MemberChange {
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
}

async fn group_members(State(st): State<St>, Path(id): Path<i64>, Json(m): Json<MemberChange>) -> ApiResult {
    let added = st.app.db.set_group_members(id, &m.add, true)?;
    let removed = st.app.db.set_group_members(id, &m.remove, false)?;
    Ok(Json(json!({ "added": added, "removed": removed })))
}

// --------------------------------------------------------------------- GPOs

async fn list_gpos(State(st): State<St>) -> ApiResult {
    Ok(Json(json!({ "gpos": gpo::list(&st.app).await? })))
}

async fn list_imported_gpos(State(st): State<St>) -> ApiResult {
    Ok(Json(json!({ "gpos": gpo::list_imported(&st.app) })))
}

async fn get_gpo(State(st): State<St>, Path(guid): Path<String>) -> ApiResult {
    let (meta, snap) = gpo::read_snapshot(&st.app, &guid).map_err(|_| anyhow!("GPO snapshot not found"))?;
    Ok(Json(json!({ "meta": meta, "snapshot": snap })))
}

async fn import_gpo(State(st): State<St>, Path(guid): Path<String>) -> ApiResult {
    let meta = gpo::import(&st.app, &guid).await?;
    Ok(Json(json!(meta)))
}

#[derive(Deserialize)]
struct ImportFolder {
    path: String,
    #[serde(default)]
    name: Option<String>,
}

async fn import_gpo_folder(State(st): State<St>, Json(r): Json<ImportFolder>) -> ApiResult {
    let meta = gpo::import_folder(&st.app, std::path::Path::new(r.path.trim()), r.name)?;
    Ok(Json(json!(meta)))
}

async fn delete_gpo(State(st): State<St>, Path(guid): Path<String>) -> ApiResult {
    gpo::delete(&st.app, &guid)?;
    Ok(Json(json!({ "ok": true })))
}

// ------------------------------------------------------------------ updates

async fn update_status(State(st): State<St>) -> ApiResult {
    Ok(Json(json!(update::status(&st.app))))
}

async fn update_check(State(st): State<St>) -> ApiResult {
    Ok(Json(json!(update::check(&st.app).await?)))
}

async fn update_install(State(st): State<St>) -> ApiResult {
    let msg = update::install(&st.app).await?;
    Ok(Json(json!({ "message": msg })))
}

async fn save_update_settings(State(st): State<St>, Json(s): Json<update::UpdateSettings>) -> ApiResult {
    st.app.db.set_setting("update", &s)?;
    Ok(Json(json!({ "ok": true })))
}

// ----------------------------------------------------------------- packages

async fn list_packages(State(st): State<St>) -> ApiResult {
    let lib = st.app.library()?;
    let builtin: Vec<&str> = crate::app::builtin_packages().iter().map(|(f, _)| *f).collect();
    let mut list: Vec<Value> = lib
        .packages
        .values()
        .map(|p| {
            let file = lib.files.get(&p.id).and_then(|f| f.file_name()).map(|f| f.to_string_lossy().to_string());
            json!({
                "id": p.id, "name": p.name, "version": p.version, "description": p.description,
                "category": if p.category.is_empty() { "Other" } else { &p.category },
                "platforms": p.platforms, "variables": p.variables, "steps": p.steps.len(),
                "has_detect": p.detect.windows.is_some() || p.detect.linux.is_some(),
                "builtin": file.as_deref().is_some_and(|f| builtin.contains(&f)),
                "file": file,
            })
        })
        .collect();
    list.sort_by(|a, b| {
        (a["category"].as_str(), a["name"].as_str()).cmp(&(b["category"].as_str(), b["name"].as_str()))
    });
    Ok(Json(json!({ "packages": list, "errors": lib.errors })))
}

async fn get_package(State(st): State<St>, Path(id): Path<String>) -> ApiResult {
    let lib = st.app.library()?;
    let file = lib.files.get(&id).ok_or_else(|| anyhow!("package not found"))?;
    let yaml = std::fs::read_to_string(file)?;
    Ok(Json(json!({ "yaml": yaml, "package": lib.packages[&id] })))
}

#[derive(Deserialize)]
struct YamlBody {
    yaml: String,
}

async fn validate_package(Json(b): Json<YamlBody>) -> ApiResult {
    Ok(Json(match Package::from_yaml(&b.yaml) {
        Ok(p) => json!({ "ok": true, "id": p.id, "steps": p.steps.len() }),
        Err(e) => json!({ "ok": false, "error": format!("{e:#}") }),
    }))
}

fn write_package(app: &App, yaml: &str, replacing: Option<&str>) -> anyhow::Result<String> {
    let pkg = Package::from_yaml(yaml)?;
    let lib = app.library()?;
    if let Some(old) = replacing {
        if old != pkg.id {
            bail!("the id in the YAML ('{}') must stay '{old}'; use Duplicate to make a new package", pkg.id);
        }
    } else if lib.packages.contains_key(&pkg.id) {
        bail!("a package with id '{}' already exists", pkg.id);
    }
    let path = lib.files.get(&pkg.id).cloned().unwrap_or_else(|| app.packages_dir().join(format!("{}.yaml", pkg.id)));
    std::fs::write(&path, yaml)?;
    Ok(pkg.id)
}

async fn create_package(State(st): State<St>, Json(b): Json<YamlBody>) -> ApiResult {
    let id = write_package(&st.app, &b.yaml, None)?;
    Ok(Json(json!({ "id": id })))
}

async fn save_package(State(st): State<St>, Path(id): Path<String>, Json(b): Json<YamlBody>) -> ApiResult {
    write_package(&st.app, &b.yaml, Some(&id))?;
    Ok(Json(json!({ "id": id })))
}

async fn delete_package(State(st): State<St>, Path(id): Path<String>) -> ApiResult {
    let lib = st.app.library()?;
    let file = lib.files.get(&id).ok_or_else(|| anyhow!("package not found"))?;
    // Built-in files are recreated on start; rename instead of deleting so
    // the deletion sticks.
    let disabled = file.with_extension("yaml.disabled");
    std::fs::rename(file, disabled)?;
    Ok(Json(json!({ "ok": true })))
}

async fn list_repository(State(st): State<St>) -> ApiResult {
    let files: Vec<Value> = engine::list_repository(&st.app)
        .into_iter()
        .map(|(path, size)| json!({ "path": path, "size": size }))
        .collect();
    Ok(Json(json!({ "dir": st.app.repo_dir(), "files": files })))
}

// ------------------------------------------------------------------ catalog

async fn catalog_list(State(st): State<St>) -> ApiResult {
    Ok(Json(catalog::overview(&st.app)?))
}

/// Runs in the background; the page polls `checking`.
async fn catalog_check(State(st): State<St>) -> ApiResult {
    catalog::start_check(&st.app, true);
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct CatalogSettingsReq {
    auto_check: bool,
}

async fn catalog_settings(State(st): State<St>, Json(r): Json<CatalogSettingsReq>) -> ApiResult {
    catalog::set_auto_check(&st.app, r.auto_check)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct CatalogAppReq {
    #[serde(default)]
    deploy_groups: Vec<String>,
}

async fn catalog_save(State(st): State<St>, Path(id): Path<String>, Json(r): Json<CatalogAppReq>) -> ApiResult {
    catalog::set_groups(&st.app, &id, r.deploy_groups)?;
    Ok(Json(json!({ "ok": true })))
}

/// Downloads run in the background; the page polls the app's `busy` progress.
async fn catalog_add(State(st): State<St>, Path(id): Path<String>) -> ApiResult {
    catalog::start_refresh(&st.app, &id, true)?;
    Ok(Json(json!({ "ok": true })))
}

async fn catalog_update(State(st): State<St>, Path(id): Path<String>) -> ApiResult {
    catalog::start_refresh(&st.app, &id, false)?;
    Ok(Json(json!({ "ok": true })))
}

async fn catalog_remove(State(st): State<St>, Path(id): Path<String>) -> ApiResult {
    catalog::remove(&st.app, &id)?;
    Ok(Json(json!({ "ok": true })))
}

// -------------------------------------------------------------- credentials

async fn list_credentials(State(st): State<St>) -> ApiResult {
    Ok(Json(json!(st.app.db.list_credentials()?)))
}

async fn add_credential(State(st): State<St>, Json(mut c): Json<NewCredential>) -> ApiResult {
    if c.name.trim().is_empty() {
        return Err(anyhow!("name is required").into());
    }
    if !matches!(c.kind.as_str(), "windows" | "ssh" | "ldap") {
        return Err(anyhow!("kind must be windows, ssh or ldap").into());
    }
    if c.kind != "windows" && c.username.trim().is_empty() {
        return Err(anyhow!("username is required").into());
    }
    c.key_path = c.key_path.filter(|k| !k.trim().is_empty());
    c.default_for = c.default_for.filter(|d| d == "windows" || d == "linux");
    let id = st.app.db.add_credential(&c)?;
    Ok(Json(json!({ "id": id })))
}

async fn delete_credential(State(st): State<St>, Path(id): Path<i64>) -> ApiResult {
    st.app.db.delete_credential(id)?;
    Ok(Json(json!({ "ok": true })))
}

// ----------------------------------------------------------------- settings

async fn get_settings(State(st): State<St>) -> ApiResult {
    let ad: AdSettings = st.app.db.get_setting("ad")?;
    Ok(Json(json!({
        "ad": ad,
        "winrm": st.app.winrm_settings(),
        "data_dir": st.app.data_dir,
        "packages_dir": st.app.packages_dir(),
        "repo_dir": st.app.repo_dir(),
        "console_os": std::env::consts::OS,
        "version": env!("CARGO_PKG_VERSION"),
    })))
}

async fn save_ad_settings(State(st): State<St>, Json(mut s): Json<AdSettings>) -> ApiResult {
    let old: AdSettings = st.app.db.get_setting("ad")?;
    s.last_sync = old.last_sync;
    st.app.db.set_setting("ad", &s)?;
    Ok(Json(json!({ "ok": true })))
}

async fn save_winrm_settings(State(st): State<St>, Json(s): Json<WinRmSettings>) -> ApiResult {
    st.app.db.set_setting("winrm", &s)?;
    Ok(Json(json!({ "ok": true })))
}

async fn forget_host(State(st): State<St>, Path(host): Path<String>) -> ApiResult {
    let id = if host.contains(':') { host } else { format!("{host}:22") };
    st.app.db.forget_known_host(&id.to_ascii_lowercase())?;
    Ok(Json(json!({ "ok": true })))
}

// ----------------------------------------------------------------------- AD

#[derive(Deserialize)]
struct AdPreview {
    #[serde(default)]
    settings: Option<AdSettings>,
}

async fn ad_preview(State(st): State<St>, Json(req): Json<AdPreview>) -> ApiResult {
    let s = match req.settings {
        Some(s) => s,
        None => st.app.db.get_setting("ad")?,
    };
    let list = ad::fetch(&st.app, &s).await?;
    Ok(Json(json!({ "computers": list })))
}

#[derive(Deserialize)]
struct AdImport {
    computers: Vec<AdComputer>,
}

async fn ad_import(State(st): State<St>, Json(req): Json<AdImport>) -> ApiResult {
    let mut s: AdSettings = st.app.db.get_setting("ad")?;
    let n = ad::import(&st.app, &s, &req.computers)?;
    s.last_sync = Some(crate::db::now());
    st.app.db.set_setting("ad", &s)?;
    Ok(Json(json!({ "imported": n })))
}

// -------------------------------------------------------------- deployments

#[derive(Deserialize)]
struct LimitQ {
    limit: Option<usize>,
}

async fn list_deployments(State(st): State<St>, Query(q): Query<LimitQ>) -> ApiResult {
    Ok(Json(json!(st.app.db.list_deployments(q.limit.unwrap_or(100).min(1000))?)))
}

#[derive(Deserialize)]
struct CreateDeployment {
    #[serde(default)]
    package_id: Option<String>,
    /// Ad-hoc command instead of a package.
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    shell: Option<Shell>,
    #[serde(default)]
    variables: BTreeMap<String, String>,
    targets: TargetSpec,
    #[serde(default)]
    options: DeployOptions,
    /// Local time "YYYY-MM-DD HH:MM" to run later.
    #[serde(default)]
    scheduled_for: Option<String>,
}

async fn create_deployment(State(st): State<St>, Json(req): Json<CreateDeployment>) -> ApiResult {
    let app = &st.app;
    let pkg = match (&req.package_id, &req.command) {
        (Some(id), _) => app.library()?.packages.remove(id).ok_or_else(|| anyhow!("package '{id}' not found"))?,
        (None, Some(cmd)) if !cmd.trim().is_empty() => engine::adhoc_package(cmd, req.shell)?,
        _ => return Err(anyhow!("choose a package or enter a command").into()),
    };
    let when = match req.scheduled_for.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(scheduler::parse_when(s)?.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        None => None,
    };
    let source = if req.command.is_some() && req.package_id.is_none() { "command" } else { "console" };
    let id = engine::create_deployment(app, &pkg, req.variables, req.targets, req.options, source, when.clone())?;
    if when.is_none() {
        engine::start(app, id.clone());
    }
    Ok(Json(json!({ "id": id })))
}

async fn get_deployment(State(st): State<St>, Path(id): Path<String>) -> ApiResult {
    let dep = st.app.db.get_deployment(&id)?.ok_or_else(|| anyhow!("deployment not found"))?;
    let targets = st.app.db.list_targets(&id, false)?;
    let pkg: Option<Package> = serde_yaml::from_str(&dep.package_yaml).ok();
    let secret: Vec<String> = pkg
        .map(|p| p.variables.into_iter().filter(|(_, v)| v.secret).map(|(k, _)| k).collect())
        .unwrap_or_default();
    let mut dep_json = json!(dep);
    if let Some(vars) = dep_json["variables"].as_object_mut() {
        for k in secret {
            if vars.contains_key(&k) {
                vars.insert(k, json!("********"));
            }
        }
    }
    Ok(Json(json!({ "deployment": dep_json, "targets": targets, "package_yaml": dep.package_yaml })))
}

async fn target_log(State(st): State<St>, Path(id): Path<i64>) -> Result<String, ApiError> {
    Ok(st.app.db.target_log(id)?)
}

async fn cancel_deployment(State(st): State<St>, Path(id): Path<String>) -> ApiResult {
    let dep = st.app.db.get_deployment(&id)?.ok_or_else(|| anyhow!("deployment not found"))?;
    if dep.status == "scheduled" {
        st.app.db.set_deployment_status(&id, "cancelled")?;
        return Ok(Json(json!({ "ok": true })));
    }
    if !engine::cancel(&st.app, &id) {
        return Err(anyhow!("deployment is not running").into());
    }
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct RedeployReq {
    #[serde(default)]
    only_failed: bool,
}

async fn redeploy(State(st): State<St>, Path(id): Path<String>, Json(r): Json<RedeployReq>) -> ApiResult {
    let new_id = engine::redeploy(&st.app, &id, r.only_failed)?;
    engine::start(&st.app, new_id.clone());
    Ok(Json(json!({ "id": new_id })))
}

async fn delete_deployment(State(st): State<St>, Path(id): Path<String>) -> ApiResult {
    let dep = st.app.db.get_deployment(&id)?.ok_or_else(|| anyhow!("deployment not found"))?;
    if dep.status == "running" {
        return Err(anyhow!("cancel the deployment before deleting it").into());
    }
    st.app.db.delete_deployment(&id)?;
    Ok(Json(json!({ "ok": true })))
}

// ---------------------------------------------------------------- schedules

async fn list_schedules(State(st): State<St>) -> ApiResult {
    Ok(Json(json!(st.app.db.list_schedules()?)))
}

async fn save_schedule(State(st): State<St>, Json(mut s): Json<Schedule>) -> ApiResult {
    if s.name.trim().is_empty() {
        return Err(anyhow!("schedule name is required").into());
    }
    let lib = st.app.library()?;
    let pkg = lib.packages.get(&s.package_id).ok_or_else(|| anyhow!("package '{}' not found", s.package_id))?;
    pkg.resolve_vars(&s.variables)?;
    if s.targets.computers.is_empty() && s.targets.tags.is_empty() && s.targets.groups.is_empty() {
        return Err(anyhow!("choose at least one computer or tag").into());
    }
    s.next_run = scheduler::next_run(&s, chrono::Local::now())?;
    if s.next_run.is_none() {
        return Err(anyhow!("that time is in the past").into());
    }
    let id = st.app.db.save_schedule(&s)?;
    Ok(Json(json!({ "id": id, "next_run": s.next_run })))
}

async fn delete_schedule(State(st): State<St>, Path(id): Path<i64>) -> ApiResult {
    st.app.db.delete_schedule(id)?;
    Ok(Json(json!({ "ok": true })))
}
