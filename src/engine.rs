//! Deployment engine: resolves targets, runs package steps on each target in
//! parallel (optionally in rolling batches) and records results.

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tracing::{error, info};

use crate::app::{App, WinRmSettings};
use crate::db::{DeployOptions, NewDeployment, TargetSpec, not_found};
use crate::package::{Install, Os, Package, ServiceAction, Shell, Step, render};
use crate::transport::ssh::Ssh;
use crate::transport::winrm::WinRm;
use crate::transport::{
    LogFn, Transport, default_port, detect_os, port_open, ps_quote, sh_quote, wrap_script,
};

/// A computer resolved for a deployment.
#[derive(Debug, Clone)]
pub struct Target {
    pub name: String,
    pub host: String,
    pub os: Option<Os>,
    pub credential_id: Option<i64>,
    pub port: Option<u16>,
    pub in_db: bool,
}

/// Expand tags and names into a de-duplicated target list. Names that are
/// not in the computer list are used as-is (OS detected by port probe).
pub fn resolve_targets(app: &App, spec: &TargetSpec) -> Result<Vec<Target>> {
    let all = app.db.list_computers()?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut push = |t: Target| {
        if seen.insert(t.name.to_ascii_lowercase()) {
            out.push(t);
        }
    };
    let from_db = |c: &crate::db::Computer| Target {
        name: c.name.clone(),
        host: c.connect_host().to_string(),
        os: Os::parse(&c.os),
        credential_id: c.credential_id,
        port: c.port,
        in_db: true,
    };
    if !spec.groups.is_empty() {
        let groups = app.db.list_groups()?;
        let wanted: Vec<&crate::db::Group> = spec
            .groups
            .iter()
            .map(|name| {
                groups
                    .iter()
                    .find(|g| g.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| anyhow!("computer group '{name}' not found"))
            })
            .collect::<Result<_>>()?;
        let inv = crate::groups::load_inventory(&app.db, wanted.iter().map(|g| &g.rules))?;
        for g in wanted {
            for c in crate::groups::members(g, &all, &inv) {
                push(from_db(c));
            }
        }
    }
    for tag in &spec.tags {
        for c in &all {
            if tag == "*" || c.tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                push(from_db(c));
            }
        }
    }
    for name in spec.computers.iter().map(|n| n.trim()).filter(|n| !n.is_empty()) {
        match all.iter().find(|c| c.name.eq_ignore_ascii_case(name)) {
            Some(c) => push(from_db(c)),
            None => push(Target {
                name: name.to_string(),
                host: name.to_string(),
                os: None,
                credential_id: None,
                port: None,
                in_db: false,
            }),
        }
    }
    Ok(out)
}

/// Record a deployment. It is started by the caller (`start`) or by the
/// scheduler when `scheduled_for` is set.
pub fn create_deployment(
    app: &App,
    pkg: &Package,
    variables: BTreeMap<String, String>,
    targets: TargetSpec,
    options: DeployOptions,
    source: &str,
    scheduled_for: Option<String>,
) -> Result<String> {
    pkg.validate()?;
    pkg.resolve_vars(&variables)?;
    if targets.computers.iter().all(|c| c.trim().is_empty()) && targets.tags.is_empty() && targets.groups.is_empty() {
        bail!("choose at least one computer, group or tag");
    }
    if resolve_targets(app, &targets)?.is_empty() {
        bail!("no computers match the selected groups/tags");
    }
    let id = uuid::Uuid::new_v4().to_string();
    app.db.create_deployment(&NewDeployment {
        id: id.clone(),
        package_id: pkg.id.clone(),
        package_name: pkg.name.clone(),
        package_yaml: serde_yaml::to_string(pkg)?,
        variables,
        targets,
        options,
        source: source.to_string(),
        scheduled_for,
    })?;
    Ok(id)
}

/// Run a deployment in the background.
pub fn start(app: &App, id: String) {
    let app = app.clone();
    tokio::spawn(async move {
        if let Err(e) = run_deployment(&app, &id).await {
            error!("deployment {id} failed: {e:#}");
            let _ = app.db.set_deployment_status(&id, "failed");
        }
    });
}

