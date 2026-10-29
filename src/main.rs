//! PSCCM - lightweight, agentless deployment manager for Windows and Linux.

mod ad;
mod agent;
mod app;
mod catalog;
mod db;
mod engine;
mod gpo;
mod groups;
mod inventory;
mod package;
mod scheduler;
mod secrets;
mod service;
mod transport;
mod update;
mod web;
mod wu;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
use std::path::PathBuf;


use crate::app::{App, Ctx};
use crate::db::{DeployOptions, NewComputer, NewCredential, TargetSpec};
use crate::package::{Package, Shell};


#[derive(Parser)]
#[command(name = "psccm", version, about = "Agentless deployment manager for Windows (WinRM) and Linux (SSH)")]
struct Cli {
    /// Data folder (database, packages, repository). Default: "data" next to the executable.
    #[arg(long, env = "PSCCM_DATA", global = true)]
    data_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the web console (default when no command is given).
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Required when binding to a non-loopback address.
        #[arg(long, env = "PSCCM_TOKEN")]
        token: Option<String>,
        /// Do not open the browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Deploy a package and wait for the result (exit code 1 if any target failed).
    Deploy {
        package: String,
        /// Computer names, comma separated.
        #[arg(short, long, value_delimiter = ',')]
        targets: Vec<String>,
        /// Computer tags, comma separated ("*" = all computers).
        #[arg(long, value_delimiter = ',')]
        tag: Vec<String>,
        /// Computer groups, comma separated.
        #[arg(short, long, value_delimiter = ',')]
        group: Vec<String>,
        /// Package variables: -v version=22.11.0
        #[arg(short = 'v', long = "var")]
        vars: Vec<String>,
        #[arg(long, default_value_t = 10)]
        concurrency: usize,
        /// Rolling deployment: computers per batch.
        #[arg(long, default_value_t = 0)]
        batch_size: usize,
        /// Stop starting new computers after this many failures.
        #[arg(long)]
        max_failures: Option<usize>,
        /// Ignore the package's detect check and always run the steps.
        #[arg(long)]
        force: bool,
    },
    /// Run an ad-hoc command on computers.
    Run {
        #[arg(required = true, value_delimiter = ',')]
        targets: Vec<String>,
        #[arg(short, long)]
        script: String,
        /// powershell, cmd, bash or sh (default: by OS)
        #[arg(long)]
        shell: Option<String>,
    },
    /// Manage packages.
    Packages {
        #[command(subcommand)]
        cmd: PackagesCmd,
    },
    /// Manage the computer list.
    Computers {
        #[command(subcommand)]
        cmd: ComputersCmd,
    },
    /// Active Directory (read-only).
    Ad {
        #[command(subcommand)]
        cmd: AdCmd,
    },
    /// Manage stored credentials.
    Cred {
        #[command(subcommand)]
        cmd: CredCmd,
    },
    /// Computer groups.
    Groups {
        #[command(subcommand)]
        cmd: GroupsCmd,
    },
    /// Group Policy objects from AD (read-only import).
    Gpo {
        #[command(subcommand)]
        cmd: GpoCmd,
    },
    /// App catalog: common apps (7-Zip, Chrome, Node.js...) at their latest version.
    Catalog {
        #[command(subcommand)]
        cmd: CatalogCmd,
    },
    /// Check for (and optionally install) a newer PSCCM release.
    Update {
        #[arg(long)]
        install: bool,
    },
    /// Windows service management.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    /// Run or manage the PSCCM agent on this computer.
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Inventory: hardware, OS and installed software collected from the computers.
    Inventory {
        #[command(subcommand)]
        cmd: InventoryCmd,
    },
    /// Windows Update: missing updates on the Windows computers.
    Updates {
        #[command(subcommand)]
        cmd: UpdatesCmd,
    },
    /// Recent deployments.
    History {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Subcommand)]
