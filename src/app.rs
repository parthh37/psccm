//! Shared application state.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::db::Db;
use crate::package::{Library, load_library};

pub type App = Arc<Ctx>;

pub struct Ctx {
    pub db: Db,
    pub data_dir: PathBuf,
    /// Cancellation flags of running deployments.
    pub cancels: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WinRmSettings {
    /// Use HTTPS (port 5986) instead of HTTP (5985, encrypted by Kerberos/NTLM).
    #[serde(default)]
    pub use_ssl: bool,
    /// Accept self-signed WinRM HTTPS certificates.
    #[serde(default)]
    pub skip_ca_check: bool,
}

/// Built-in packages written to the packages folder on first start.
const BUILTIN: &[(&str, &str)] = &[
    ("7zip.yaml", include_str!("../packages/7zip.yaml")),
    ("google-chrome.yaml", include_str!("../packages/google-chrome.yaml")),
    ("notepad-plus-plus.yaml", include_str!("../packages/notepad-plus-plus.yaml")),
    ("nodejs.yaml", include_str!("../packages/nodejs.yaml")),
    ("iis-install.yaml", include_str!("../packages/iis-install.yaml")),
    ("iis-site-deploy.yaml", include_str!("../packages/iis-site-deploy.yaml")),
    ("iis-apppool-recycle.yaml", include_str!("../packages/iis-apppool-recycle.yaml")),
    ("iis-rolling-deploy.yaml", include_str!("../packages/iis-rolling-deploy.yaml")),
    ("uninstall-app.yaml", include_str!("../packages/uninstall-app.yaml")),
    ("haproxy-upgrade.yaml", include_str!("../packages/haproxy-upgrade.yaml")),
    ("haproxy-config.yaml", include_str!("../packages/haproxy-config.yaml")),
    ("haproxy-server-state.yaml", include_str!("../packages/haproxy-server-state.yaml")),
    ("install-msi.yaml", include_str!("../packages/install-msi.yaml")),
    ("install-exe.yaml", include_str!("../packages/install-exe.yaml")),
    ("linux-package.yaml", include_str!("../packages/linux-package.yaml")),
    ("linux-update.yaml", include_str!("../packages/linux-update.yaml")),
    ("reboot.yaml", include_str!("../packages/reboot.yaml")),
];

pub fn builtin_packages() -> &'static [(&'static str, &'static str)] {
    BUILTIN
}