pub fn cancel(app: &App, id: &str) -> bool {
    match app.cancels.lock().unwrap().get(id) {
        Some(flag) => {
            flag.store(true, Ordering::SeqCst);
            true
        }
        None => false,
    }
}

/// A one-step package for "run this command on these computers".
pub fn adhoc_package(command: &str, shell: Option<Shell>) -> Result<Package> {
    let first = command.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let short: String = first.chars().take(60).collect();
    let pkg: Package = serde_json::from_value(serde_json::json!({
        "id": "adhoc",
        "name": format!("Command: {short}"),
        "category": "Ad-hoc",
        "platforms": shell.map(|s| vec![s.os()]).unwrap_or_default(),
        "steps": [{ "name": "Run command", "script": command, "shell": shell }],
    }))?;
    pkg.validate()?;
    Ok(pkg)
}

/// Create a new deployment from an old one (all targets or only failed ones).
pub fn redeploy(app: &App, id: &str, only_failed: bool) -> Result<String> {
    let dep = app.db.get_deployment(id)?.ok_or_else(|| not_found("deployment"))?;
    let pkg: Package = serde_yaml::from_str(&dep.package_yaml)?;
    let targets = if only_failed {
        let failed: Vec<String> = app
            .db
            .list_targets(id, false)?
            .into_iter()
            .filter(|t| matches!(t.status.as_str(), "failed" | "offline" | "cancelled"))
            .map(|t| t.computer)
            .collect();
        if failed.is_empty() {
            bail!("no failed targets to redeploy");
        }
        TargetSpec { computers: failed, ..Default::default() }
    } else {
        dep.targets.clone()
    };
    create_deployment(app, &pkg, dep.variables, targets, dep.options, "redeploy", None)
}

struct Run {
    app: App,
    dep_id: String,
    pkg: Package,
    library: HashMap<String, Package>,
    vars: BTreeMap<String, String>,
    secrets: Vec<String>,
    options: DeployOptions,
    cancel: Arc<AtomicBool>,
    failures: AtomicUsize,
    winrm: WinRmSettings,
}

enum Outcome {
    Success,
    Skipped(String),
    Offline(String),
    Cancelled,
}

/// Execute a deployment to completion and return its final status.
pub async fn run_deployment(app: &App, id: &str) -> Result<String> {
    let dep = app.db.get_deployment(id)?.ok_or_else(|| not_found("deployment"))?;
    if !matches!(dep.status.as_str(), "queued" | "scheduled") {
        bail!("deployment {id} already ran ({})", dep.status);
    }
    app.db.set_deployment_status(id, "running")?;
    let pkg: Package = serde_yaml::from_str(&dep.package_yaml)?;
    let vars = pkg.resolve_vars(&dep.variables)?;
    let targets = resolve_targets(app, &dep.targets)?;
    info!("deployment {id}: '{}' to {} computer(s)", pkg.name, targets.len());

    let mut rows = Vec::new();
    for t in targets {
        let os = t.os.map(|o| o.as_str()).unwrap_or("unknown");
        rows.push((app.db.add_target(id, &t.name, os)?, t));
    }
    let cancel = Arc::new(AtomicBool::new(false));
    app.cancels.lock().unwrap().insert(id.to_string(), cancel.clone());

    let run = Arc::new(Run {
        app: app.clone(),
        dep_id: id.to_string(),
        secrets: pkg.secret_values(&vars),
        library: app.library()?.packages,
        pkg,
        vars,
        options: dep.options.clone(),
        cancel: cancel.clone(),
        failures: AtomicUsize::new(0),
        winrm: app.winrm_settings(),
    });
    let batch = match dep.options.batch_size {
        0 => rows.len().max(1),
        n => n,
    };
    let concurrency = dep.options.concurrency.clamp(1, 256);
    for (i, chunk) in rows.chunks(batch).enumerate() {
        if dep.options.batch_size > 0 {
            info!("deployment {id}: batch {} ({} computers)", i + 1, chunk.len());
        }
        futures::stream::iter(chunk.iter().cloned())
            .for_each_concurrent(concurrency, |(row, t)| {
                let run = run.clone();
                async move { run_target(&run, row, t).await }
            })
            .await;
    }
    app.cancels.lock().unwrap().remove(id);

    let results = app.db.list_targets(id, false)?;
    let bad = results
        .iter()
        .filter(|t| matches!(t.status.as_str(), "failed" | "offline" | "cancelled"))
        .count();
    let status = if cancel.load(Ordering::SeqCst) {
        "cancelled"
    } else if bad == 0 {
        "success"
    } else if bad == results.len() {
        "failed"
    } else {
        "partial"
    };
    app.db.set_deployment_status(id, status)?;
    info!("deployment {id}: {status}");
    let succeeded = results.into_iter().filter(|t| t.status == "success").map(|t| t.computer).collect();
    crate::inventory::after_deployment(app, succeeded);
    Ok(status.to_string())
}