enum GroupsCmd {
    /// List groups and their members.
    List,
    /// Create a static group with the given computers.
    Create {
        name: String,
        #[arg(long, value_delimiter = ',')]
        computers: Vec<String>,
    },
}

#[derive(Subcommand)]
enum InventoryCmd {
    /// Scan computers now (all when no names are given) and wait for the result.
    Scan { names: Vec<String> },
    /// Show what is known about a computer.
    Show { name: String },
    /// Installed software across all computers, optionally filtered by name (* and ?).
    Software {
        #[arg(long)]
        name: Option<String>,
    },
    /// Software installed, removed or updated and OS/hardware changes (of one computer or all).
    Changes {
        computer: Option<String>,
        /// Only the last N days.
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// App catalog programs that are older than the latest version, with the computers that have them.
    Outdated,
}

#[derive(Subcommand)]
enum UpdatesCmd {
    /// Scan Windows computers for missing updates now (all when no names are given) and wait.
    Scan { names: Vec<String> },
    /// Missing updates and the computers that need them.
    List {
        /// Only security and critical updates.
        #[arg(long)]
        security: bool,
    },
    /// Missing updates and recent installs of one computer.
    Show { name: String },
}

#[derive(Subcommand)]
enum GpoCmd {
    /// List GPOs in the domain.
    List,
    /// Import a GPO (by GUID) and create its package.
    Import { guid: String },
    /// Import a GPO backup / GPO folder from disk (no AD needed).
    ImportFolder {
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand)]
enum CatalogCmd {
    /// Show the catalog with the latest and downloaded versions.
    List,
    /// Look up the latest versions and download new versions of added apps.
    Check,
    /// Download the latest installer of apps and create their packages (catalog-<id>).
    Add {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Download the newest version of added apps (all of them when none are given).
    Update { ids: Vec<String> },
    /// Remove an app's package and downloaded installers.
    Remove { id: String },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Register and start the PSCCM Windows service (run as administrator).
    Install {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = 8080)]
        port: u16,
        /// Access token (required when binding to a non-loopback address).
        #[arg(long, env = "PSCCM_TOKEN")]
        token: Option<String>,
        /// Run the service as this account (e.g. CORP\svc-psccm); password from PSCCM_SERVICE_PASSWORD.
        #[arg(long)]
        account: Option<String>,
    },
    /// Stop and remove the service.
    Uninstall,
    /// Used by the service manager.
    #[command(hide = true)]
    Run,
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Install the agent (stores config and registers the service).
    Install {
        /// Console base URL the agent connects to, e.g. https://console:8080.
        #[arg(long)]
        server: String,
        /// Enrollment token from the console (Settings → Agents).
        #[arg(long, default_value = "")]
        token: String,
    },
    /// Stop and remove the agent.
    Uninstall,
    /// Enroll once in the foreground (for testing).
    Enroll,
    /// Show the agent's configuration and enrollment status.
    Status,
    /// Run the agent (used by the service; also works in the foreground).
    Run,
}

#[derive(Subcommand)]
enum PackagesCmd {
    List,
    Show { id: String },
    /// Check a package file for mistakes.
    Validate { file: PathBuf },
}

#[derive(Subcommand)]
enum ComputersCmd {
    List,
    Add {
        #[arg(required = true)]
        names: Vec<String>,
        /// windows or linux (detected automatically when omitted)
        #[arg(long)]
        os: Option<String>,
        #[arg(long, value_delimiter = ',')]
        tag: Vec<String>,
    },
    Remove {
        #[arg(required = true)]
        names: Vec<String>,
    },
    /// Check which computers are reachable.
    Check,
}

#[derive(Subcommand)]
enum AdCmd {
    /// List computers in AD; with --import add them to the computer list.
    Sync {
        #[arg(long)]
        import: bool,
    },
}

