//! Package definitions: the YAML format that describes what to deploy.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Windows,
    Linux,
}

impl Os {
    pub fn as_str(&self) -> &'static str {
        match self {
            Os::Windows => "windows",
            Os::Linux => "linux",
        }
    }

    pub fn parse(s: &str) -> Option<Os> {
        let s = s.to_ascii_lowercase();
        if s.contains("windows") {
            Some(Os::Windows)
        } else if s == "linux" || s == "unix" {
            Some(Os::Linux)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    Powershell,
    Cmd,
    Bash,
    Sh,
}

impl Shell {
    pub fn default_for(os: Os) -> Shell {
        match os {
            Os::Windows => Shell::Powershell,
            Os::Linux => Shell::Bash,
        }
    }

    pub fn os(&self) -> Os {
        match self {
            Shell::Powershell | Shell::Cmd => Os::Windows,
            Shell::Bash | Shell::Sh => Os::Linux,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OsScripts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linux: Option<String>,
}

impl OsScripts {
    pub fn for_os(&self, os: Os) -> Option<&str> {
        match os {
            Os::Windows => self.windows.as_deref(),
            Os::Linux => self.linux.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variable {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    /// Masked in logs and the UI.
    #[serde(default)]
    pub secret: bool,
    /// Optional fixed choices shown as a dropdown in the UI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Install {
    /// File in the repository folder (pushed from the console).
    #[serde(default)]
    pub file: Option<String>,
    /// URL the target downloads directly.
    #[serde(default)]
    pub url: Option<String>,
    /// Extra installer arguments. MSI installs already get `/qn /norestart`.
    #[serde(default)]
    pub args: String,
    /// Optional SHA-256 the downloaded/pushed file must match.
    #[serde(default)]
    pub sha256: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopySpec {
    /// Path relative to the repository folder.
    pub src: String,
    /// Absolute destination path on the target (file path).
    pub dest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceSpec {
    pub name: String,
    pub action: ServiceAction,
}

/// Install missing Windows updates through the Windows Update Agent (see `wu.rs`).
/// Every field may use `{{variables}}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowsUpdateSpec {
    /// Comma-separated classifications ("Security Updates, Critical Updates"); "*" = all.
    /// Default: security, critical and definition updates and update rollups.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub classifications: String,
    /// Only these KBs or update IDs, comma-separated (empty = by classification).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kbs: String,
    /// Never these KBs or update IDs. Updates declined in the console are always left out.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub exclude: String,
    /// "if-required" (default), "never" or "always".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reboot: String,
    /// Install rounds: after a restart, look again for updates that needed it (default 2).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub passes: String,
    /// "default" (WSUS when the computer has one, else Windows Update) or "windows-update".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub name: String,
    /// Only run on this OS. Omit to run everywhere.
    #[serde(default)]
    pub os: Option<Os>,
    /// Condition script: the step runs only if it exits 0.
    #[serde(default)]
    pub when: Option<String>,
    /// Seconds before the step is aborted (default 1800).
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub retries: u32,
    #[serde(default = "default_retry_delay")]
    pub retry_delay: u64,
    #[serde(default)]
    pub success_codes: Option<Vec<i32>>,
    #[serde(default)]
    pub continue_on_error: bool,
    /// Run this step on another computer instead (e.g. the HAProxy host
    /// while deploying to a web server).
    #[serde(default)]
    pub delegate_to: Option<String>,

    // Exactly one of the following actions.
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub shell: Option<Shell>,
    #[serde(default)]
    pub install: Option<Install>,
    #[serde(default)]
    pub copy: Option<CopySpec>,
    #[serde(default)]
    pub service: Option<ServiceSpec>,
    #[serde(default)]
    pub reboot: bool,
    #[serde(default)]
    pub wait: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub windows_update: Option<WindowsUpdateSpec>,
    /// Run all steps of another package (nested package).
    #[serde(default)]
    pub include: Option<String>,
}

fn default_retry_delay() -> u64 {
    10
}

impl Step {
    fn action_count(&self) -> usize {
        [
            self.script.is_some(),
            self.install.is_some(),
            self.copy.is_some(),
            self.service.is_some(),
            self.reboot,
            self.wait.is_some(),
            self.windows_update.is_some(),
            self.include.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count()
    }

    pub fn runs_on(&self, os: Os) -> bool {
        let shell_ok = self.shell.map(|s| s.os() == os).unwrap_or(true);
        let wu_ok = self.windows_update.is_none() || os == Os::Windows;
        self.os.map(|o| o == os).unwrap_or(true) && shell_ok && wu_ok
    }

    pub fn success_codes(&self) -> Vec<i32> {
        match &self.success_codes {
            Some(c) => c.clone(),
            // 3010 = success, reboot required; 1641 = success, reboot started.
            None if self.install.is_some() => vec![0, 3010, 1641],
            None => vec![0],
        }
    }

    /// Every string in the step that is subject to `{{variable}}` substitution.
    fn templated_strings(&self) -> Vec<&str> {
        let mut v: Vec<&str> = vec![&self.name];
        v.extend(self.when.as_deref());
        v.extend(self.delegate_to.as_deref());
        v.extend(self.script.as_deref());
        if let Some(i) = &self.install {
            v.extend(i.file.as_deref());
            v.extend(i.url.as_deref());
            v.push(&i.args);
            v.extend(i.sha256.as_deref());
        }
        if let Some(c) = &self.copy {
            v.push(&c.src);
            v.push(&c.dest);
        }
        if let Some(s) = &self.service {
            v.push(&s.name);
        }
        if let Some(w) = &self.windows_update {
            v.extend([&w.classifications, &w.kbs, &w.exclude, &w.reboot, &w.passes, &w.source].map(String::as_str));
        }
        v
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    /// Supported operating systems. Empty means all.
    #[serde(default)]
    pub platforms: Vec<Os>,
    #[serde(default)]
    pub variables: BTreeMap<String, Variable>,
    /// Skip the package on a target when this script exits 0 ("already installed").
    #[serde(default)]
    pub detect: OsScripts,
    /// Run after all steps; a non-zero exit marks the target failed.
    #[serde(default)]
    pub verify: OsScripts,
    pub steps: Vec<Step>,
    /// Run (best effort) when a step fails, e.g. restore a config backup.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rollback: Vec<Step>,
}

/// Variables every package can use without declaring them.
pub const BUILTIN_VARS: &[&str] = &["host", "host_short", "os", "workdir", "deployment_id"];

impl Package {
    pub fn from_yaml(text: &str) -> Result<Package> {
        let pkg: Package = serde_yaml::from_str(text).context("invalid package YAML")?;
        pkg.validate()?;
        Ok(pkg)
    }

    pub fn supports(&self, os: Os) -> bool {
        self.platforms.is_empty() || self.platforms.contains(&os)
    }

    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || !self
                .id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            bail!("package id '{}' must use only a-z, 0-9, '-' and '_'", self.id);
        }
        if self.steps.is_empty() {
            bail!("package '{}' has no steps", self.id);
        }
        let known = |name: &str| BUILTIN_VARS.contains(&name) || self.variables.contains_key(name);
        let check = |what: &str, s: &str| -> Result<()> {
            for v in template_vars(s)? {
                if !known(&v) {
                    bail!("{what}: unknown variable '{{{{{v}}}}}' (declare it under 'variables')");
                }
            }
            Ok(())
        };
        let all_steps = self.steps.iter().map(|s| (false, s)).chain(self.rollback.iter().map(|s| (true, s)));
        for (i, (is_rollback, step)) in all_steps.enumerate() {
            let label = if is_rollback {
                format!("rollback step '{}'", step.name)
            } else {
                format!("step {} '{}'", i + 1, step.name)
            };
            if is_rollback && step.include.is_some() {
                bail!("{label}: 'include' is not allowed in rollback steps");
            }
            match step.action_count() {
                1 => {}
                0 => bail!("{label}: needs one action (script, install, copy, service, reboot, wait, windows_update or include)"),
                _ => bail!("{label}: has more than one action; split it into separate steps"),
            }
            if step.shell.is_some() && step.script.is_none() {
                bail!("{label}: 'shell' only applies to 'script' steps");
            }
            if let (Some(os), Some(shell)) = (step.os, step.shell)
                && shell.os() != os {
                    bail!("{label}: shell {shell:?} cannot run on {}", os.as_str());
                }
            if let Some(inst) = &step.install
                && inst.file.is_some() == inst.url.is_some() {
                    bail!("{label}: install needs exactly one of 'file' or 'url'");
                }
            for s in step.templated_strings() {
                check(&label, s)?;
            }
        }
        for (what, scripts) in [("detect", &self.detect), ("verify", &self.verify)] {
            for s in [&scripts.windows, &scripts.linux].into_iter().flatten() {
                check(what, s)?;
            }
        }
        Ok(())
    }

    /// Resolve the final variable values: declared defaults overridden by `given`.
    pub fn resolve_vars(&self, given: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
        let mut out = BTreeMap::new();
        for (name, var) in &self.variables {
            match given.get(name).or(var.default.as_ref()) {
                Some(v) => {
                    if !var.options.is_empty() && !var.options.contains(v) {
                        bail!("variable '{name}' must be one of: {}", var.options.join(", "));
                    }
                    out.insert(name.clone(), v.clone());
                }
                None if var.required => bail!("variable '{name}' is required"),
                None => {
                    out.insert(name.clone(), String::new());
                }
            }
        }
        for k in given.keys() {
            if !self.variables.contains_key(k) {
                bail!("package '{}' has no variable '{k}'", self.id);
            }
        }
        Ok(out)
    }

    pub fn secret_values(&self, vars: &BTreeMap<String, String>) -> Vec<String> {
        self.variables
            .iter()
            .filter(|(_, v)| v.secret)
            .filter_map(|(k, _)| vars.get(k).filter(|s| !s.is_empty()).cloned())
            .collect()
    }

    /// Steps that apply to `os`, with `include` steps expanded into the
    /// included package's steps. Included packages' variable defaults are
    /// added to `vars` where the caller has not set them.
    pub fn plan(
        &self,
        os: Os,
        library: &HashMap<String, Package>,
        vars: &mut BTreeMap<String, String>,
    ) -> Result<Vec<Step>> {
        self.plan_inner(os, library, vars, 0)
    }

    fn plan_inner(
        &self,
        os: Os,
        library: &HashMap<String, Package>,
        vars: &mut BTreeMap<String, String>,
        depth: usize,
    ) -> Result<Vec<Step>> {
        if depth > 5 {
            bail!("include nesting is too deep (loop between packages?)");
        }
        let mut out = Vec::new();
        // A delegated step's `os` refers to the delegate host, checked when it runs.
        for step in self.steps.iter().filter(|s| s.delegate_to.is_some() || s.runs_on(os)) {
            if let Some(inc) = &step.include {
                let child = library
                    .get(inc)
                    .ok_or_else(|| anyhow!("step '{}' includes unknown package '{inc}'", step.name))?;
                for (k, v) in &child.variables {
                    if !vars.contains_key(k) {
                        if v.required && v.default.is_none() {
                            bail!(
                                "included package '{inc}' needs variable '{k}'; declare it in '{}' too",
                                self.id
                            );
                        }
                        vars.insert(k.clone(), v.default.clone().unwrap_or_default());
                    }
                }
                out.extend(child.plan_inner(os, library, vars, depth + 1)?);
            } else {
                out.push(step.clone());
            }
        }
        Ok(out)
    }
}

/// Names of the `{{variables}}` used in `s`.
pub fn template_vars(s: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let end = after.find("}}").ok_or_else(|| anyhow!("unclosed '{{{{' in: {s}"))?;
        out.push(after[..end].trim().to_string());
        rest = &after[end + 2..];
    }
    Ok(out)
}

/// Replace `{{name}}` with values from `vars`. Unknown names are an error.
pub fn render(s: &str, vars: &BTreeMap<String, String>) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after.find("}}").ok_or_else(|| anyhow!("unclosed '{{{{'"))?;
        let key = after[..end].trim();
        let val = vars.get(key).ok_or_else(|| anyhow!("unknown variable '{key}'"))?;
        out.push_str(val);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

pub struct Library {
    pub packages: HashMap<String, Package>,
    /// Package id -> file it was loaded from.
    pub files: HashMap<String, std::path::PathBuf>,
    /// Files that failed to load, with the reason.
    pub errors: Vec<(String, String)>,
}

/// Load every `*.yaml` / `*.yml` package in `dir`.
pub fn load_library(dir: &Path) -> Result<Library> {
    let mut packages = HashMap::new();
    let mut files = HashMap::new();
    let mut errors = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "yaml" && ext != "yml" {
            continue;
        }
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        match std::fs::read_to_string(&path)
            .map_err(anyhow::Error::from)
            .and_then(|t| Package::from_yaml(&t))
        {
            Ok(p) => {
                if packages.contains_key(&p.id) {
                    errors.push((fname, format!("duplicate package id '{}'", p.id)));
                } else {
                    files.insert(p.id.clone(), path.clone());
                    packages.insert(p.id.clone(), p);
                }
            }
            Err(e) => errors.push((fname, format!("{e:#}"))),
        }
    }
    Ok(Library { packages, files, errors })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_replaces_and_rejects_unknown() {
        let mut v = BTreeMap::new();
        v.insert("ver".to_string(), "1.2".to_string());
        assert_eq!(render("node-{{ ver }}.msi", &v).unwrap(), "node-1.2.msi");
        assert!(render("{{nope}}", &v).is_err());
        assert!(render("{{ver", &v).is_err());
    }

    #[test]
    fn validation_catches_mistakes() {
        let bad_var = "id: x\nname: X\nsteps:\n  - name: s\n    script: echo {{missing}}\n";
        assert!(Package::from_yaml(bad_var).is_err());
        let two_actions = "id: x\nname: X\nsteps:\n  - name: s\n    script: a\n    wait: 3\n";
        assert!(Package::from_yaml(two_actions).is_err());
        let typo = "id: x\nname: X\nsteps:\n  - name: s\n    scrpt: a\n";
        assert!(Package::from_yaml(typo).is_err());
        let ok = "id: x\nname: X\nvariables:\n  v: {default: '1'}\nsteps:\n  - name: s\n    script: echo {{v}} {{host}}\n";
        assert!(Package::from_yaml(ok).is_ok());
    }

    #[test]
    fn plan_filters_os_and_expands_includes() {
        let child = Package::from_yaml(
            "id: child\nname: C\nvariables:\n  c: {default: x}\nsteps:\n  - name: c1\n    script: echo {{c}}\n",
        )
        .unwrap();
        let parent = Package::from_yaml(
            "id: parent\nname: P\nsteps:\n  - name: w\n    os: windows\n    script: a\n  - name: l\n    os: linux\n    script: b\n  - name: inc\n    include: child\n",
        )
        .unwrap();
        let lib: HashMap<_, _> = [("child".to_string(), child)].into_iter().collect();
        let mut vars = BTreeMap::new();
        let steps = parent.plan(Os::Linux, &lib, &mut vars).unwrap();
        let names: Vec<_> = steps.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["l", "c1"]);
        assert_eq!(vars.get("c").unwrap(), "x");
    }
}