async fn run_target(run: &Run, row: i64, t: Target) {
    let db = &run.app.db;
    if run.cancel.load(Ordering::SeqCst) {
        let _ = db.update_target(row, "cancelled", None, Some("deployment cancelled"));
        return;
    }
    if let Some(max) = run.options.max_failures
        && run.failures.load(Ordering::SeqCst) > max {
            let _ = db.update_target(row, "cancelled", None, Some("not started: failure limit reached"));
            return;
        }
    let log = make_logger(run, row);
    let result = run_target_inner(run, row, &t, &log).await;
    let (status, msg) = match result {
        Ok(Outcome::Success) => ("success", None),
        Ok(Outcome::Skipped(m)) => ("skipped", Some(m)),
        Ok(Outcome::Offline(m)) => ("offline", Some(m)),
        Ok(Outcome::Cancelled) => ("cancelled", Some("deployment cancelled".to_string())),
        Err(e) => ("failed", Some(format!("{e:#}"))),
    };
    if let Some(m) = &msg {
        log(&format!("==> {}: {m}", status.to_uppercase()));
    } else {
        log("==> SUCCESS");
    }
    if matches!(status, "failed" | "offline") {
        run.failures.fetch_add(1, Ordering::SeqCst);
    }
    let _ = db.update_target(row, status, None, msg.as_deref());
}

fn make_logger(run: &Run, row: i64) -> LogFn {
    const MAX_LINES: usize = 20_000;
    let db = run.app.db.clone();
    let secrets = run.secrets.clone();
    let count = AtomicUsize::new(0);
    Arc::new(move |line: &str| {
        let n = count.fetch_add(1, Ordering::Relaxed);
        if n > MAX_LINES {
            return;
        }
        let mut l = if n == MAX_LINES { "[log truncated]".to_string() } else { line.to_string() };
        for s in &secrets {
            l = l.replace(s.as_str(), "********");
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        let _ = db.append_log(row, &format!("{ts} {l}\n"));
    })
}

/// What a step needs to talk to its target.
struct StepCx<'a> {
    run: &'a Run,
    transport: &'a dyn Transport,
    os: Os,
    host: &'a str,
    port: u16,
    vars: &'a BTreeMap<String, String>,
    workdir: &'a str,
    used_workdir: AtomicBool,
    log: &'a LogFn,
}

impl StepCx<'_> {
    async fn script(&self, shell: Shell, body: &str, timeout: Duration) -> Result<i32> {
        self.transport.run(&wrap_script(shell, body), shell, timeout, self.log).await
    }

    fn default_shell(&self) -> Shell {
        Shell::default_for(self.os)
    }

    fn remote_path(&self, name: &str) -> String {
        match self.os {
            Os::Windows => format!(r"{}\{name}", self.workdir),
            Os::Linux => format!("{}/{name}", self.workdir),
        }
    }

    async fn push(&self, rel: &str, timeout: Duration) -> Result<String> {
        let local = self.run.app.repo_file(rel)?;
        let name = local.file_name().unwrap().to_string_lossy().to_string();
        let remote = self.remote_path(&name);
        let size = std::fs::metadata(&local)?.len();
        (self.log)(&format!("    uploading {name} ({:.1} MB)", size as f64 / 1_048_576.0));
        self.used_workdir.store(true, Ordering::SeqCst);
        self.transport.upload(&local, &remote, timeout, self.log).await?;
        Ok(remote)
    }
}