#[derive(Subcommand)]
enum CredCmd {
    List,
    /// Add a credential; the password is prompted (or read from PSCCM_SECRET).
    Add {
        #[arg(long)]
        name: String,
        /// windows, ssh or ldap
        #[arg(long)]
        kind: String,
        /// DOMAIN\user or user@domain (windows/ldap), login name (ssh).
        /// Empty windows user = the account running the console.
        #[arg(long, default_value = "")]
        user: String,
        /// SSH private key file (the prompted secret is its passphrase).
        #[arg(long)]
        key: Option<String>,
        /// Make this the default credential for "windows" or "linux" computers.
        #[arg(long)]
        default_for: Option<String>,
        /// Don't prompt for a secret.
        #[arg(long)]
        no_secret: bool,
    },
    Remove { name: String },
}

/// "data" next to the executable (portable), or %ProgramData%\PSCCM when
/// installed with the setup program.
pub fn default_data_dir() -> PathBuf {
    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()));
    if let Some(d) = &exe_dir
        && (d.join("data").is_dir() || !d.join("unins000.exe").exists()) {
            return d.join("data");
        }
    match std::env::var_os("ProgramData") {
        Some(pd) => PathBuf::from(pd).join("PSCCM"),
        None => PathBuf::from("data"),
    }
}

fn main() {
    let cli = Cli::parse();
    let data_dir = cli.data_dir.clone().unwrap_or_else(default_data_dir);
    // `agent run` manages its own runtime (and Windows service dispatch), so it
    // is handled before the console's tokio runtime is built.
    if matches!(cli.command, Some(Cmd::Agent { cmd: AgentCmd::Run })) {
        let agent_dir = agent::runtime::data_dir();
        let _ = std::fs::create_dir_all(&agent_dir);
        let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "psccm=info,warn".into());
        match std::fs::OpenOptions::new().create(true).append(true).open(agent_dir.join("agent.log")) {
            Ok(f) => tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(std::sync::Mutex::new(f)).init(),
            Err(_) => tracing_subscriber::fmt().with_env_filter(filter).init(),
        }
        if let Err(e) = agent::runtime::run() {
            tracing::error!("{e:#}");
            std::process::exit(1);
        }
        return;
    }
    let is_service = matches!(cli.command, Some(Cmd::Service { cmd: ServiceCmd::Run }));
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "psccm=info,warn".into());
    if is_service {
        // No console for a service: log to data\psccm.log.
        let _ = std::fs::create_dir_all(&data_dir);
        let log = std::fs::OpenOptions::new().create(true).append(true).open(data_dir.join("psccm.log"));
        match log {
            Ok(f) => tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(std::sync::Mutex::new(f)).init(),
            Err(_) => tracing_subscriber::fmt().with_env_filter(filter).init(),
        }
        if let Err(e) = service::run(data_dir) {
            tracing::error!("{e:#}");
            std::process::exit(1);
        }
        return;
    }
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(e) = rt.block_on(real_main(cli, data_dir)) {
        eprintln!("error: {e:#}");
        std::process::exit(2);
    }
}