impl Ctx {
    pub fn open(data_dir: &Path) -> Result<App> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("creating data directory {}", data_dir.display()))?;
        let ctx = Ctx {
            db: Db::open(&data_dir.join("psccm.db"))?,
            data_dir: data_dir.to_path_buf(),
            cancels: Mutex::new(HashMap::new()),
        };
        std::fs::create_dir_all(ctx.packages_dir())?;
        std::fs::create_dir_all(ctx.repo_dir())?;
        for (file, text) in BUILTIN {
            let path = ctx.packages_dir().join(file);
            // A deleted built-in is renamed to *.yaml.disabled; don't bring it back.
            if !path.exists() && !path.with_extension("yaml.disabled").exists() {
                std::fs::write(&path, text)?;
            }
        }
        Ok(Arc::new(ctx))
    }

    pub fn packages_dir(&self) -> PathBuf {
        self.data_dir.join("packages")
    }

    /// Installers and files that packages push to targets.
    pub fn repo_dir(&self) -> PathBuf {
        self.data_dir.join("repository")
    }

    pub fn library(&self) -> Result<Library> {
        load_library(&self.packages_dir())
    }

    pub fn winrm_settings(&self) -> WinRmSettings {
        self.db.get_setting("winrm").unwrap_or_default()
    }

    /// Resolve a repository-relative path, refusing anything that escapes it.
    pub fn repo_file(&self, rel: &str) -> Result<PathBuf> {
        let rel = rel.replace('\\', "/");
        if rel.split('/').any(|p| p == "..") || Path::new(&rel).is_absolute() {
            anyhow::bail!("repository path '{rel}' must be relative and stay inside the repository");
        }
        let path = self.repo_dir().join(&rel);
        if !path.is_file() {
            anyhow::bail!("file '{rel}' not found in repository folder {}", self.repo_dir().display());
        }
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use crate::package::Package;
    use std::collections::HashMap;

    #[test]
    fn builtin_packages_are_valid() {
        let mut lib = HashMap::new();
        for (file, text) in super::BUILTIN {
            let p = Package::from_yaml(text).unwrap_or_else(|e| panic!("{file}: {e:#}"));
            lib.insert(p.id.clone(), p);
        }
        // Every include resolves and every OS plans without error.
        for p in lib.values() {
            for os in [crate::package::Os::Windows, crate::package::Os::Linux] {
                if p.supports(os) {
                    let mut vars: std::collections::BTreeMap<String, String> =
                        p.variables.keys().map(|k| (k.clone(), "x".to_string())).collect();
                    p.plan(os, &lib, &mut vars).unwrap_or_else(|e| panic!("{}: {e:#}", p.id));
                }
            }
        }
    }

    /// Every script in the built-in packages parses with the real PowerShell
    /// and bash parsers (skipped where the interpreter is missing).
    #[test]
    fn builtin_scripts_parse() {
        use crate::package::{Os, Shell, render};
        use crate::transport::wrap_script;
        let dir = std::env::temp_dir().join(format!("psccm-parse-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut n = 0;
        for (file, text) in super::BUILTIN {
            let p = Package::from_yaml(text).unwrap();
            let mut vars: std::collections::BTreeMap<String, String> =
                p.variables.iter().map(|(k, v)| (k.clone(), v.default.clone().unwrap_or("x".into()))).collect();
            for b in crate::package::BUILTIN_VARS {
                vars.insert(b.to_string(), "x".into());
            }
            let mut scripts: Vec<(Shell, &str)> = Vec::new();
            for os in [Os::Windows, Os::Linux] {
                let sh = Shell::default_for(os);
                scripts.extend(p.detect.for_os(os).map(|s| (sh, s)));
                scripts.extend(p.verify.for_os(os).map(|s| (sh, s)));
            }
            for s in p.steps.iter().chain(&p.rollback) {
                let sh = s.shell.unwrap_or(Shell::default_for(s.os.unwrap_or(if p.supports(Os::Windows) { Os::Windows } else { Os::Linux })));
                scripts.extend(s.script.as_deref().map(|t| (sh, t)));
                scripts.extend(s.when.as_deref().map(|t| (sh, t)));
            }
            for (shell, body) in scripts {
                let ext = if shell == Shell::Powershell { "ps1" } else { "sh" };
                let path = dir.join(format!("{}-{n}.{ext}", file.trim_end_matches(".yaml")));
                std::fs::write(&path, wrap_script(shell, &render(body, &vars).unwrap())).unwrap();
                n += 1;
            }
        }
        // A working bash (on Windows runners "bash" may be a WSL stub without a distro).
        let bash = std::process::Command::new("bash")
            .args(["-c", "echo ok"])
            .output()
            .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "ok");
        let mut errors = Vec::new();
        for e in std::fs::read_dir(&dir).unwrap() {
            let path = e.unwrap().path();
            if bash && path.extension().unwrap() == "sh" {
                let out = std::process::Command::new("bash").arg("-n").arg(&path).output().unwrap();
                if !out.status.success() {
                    errors.push(format!("{}: {}", path.display(), String::from_utf8_lossy(&out.stderr)));
                }
            }
        }
        if cfg!(windows) {
            let ps = format!(
                "Get-ChildItem '{}' -Filter *.ps1 | ForEach-Object {{ $e = $null; [void][Management.Automation.Language.Parser]::ParseFile($_.FullName, [ref]$null, [ref]$e); foreach ($x in $e) {{ \"$($_.Name):$($x.Extent.StartLineNumber): $($x.Message)\" }} }}",
                dir.display()
            );
            let out = std::process::Command::new("powershell.exe").args(["-NoProfile", "-Command", &ps]).output().unwrap();
            let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !text.is_empty() {
                errors.push(text);
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(errors.is_empty(), "script syntax errors:\n{}", errors.join("\n"));
    }
}