pub fn build_transport(
    app: &App,
    t: &Target,
    os: Os,
    port: u16,
    winrm: &WinRmSettings,
) -> Result<Box<dyn Transport>> {
    let cred = match t.credential_id {
        Some(id) => app.db.get_credential(id)?,
        None => None,
    };
    let cred = match cred {
        Some(c) => Some(c),
        None => app.db.default_credential(os.as_str())?,
    };
    match os {
        Os::Windows => {
            let (user, password) = match &cred {
                Some(c) if !c.username.is_empty() => {
                    (Some(c.username.clone()), app.db.credential_secret(c.id)?)
                }
                _ => (None, None),
            };
            Ok(Box::new(WinRm {
                host: t.host.clone(),
                port: Some(port),
                ssl: winrm.use_ssl,
                skip_ca_check: winrm.skip_ca_check,
                user,
                password,
            }))
        }
        Os::Linux => {
            let c = cred.ok_or_else(|| {
                anyhow!(
                    "no SSH credential for {}: add one in Settings → Credentials (default for Linux) or assign one to the computer",
                    t.name
                )
            })?;
            let secret = app.db.credential_secret(c.id)?;
            Ok(Box::new(Ssh::new(t.host.clone(), port, c.username, secret, c.key_path, app.db.clone())))
        }
    }
}

async fn run_target_inner(run: &Run, row: i64, t: &Target, log: &LogFn) -> Result<Outcome> {
    let app = &run.app;
    app.db.update_target(row, "running", Some("connecting"), None)?;
    let os = match t.os {
        Some(os) => os,
        None => {
            log("OS unknown, probing WinRM/SSH ports");
            match detect_os(&t.host, run.winrm.use_ssl).await {
                Some(os) => {
                    log(&format!("detected {}", os.as_str()));
                    if t.in_db {
                        app.db.set_computer_os(&t.name, os.as_str())?;
                    }
                    os
                }
                None => return Ok(Outcome::Offline("no WinRM or SSH port answering".into())),
            }
        }
    };
    if !run.pkg.supports(os) {
        return Ok(Outcome::Skipped(format!("package does not support {}", os.as_str())));
    }
    let port = t.port.unwrap_or(default_port(os, run.winrm.use_ssl));
    if !port_open(&t.host, port, Duration::from_secs(5)).await {
        return Ok(Outcome::Offline(format!("{}:{port} not reachable", t.host)));
    }
    let transport = build_transport(app, t, os, port, &run.winrm)?;

    let short = &run.dep_id[..8];
    let workdir = match os {
        Os::Windows => format!(r"C:\Windows\Temp\psccm\{short}"),
        Os::Linux => format!("/tmp/psccm-{short}"),
    };
    let mut vars = run.vars.clone();
    vars.insert("host".into(), t.name.clone());
    let short = if t.name.parse::<std::net::IpAddr>().is_ok() { &t.name } else { t.name.split('.').next().unwrap_or(&t.name) };
    vars.insert("host_short".into(), short.to_string());
    vars.insert("os".into(), os.as_str().into());
    vars.insert("workdir".into(), workdir.clone());
    vars.insert("deployment_id".into(), run.dep_id.clone());
    let steps = run.pkg.plan(os, &run.library, &mut vars)?;
    // Values may refer to built-ins, e.g. server_name: "{{host_short}}".
    let snapshot = vars.clone();
    for v in vars.values_mut() {
        if v.contains("{{") {
            *v = render(v, &snapshot)?;
        }
    }

    let cx = StepCx {
        run,
        transport: transport.as_ref(),
        os,
        host: &t.host,
        port,
        vars: &vars,
        workdir: &workdir,
        used_workdir: AtomicBool::new(false),
        log,
    };
    let result = run_steps(&cx, row, &steps).await;
    if cx.used_workdir.load(Ordering::SeqCst) {
        let cleanup = match os {
            Os::Windows => format!("Remove-Item -Recurse -Force -LiteralPath {} -ErrorAction SilentlyContinue", ps_quote(&workdir)),
            Os::Linux => format!("rm -rf {}", sh_quote(&workdir)),
        };
        let quiet: LogFn = Arc::new(|_| {});
        let _ = transport.run(&wrap_script(cx.default_shell(), &cleanup), cx.default_shell(), Duration::from_secs(120), &quiet).await;
    }
    result
}