async fn real_main(cli: Cli, data_dir: PathBuf) -> Result<()> {
    let app = Ctx::open(&data_dir)?;
    let command = cli.command.unwrap_or(Cmd::Serve {
        bind: "127.0.0.1".into(),
        port: 8080,
        token: std::env::var("PSCCM_TOKEN").ok(),
        no_browser: false,
    });
    match command {
        Cmd::Serve { bind, port, token, no_browser } => {
            let addr: std::net::SocketAddr = format!("{bind}:{port}").parse().context("invalid --bind/--port")?;
            app.db.fail_interrupted()?;
            app.db.fail_interrupted_jobs()?;
            scheduler::spawn(app.clone());
            let url = format!("http://{}:{port}/", if addr.ip().is_unspecified() { "localhost".into() } else { bind.clone() });
            println!("PSCCM console: {url}");
            println!("Data folder:   {}", app.data_dir.display());
            if !no_browser {
                open_browser(&url);
            }
            let shutdown = async {
                let _ = tokio::signal::ctrl_c().await;
            };
            web::serve(app, addr, token.filter(|t| !t.is_empty()), shutdown).await
        }
        Cmd::Service { cmd } => match cmd {
            ServiceCmd::Install { bind, port, token, account } => {
                let token = token.filter(|t| !t.is_empty());
                let ip: std::net::IpAddr = bind.parse().context("invalid --bind")?;
                if !ip.is_loopback() && token.is_none() {
                    bail!("binding to {bind} needs --token");
                }
                app.db.set_setting("server", &service::ServerSettings { bind, port, token })?;
                let password = std::env::var("PSCCM_SERVICE_PASSWORD").ok();
                service::install(&app.data_dir, account, password)?;
                println!("Service '{}' installed and started (data: {})", service::SERVICE_NAME, app.data_dir.display());
                Ok(())
            }
            ServiceCmd::Uninstall => {
                service::uninstall()?;
                println!("Service removed");
                Ok(())
            }
            ServiceCmd::Run => bail!("'service run' is started by the Windows service manager"),
        },
        Cmd::Agent { cmd } => match cmd {
            AgentCmd::Install { server, token } => agent::runtime::install(&server, &token),
            AgentCmd::Uninstall => agent::runtime::uninstall(),
            AgentCmd::Enroll => agent::runtime::enroll_now().await,
            AgentCmd::Status => agent::runtime::status(),
            AgentCmd::Run => unreachable!("handled in main()"),
        },
        Cmd::Update { install } => {
            let r = update::check(&app).await?;
            println!("Installed: {}   Latest: {} ({})", update::CURRENT, r.version, r.url);
            if !r.newer {
                println!("Up to date.");
            } else if install {
                println!("{}", update::install(&app).await?);
            } else {
                println!("A newer version is available; run 'psccm update --install'.");
            }
            Ok(())
        }
        Cmd::Groups { cmd } => match cmd {
            GroupsCmd::List => {
                let all = app.db.list_computers()?;
                let list = app.db.list_groups()?;
                let inv = groups::load_inventory(&app.db, list.iter().map(|g| &g.rules))?;
                for g in list {
                    let m: Vec<&str> = groups::members(&g, &all, &inv).iter().map(|c| c.name.as_str()).collect();
                    println!("{:<24} {:<8} {:>4}  {}", g.name, g.kind, m.len(), m.join(", "));
                }
                Ok(())
            }
            GroupsCmd::Create { name, computers } => {
                let id = app.db.save_group(&db::Group {
                    id: 0,
                    name,
                    description: String::new(),
                    kind: "static".into(),
                    rules: Default::default(),
                    members: vec![],
                })?;
                let n = app.db.set_group_members(id, &computers, true)?;
                println!("Group created with {n} computer(s)");
                Ok(())
            }
        },
        Cmd::Catalog { cmd } => catalog_cmd(&app, cmd).await,
        Cmd::Gpo { cmd } => match cmd {
            GpoCmd::List => {
                for g in gpo::list(&app).await? {
                    let imported = g.imported.as_ref().map(|m| format!("imported v{}", m.computer_version)).unwrap_or_default();
                    println!("{{{}}}  v{:<5} {:<45} {}", g.guid, g.computer_version, g.name, imported);
                }
                Ok(())
            }
            GpoCmd::ImportFolder { path, name } => {
                let m = gpo::import_folder(&app, &path, name)?;
                println!("Imported '{}' as package '{}' ({} registry settings)", m.name, m.package_id, m.machine_settings);
                Ok(())
            }
            GpoCmd::Import { guid } => {
                let m = gpo::import(&app, &guid).await?;
                println!("Imported '{}' as package '{}': {} registry settings, security: {}, audit: {}", m.name, m.package_id, m.machine_settings, m.has_security, m.has_audit);
                for u in &m.unsupported {
                    println!("  not applied: {u}");
                }
                Ok(())
            }
        },
        Cmd::Deploy { package, targets, tag, group, vars, concurrency, batch_size, max_failures, force } => {
            let lib = app.library()?;
            let pkg = lib.packages.get(&package).ok_or_else(|| anyhow!("package '{package}' not found"))?;
            let options = DeployOptions { concurrency, batch_size, max_failures, use_detect: !force };
            let spec = TargetSpec { computers: targets, tags: tag, groups: group };
            let id = engine::create_deployment(&app, pkg, parse_vars(&vars)?, spec, options, "cli", None)?;
            run_and_report(&app, &id).await
        }
        Cmd::Run { targets, script, shell } => {
            let shell = shell.map(|s| parse_shell(&s)).transpose()?;
            let pkg = engine::adhoc_package(&script, shell)?;
            let spec = TargetSpec { computers: targets, ..Default::default() };
            let id = engine::create_deployment(&app, &pkg, BTreeMap::new(), spec, DeployOptions::default(), "cli", None)?;
            run_and_report(&app, &id).await
        }
        Cmd::Packages { cmd } => packages_cmd(&app, cmd),
        Cmd::Computers { cmd } => computers_cmd(&app, cmd).await,
        Cmd::Ad { cmd: AdCmd::Sync { import } } => {
            let settings: ad::AdSettings = app.db.get_setting("ad")?;
            if import {
                println!("Imported {} computers from AD", ad::sync(&app).await?);
            } else {
                let list = ad::fetch(&app, &settings).await?;
                for c in &list {
                    println!(
                        "{:<40} {:<35} {:<6} {}",
                        c.name,
                        c.os,
                        if c.exists { "known" } else { "new" },
                        c.last_logon.as_deref().unwrap_or("-")
                    );
                }
                println!("{} computers (run with --import to add them)", list.len());
            }
            Ok(())
        }
        Cmd::Cred { cmd } => cred_cmd(&app, cmd),
        Cmd::Inventory { cmd } => inventory_cmd(&app, cmd).await,
        Cmd::Updates { cmd } => updates_cmd(&app, cmd).await,
        Cmd::History { limit } => {
            for d in app.db.list_deployments(limit)? {
                println!(
                    "{}  {:<10} {:<30} ok {:>3}  skip {:>3}  fail {:>3}  {}",
                    &d.created_at[..16.min(d.created_at.len())],
                    d.status,
                    d.package_name,
                    d.n_success,
                    d.n_skipped,
                    d.n_failed,
                    &d.id[..8]
                );
            }
            Ok(())
        }
    }
}