async fn run_steps(cx: &StepCx<'_>, row: i64, steps: &[Step]) -> Result<Outcome> {
    let run = cx.run;
    let db = &run.app.db;
    let log = cx.log;

    if run.options.use_detect
        && let Some(detect) = run.pkg.detect.for_os(cx.os) {
            db.update_target(row, "running", Some("detect"), None)?;
            log("==> Detect: is it already installed?");
            let code = cx
                .script(cx.default_shell(), &render(detect, cx.vars)?, Duration::from_secs(300))
                .await
                .context("detect script")?;
            if code == 0 {
                return Ok(Outcome::Skipped("already installed / up to date (detect passed)".into()));
            }
            log(&format!("    not installed or outdated (detect exit {code})"));
        }

    let mut delegates = HashMap::new();
    let total = steps.len();
    for (i, step) in steps.iter().enumerate() {
        if run.cancel.load(Ordering::SeqCst) {
            return Ok(Outcome::Cancelled);
        }
        let label = format!("{}/{total} {}", i + 1, render(&step.name, cx.vars)?);
        db.update_target(row, "running", Some(&label), None)?;
        log(&format!("==> Step {label}"));
        let mut attempt = 0;
        loop {
            let started = Instant::now();
            match exec_step(cx, step, &mut delegates).await {
                Ok(()) => {
                    log(&format!("    ok ({:.1}s)", started.elapsed().as_secs_f64()));
                    break;
                }
                Err(e) if attempt < step.retries => {
                    attempt += 1;
                    log(&format!(
                        "    failed: {e:#} - retry {attempt}/{} in {}s",
                        step.retries, step.retry_delay
                    ));
                    tokio::time::sleep(Duration::from_secs(step.retry_delay)).await;
                }
                Err(e) if step.continue_on_error => {
                    log(&format!("    failed: {e:#} (continue_on_error set, carrying on)"));
                    break;
                }
                Err(e) => {
                    rollback(cx, &mut delegates).await;
                    return Err(e.context(format!("step {label}")));
                }
            }
        }
    }

    if let Some(verify) = run.pkg.verify.for_os(cx.os) {
        db.update_target(row, "running", Some("verify"), None)?;
        log("==> Verify");
        let code = cx.script(cx.default_shell(), &render(verify, cx.vars)?, Duration::from_secs(300)).await?;
        if code != 0 {
            rollback(cx, &mut delegates).await;
            bail!("verify failed (exit {code})");
        }
    }
    Ok(Outcome::Success)
}

/// Connection to a `delegate_to` host, opened on first use.
struct Delegate {
    transport: Box<dyn Transport>,
    os: Os,
    host: String,
    port: u16,
    workdir: String,
}

async fn open_delegate(run: &Run, name: &str) -> Result<Delegate> {
    let spec = TargetSpec { computers: vec![name.to_string()], ..Default::default() };
    let t = resolve_targets(&run.app, &spec)?.pop().ok_or_else(|| anyhow!("delegate host is empty"))?;
    let os = match t.os {
        Some(os) => os,
        None => detect_os(&t.host, run.winrm.use_ssl)
            .await
            .ok_or_else(|| anyhow!("delegate host {name} is not reachable"))?,
    };
    let port = t.port.unwrap_or(default_port(os, run.winrm.use_ssl));
    if !port_open(&t.host, port, Duration::from_secs(5)).await {
        bail!("delegate host {}:{port} is not reachable", t.host);
    }
    let short = &run.dep_id[..8];
    Ok(Delegate {
        transport: build_transport(&run.app, &t, os, port, &run.winrm)?,
        os,
        workdir: match os {
            Os::Windows => format!(r"C:\Windows\Temp\psccm\{short}"),
            Os::Linux => format!("/tmp/psccm-{short}"),
        },
        host: t.host,
        port,
    })
}

/// Run a step on the target, or on its `delegate_to` host.
async fn exec_step(cx: &StepCx<'_>, step: &Step, delegates: &mut HashMap<String, Delegate>) -> Result<()> {
    let Some(d) = &step.delegate_to else {
        return run_step(cx, step).await;
    };
    let name = render(d, cx.vars)?;
    if !delegates.contains_key(&name) {
        let opened = open_delegate(cx.run, &name).await?;
        delegates.insert(name.clone(), opened);
    }
    let d = &delegates[&name];
    if !step.runs_on(d.os) {
        (cx.log)(&format!("    skipped: step is for {:?}, {name} is {}", step.os, d.os.as_str()));
        return Ok(());
    }
    (cx.log)(&format!("    running on {name}"));
    let dcx = StepCx {
        run: cx.run,
        transport: d.transport.as_ref(),
        os: d.os,
        host: &d.host,
        port: d.port,
        vars: cx.vars,
        workdir: &d.workdir,
        used_workdir: AtomicBool::new(false),
        log: cx.log,
    };
    run_step(&dcx, step).await
}

async fn rollback(cx: &StepCx<'_>, delegates: &mut HashMap<String, Delegate>) {
    let steps: Vec<&Step> = cx
        .run
        .pkg
        .rollback
        .iter()
        .filter(|s| s.delegate_to.is_some() || s.runs_on(cx.os))
        .collect();
    if steps.is_empty() {
        return;
    }
    (cx.log)("==> Rolling back");
    for step in steps {
        (cx.log)(&format!("==> Rollback: {}", render(&step.name, cx.vars).unwrap_or_default()));
        if let Err(e) = exec_step(cx, step, delegates).await {
            (cx.log)(&format!("    rollback step failed: {e:#}"));
        }
    }
}

async fn run_step(cx: &StepCx<'_>, step: &Step) -> Result<()> {
    let vars = cx.vars;
    let timeout = Duration::from_secs(step.timeout.unwrap_or(if step.reboot { 900 } else { 1800 }));
    if let Some(cond) = &step.when {
        let code = cx.script(cx.default_shell(), &render(cond, vars)?, Duration::from_secs(300)).await?;
        if code != 0 {
            (cx.log)(&format!("    condition not met (exit {code}), step skipped"));
            return Ok(());
        }
    }
    let codes = step.success_codes();
    let check = |code: i32| -> Result<()> {
        if !codes.contains(&code) {
            bail!("exit code {code} (success codes: {codes:?})");
        }
        if code == 3010 || code == 1641 {
            (cx.log)("    note: installer reports a reboot is required");
        }
        Ok(())
    };

    if let Some(script) = &step.script {
        if script.contains("workdir") {
            cx.used_workdir.store(true, Ordering::SeqCst);
        }
        let shell = step.shell.unwrap_or(cx.default_shell());
        return check(cx.script(shell, &render(script, vars)?, timeout).await?);
    }
    if let Some(inst) = &step.install {
        let body = install_script(cx, inst, timeout).await?;
        return check(cx.script(cx.default_shell(), &body, timeout).await?);
    }
    if let Some(copy) = &step.copy {
        let tmp = cx.push(&render(&copy.src, vars)?, timeout).await?;
        let dest = render(&copy.dest, vars)?;
        let body = match cx.os {
            Os::Windows => format!(
                "$d = {d}\nNew-Item -ItemType Directory -Force -Path (Split-Path $d) | Out-Null\nCopy-Item -LiteralPath {t} -Destination $d -Force\nWrite-Output \"Copied to $d\"",
                d = ps_quote(&dest),
                t = ps_quote(&tmp)
            ),
            Os::Linux => format!(
                "d={d}\nif [ -e \"$d\" ]; then cp -f {t} \"$d\"; else install -D -m 0644 {t} \"$d\"; fi\necho \"Copied to $d\"",
                d = sh_quote(&dest),
                t = sh_quote(&tmp)
            ),
        };
        return check(cx.script(cx.default_shell(), &body, timeout).await?);
    }
    if let Some(svc) = &step.service {
        let name = render(&svc.name, vars)?;
        let body = match (cx.os, svc.action) {
            (Os::Windows, a) => {
                let cmd = match a {
                    ServiceAction::Start => "Start-Service -Name $n",
                    ServiceAction::Stop => "Stop-Service -Name $n -Force",
                    ServiceAction::Restart => "Restart-Service -Name $n -Force",
                };
                format!("$n = {}\n{cmd}\nWrite-Output \"$n is $((Get-Service -Name $n).Status)\"", ps_quote(&name))
            }
            (Os::Linux, a) => {
                let verb = match a {
                    ServiceAction::Start => "start",
                    ServiceAction::Stop => "stop",
                    ServiceAction::Restart => "restart",
                };
                let n = sh_quote(&name);
                format!("systemctl {verb} {n}\necho \"{name}: $(systemctl is-active {n} || true)\"")
            }
        };
        return check(cx.script(cx.default_shell(), &body, timeout).await?);
    }
    if step.reboot {
        return reboot(cx, timeout).await;
    }
    if let Some(secs) = step.wait {
        (cx.log)(&format!("    waiting {secs}s"));
        tokio::time::sleep(Duration::from_secs(secs)).await;
        return Ok(());
    }
    bail!("step '{}' has no runnable action", step.name)
}