fn parse_vars(vars: &[String]) -> Result<BTreeMap<String, String>> {
    vars.iter()
        .map(|v| {
            v.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.to_string()))
                .ok_or_else(|| anyhow!("variable '{v}' must look like name=value"))
        })
        .collect()
}

fn parse_shell(s: &str) -> Result<Shell> {
    serde_json::from_value(serde_json::Value::String(s.to_lowercase()))
        .map_err(|_| anyhow!("shell must be powershell, cmd, bash or sh"))
}

async fn run_and_report(app: &App, id: &str) -> Result<()> {
    println!("Deployment {}", &id[..8]);
    let status = engine::run_deployment(app, id).await?;
    let mut failed = false;
    for t in app.db.list_targets(id, true)? {
        println!("\n=== {} [{}] {}", t.computer, t.status.to_uppercase(), t.error.as_deref().unwrap_or(""));
        print!("{}", t.log);
        failed |= matches!(t.status.as_str(), "failed" | "offline" | "cancelled");
    }
    println!("\nResult: {status}");
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// The CLI downloads but never auto-deploys: the process would exit before
/// a deployment finished. The console's daily check does that.
async fn catalog_cmd(app: &App, cmd: CatalogCmd) -> Result<()> {
    let print = |id: &str, r: &Result<String>| match r {
        Ok(m) => println!("{id:<18} {m}"),
        Err(e) => println!("{id:<18} ERROR: {e:#}"),
    };
    match cmd {
        CatalogCmd::List => {
            let (list, errors) = catalog::load(app);
            let s = catalog::settings(app)?;
            for a in &list {
                let st = s.apps.get(&a.id).cloned().unwrap_or_default();
                let have = match (&st.current, st.added) {
                    (Some(d), true) => format!("{} ({})", d.version, a.package_id()),
                    (None, true) => "added, not downloaded".into(),
                    _ => "-".into(),
                };
                println!("{:<18} {:<32} latest {:<20} have {}", a.id, a.name, st.latest.as_deref().unwrap_or("?"), have);
            }
            for e in errors {
                eprintln!("BROKEN {e}");
            }
            Ok(())
        }
        CatalogCmd::Check => {
            for (id, r) in catalog::check_all(app, false).await {
                print(&id, &r);
            }
            Ok(())
        }
        CatalogCmd::Add { ids } => {
            for id in ids {
                let r = catalog::add(app, &id).await;
                print(&id, &r);
            }
            Ok(())
        }
        CatalogCmd::Update { ids } => {
            let ids = if ids.is_empty() {
                catalog::settings(app)?.apps.into_iter().filter(|(_, a)| a.added).map(|(k, _)| k).collect()
            } else {
                ids
            };
            for id in ids {
                let r = catalog::refresh(app, &id, false).await;
                print(&id, &r);
            }
            Ok(())
        }
        CatalogCmd::Remove { id } => {
            catalog::remove(app, &id)?;
            println!("Removed {id}");
            Ok(())
        }
    }
}

fn packages_cmd(app: &App, cmd: PackagesCmd) -> Result<()> {
    match cmd {
        PackagesCmd::List => {
            let lib = app.library()?;
            let mut list: Vec<&Package> = lib.packages.values().collect();
            list.sort_by(|a, b| a.id.cmp(&b.id));
            for p in list {
                let platforms: Vec<&str> = p.platforms.iter().map(|o| o.as_str()).collect();
                println!("{:<24} {:<36} {}", p.id, p.name, platforms.join(","));
            }
            for (file, err) in &lib.errors {
                eprintln!("BROKEN {file}: {err}");
            }
            Ok(())
        }
        PackagesCmd::Show { id } => {
            let lib = app.library()?;
            let file = lib.files.get(&id).ok_or_else(|| anyhow!("package '{id}' not found"))?;
            print!("{}", std::fs::read_to_string(file)?);
            Ok(())
        }
        PackagesCmd::Validate { file } => {
            let p = Package::from_yaml(&std::fs::read_to_string(&file)?)?;
            println!("OK: '{}' ({}), {} steps", p.name, p.id, p.steps.len());
            Ok(())
        }
    }
}

async fn computers_cmd(app: &App, cmd: ComputersCmd) -> Result<()> {
    match cmd {
        ComputersCmd::List => {
            for c in app.db.list_computers()? {
                let online = match c.online {
                    Some(true) => "online",
                    Some(false) => "offline",
                    None => "-",
                };
                println!("{:<40} {:<8} {:<8} {:<6} {}", c.name, c.os, online, c.source, c.tags.join(","));
            }
            Ok(())
        }
        ComputersCmd::Add { names, os, tag } => {
            if let Some(o) = &os
                && o != "windows" && o != "linux" {
                    bail!("--os must be windows or linux");
                }
            for n in &names {
                app.db.upsert_computer(&NewComputer {
                    name: n.clone(),
                    os: os.clone(),
                    tags: Some(tag.clone()),
                    ..Default::default()
                })?;
            }
            println!("Added {} computer(s)", names.len());
            Ok(())
        }
        ComputersCmd::Remove { names } => {
            let ids: Vec<i64> = names
                .iter()
                .filter_map(|n| app.db.get_computer(n).ok().flatten().map(|c| c.id))
                .collect();
            app.db.delete_computers(&ids)?;
            println!("Removed {} computer(s)", ids.len());
            Ok(())
        }
        ComputersCmd::Check => {
            let mut res = engine::check_online(app, None).await?;
            res.sort();
            for (name, online) in &res {
                println!("{:<40} {}", name, if *online { "online" } else { "OFFLINE" });
            }
            Ok(())
        }
    }
}

async fn inventory_cmd(app: &App, cmd: InventoryCmd) -> Result<()> {
    match cmd {
        InventoryCmd::Scan { names } => {
            let mut results = inventory::scan_now(app, &names).await?;
            results.sort_by_key(|r| r.computer.to_lowercase());
            let mut bad = 0;
            for r in &results {
                println!("{:<40} {:<8} {}", r.computer, r.status, r.error.as_deref().unwrap_or(""));
                bad += usize::from(r.status != "ok");
            }
            println!("{} scanned, {bad} not scanned", results.len() - bad);
            if bad > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        InventoryCmd::Show { name } => {
            let inv = app
                .db
                .get_inventory(&name)?
                .ok_or_else(|| anyhow!("no inventory for '{name}' (run 'psccm inventory scan {name}')"))?;
            println!(
                "{}  last scan: {}  ({}{})",
                inv.computer,
                inv.scanned_at.as_deref().unwrap_or("never"),
                inv.status,
                inv.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default()
            );
            for (k, v) in &inv.facts {
                println!("  {k:<16} {v}");
            }
            for d in &inv.disks {
                println!("  disk {:<11} {:.1} GB free of {:.1} GB {}", d.mount, d.free as f64 / 1e9, d.size as f64 / 1e9, d.fs);
            }
            println!("  {} installed programs", inv.apps.len());
            for a in &inv.apps {
                println!("    {:<60} {:<24} {}", a.name, a.version, a.publisher);
            }
            Ok(())
        }
        InventoryCmd::Software { name } => {
            for (n, version, _publisher, count) in app.db.software_summary()? {
                if name.as_deref().is_none_or(|p| groups::glob(p, &n)) {
                    println!("{n:<60} {version:<24} {count:>5}");
                }
            }
            Ok(())
        }
        InventoryCmd::Changes { computer, days } => {
            let since = (chrono::Utc::now() - chrono::Duration::days(days.max(1)))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            for c in app.db.list_inventory_changes(computer.as_deref(), Some(&since), 1_000_000)? {
                let what = match c.kind.as_str() {
                    "installed" => c.new,
                    "removed" => c.old,
                    _ => format!("{} -> {}", c.old, c.new),
                };
                println!("{}  {:<24} {:<9} {:<50} {what}", c.at, c.computer, c.kind, c.name);
            }
            Ok(())
        }
        InventoryCmd::Outdated => {
            let r = catalog::outdated(app)?;
            for a in r["apps"].as_array().into_iter().flatten() {
                let outdated = a["outdated"].as_array().cloned().unwrap_or_default();
                println!(
                    "{:<32} latest {:<16} {} of {} computers outdated",
                    a["name"].as_str().unwrap_or(""),
                    a["latest"].as_str().unwrap_or("unknown"),
                    outdated.len(),
                    a["installed"]
                );
                for o in outdated {
                    println!("    {:<40} {}", o["computer"].as_str().unwrap_or(""), o["version"].as_str().unwrap_or(""));
                }
            }
            Ok(())
        }
    }
}

async fn updates_cmd(app: &App, cmd: UpdatesCmd) -> Result<()> {
    match cmd {
        UpdatesCmd::Scan { names } => {
            let mut results = wu::scan_now(app, &names).await?;
            results.sort_by_key(|r| r.computer.to_lowercase());
            let mut bad = 0;
            for r in &results {
                println!("{:<40} {:<8} {}", r.computer, r.status, r.error.as_deref().unwrap_or(""));
                bad += usize::from(r.status != "ok");
            }
            println!("{} scanned, {bad} not scanned", results.len() - bad);
            if bad > 0 {
                std::process::exit(1);
            }
            Ok(())
        }
        UpdatesCmd::List { security } => {
            let r = wu::report(app)?;
            for u in r["updates"].as_array().into_iter().flatten() {
                if security && u["security"] != true {
                    continue;
                }
                let computers: Vec<&str> = u["computers"].as_array().into_iter().flatten().filter_map(|c| c.as_str()).collect();
                println!(
                    "{:<10} {:<18} {:<9} {:>4}  {}{}",
                    u["update"]["kb"].as_str().filter(|k| !k.is_empty()).unwrap_or("-"),
                    u["update"]["classification"].as_str().unwrap_or(""),
                    u["update"]["severity"].as_str().unwrap_or(""),
                    computers.len(),
                    u["update"]["title"].as_str().unwrap_or(""),
                    if u["declined"] == true { "  (declined)" } else { "" }
                );
                println!("{:>44}{}", "", computers.join(", "));
            }
            Ok(())
        }
        UpdatesCmd::Show { name } => {
            let (s, missing) = app
                .db
                .get_wu_status(&name)?
                .ok_or_else(|| anyhow!("no update scan for '{name}' (run 'psccm updates scan {name}')"))?;
            println!(
                "{}  last scan: {}  ({}{})",
                s.computer,
                s.scanned_at.as_deref().unwrap_or("never"),
                s.status,
                s.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default()
            );
            for (k, v) in &s.info {
                println!("  {k:<16} {v}");
            }
            println!("  {} missing updates", missing.len());
            for u in &missing {
                let kb = if u.kb.is_empty() { "-" } else { &u.kb };
                println!("    {kb:<10} {:<18} {:<9} {}{}", u.classification, u.severity, u.title, if u.optional { " (optional)" } else { "" });
            }
            println!("  recent installs");
            for h in &s.history {
                let result = match h.result {
                    2 => "ok",
                    3 => "ok*",
                    4 => "failed",
                    5 => "aborted",
                    _ => "?",
                };
                println!("    {:<20} {result:<7} {}", h.at, h.title);
            }
            Ok(())
        }
    }
}

fn cred_cmd(app: &App, cmd: CredCmd) -> Result<()> {
    match cmd {
        CredCmd::List => {
            for c in app.db.list_credentials()? {
                println!(
                    "{:<20} {:<8} {:<30} {}",
                    c.name,
                    c.kind,
                    if c.username.is_empty() { "(console user)" } else { &c.username },
                    c.default_for.map(|d| format!("default for {d}")).unwrap_or_default()
                );
            }
            Ok(())
        }
        CredCmd::Add { name, kind, user, key, default_for, no_secret } => {
            let secret = if no_secret {
                None
            } else if let Ok(s) = std::env::var("PSCCM_SECRET") {
                Some(s)
            } else {
                let prompt = if key.is_some() { "Key passphrase (empty for none): " } else { "Password: " };
                Some(rpassword::prompt_password(prompt)?)
            };
            app.db.add_credential(&NewCredential {
                name,
                kind,
                username: user,
                secret,
                key_path: key,
                default_for,
            })?;
            println!("Saved");
            Ok(())
        }
        CredCmd::Remove { name } => {
            let c = app
                .db
                .list_credentials()?
                .into_iter()
                .find(|c| c.name == name)
                .ok_or_else(|| anyhow!("credential '{name}' not found"))?;
            app.db.delete_credential(c.id)?;
            println!("Removed");
            Ok(())
        }
    }
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    let r = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(not(windows))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    let _ = r;
}