/// Build the install script; pushes the installer first when it comes from
/// the repository.
async fn install_script(cx: &StepCx<'_>, inst: &Install, timeout: Duration) -> Result<String> {
    let vars = cx.vars;
    let args = render(&inst.args, vars)?;
    let (path, url) = match (&inst.file, &inst.url) {
        (Some(file), _) => (cx.push(&render(file, vars)?, timeout).await?, None),
        (None, Some(url)) => {
            let url = render(url, vars)?;
            let name = file_name_from_url(&url);
            cx.used_workdir.store(true, Ordering::SeqCst);
            (cx.remote_path(&name), Some(url))
        }
        (None, None) => bail!("install needs 'file' or 'url'"),
    };
    let sha = match &inst.sha256 {
        Some(s) => Some(render(s, vars)?),
        None => None,
    };
    Ok(match cx.os {
        Os::Windows => {
            let mut s = format!("$f = {}\n", ps_quote(&path));
            if let Some(u) = &url {
                s += &format!("Get-PsccmFile -Url {} -OutFile $f\n", ps_quote(u));
            }
            if let Some(h) = &sha {
                s += &format!(
                    "$h = (Get-FileHash -Algorithm SHA256 -LiteralPath $f).Hash\nif ($h -ne {}) {{ Write-Output \"SHA256 mismatch: got $h\"; exit 1 }}\n",
                    ps_quote(&h.to_uppercase())
                );
            }
            s += &format!("$a = {}\n", ps_quote(&args));
            s += WIN_INSTALL;
            s
        }
        Os::Linux => {
            let mut s = format!("f={}\n", sh_quote(&path));
            if let Some(u) = &url {
                s += &format!("download {} \"$f\"\n", sh_quote(u));
            }
            if let Some(h) = &sha {
                s += &format!("echo \"{}  $f\" | sha256sum -c -\n", h.to_lowercase());
            }
            s += &LINUX_INSTALL.replace("@ARGS@", &args);
            s
        }
    })
}

const WIN_INSTALL: &str = r#"switch ([IO.Path]::GetExtension($f).ToLower()) {
    '.msi' { $code = Invoke-PsccmProcess -FilePath 'msiexec.exe' -Arguments "/i `"$f`" /qn /norestart /l*v `"$f.log`" $a" }
    '.msp' { $code = Invoke-PsccmProcess -FilePath 'msiexec.exe' -Arguments "/p `"$f`" /qn /norestart /l*v `"$f.log`" $a" }
    '.msu' { $code = Invoke-PsccmProcess -FilePath 'wusa.exe' -Arguments "`"$f`" /quiet /norestart $a" }
    default { $code = Invoke-PsccmProcess -FilePath $f -Arguments $a }
}
Write-Output "Installer exit code: $code"
if ($code -notin 0, 3010, 1641 -and (Test-Path "$f.log")) {
    Write-Output '--- end of MSI log ---'
    Get-Content "$f.log" -Tail 25
}
exit $code
"#;

const LINUX_INSTALL: &str = r#"case "$f" in
    *.deb) apt-get install -y @ARGS@ "$f" ;;
    *.rpm) if command -v dnf >/dev/null 2>&1; then dnf install -y @ARGS@ "$f"; else yum install -y @ARGS@ "$f"; fi ;;
    *.sh|*.run) bash "$f" @ARGS@ ;;
    *) echo "Cannot install $f automatically; use a script step" >&2; exit 1 ;;
esac
"#;

fn file_name_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or("");
    let clean: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    if clean.is_empty() { "installer.bin".into() } else { clean }
}

async fn reboot(cx: &StepCx<'_>, timeout: Duration) -> Result<()> {
    let log = cx.log;
    let shell = cx.default_shell();
    let cmd = match cx.os {
        Os::Windows => "shutdown.exe /r /t 5 /c \"PSCCM deployment reboot\" /d p:0:0",
        Os::Linux => "nohup sh -c 'sleep 3; systemctl reboot || /sbin/reboot' >/dev/null 2>&1 &",
    };
    log("    reboot requested");
    // The connection may drop while the command returns; that is expected.
    let _ = cx.script(shell, cmd, Duration::from_secs(120)).await;
    cx.transport.reset().await;

    let deadline = Instant::now() + timeout;
    let probe = Duration::from_secs(3);
    log("    waiting for the computer to go down");
    while port_open(cx.host, cx.port, probe).await {
        if Instant::now() > deadline {
            bail!("computer did not restart within {}s", timeout.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    log("    down; waiting for it to come back");
    let quiet: LogFn = Arc::new(|_| {});
    loop {
        if Instant::now() > deadline {
            bail!("computer did not come back within {}s", timeout.as_secs());
        }
        tokio::time::sleep(Duration::from_secs(10)).await;
        if port_open(cx.host, cx.port, probe).await {
            let alive = cx.transport.run(&wrap_script(shell, "exit 0"), shell, Duration::from_secs(60), &quiet).await;
            if matches!(alive, Ok(0)) {
                log("    back online");
                return Ok(());
            }
            cx.transport.reset().await;
        }
    }
}

/// Probe each computer's management port and record online/offline. Unknown
/// OSes are detected on the way.
pub async fn check_online(app: &App, names: Option<Vec<String>>) -> Result<Vec<(String, bool)>> {
    let winrm = app.winrm_settings();
    let mut computers = app.db.list_computers()?;
    if let Some(names) = names {
        let set: HashSet<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
        computers.retain(|c| set.contains(&c.name.to_ascii_lowercase()));
    }
    let results: Vec<(String, bool)> = futures::stream::iter(computers)
        .map(|c| {
            let app = app.clone();
            let ssl = winrm.use_ssl;
            async move {
                let host = c.connect_host().to_string();
                let online = match Os::parse(&c.os) {
                    Some(os) => port_open(&host, c.port.unwrap_or(default_port(os, ssl)), Duration::from_secs(3)).await,
                    None => match detect_os(&host, ssl).await {
                        Some(os) => {
                            let _ = app.db.set_computer_os(&c.name, os.as_str());
                            true
                        }
                        None => false,
                    },
                };
                let _ = app.db.set_online(&c.name, online);
                (c.name, online)
            }
        })
        .buffer_unordered(64)
        .collect()
        .await;
    Ok(results)
}

/// All files in the repository folder, relative paths with sizes.
pub fn list_repository(app: &App) -> Vec<(String, u64)> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, u64)>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else if let Ok(m) = e.metadata() {
                let rel = p.strip_prefix(base).unwrap().to_string_lossy().replace('\\', "/");
                out.push((rel, m.len()));
            }
        }
    }
    let mut out = Vec::new();
    walk(&app.repo_dir(), &app.repo_dir(), &mut out);
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_file_names() {
        assert_eq!(file_name_from_url("https://x/a/7z2409-x64.msi?dl=1"), "7z2409-x64.msi");
        assert_eq!(file_name_from_url("https://x/"), "installer.bin");
    }
}
