//! Computer groups: static member lists and dynamic rule-based groups.

use anyhow::{Result, bail};
use std::cmp::Ordering;
use std::collections::HashMap;

use crate::db::{Computer, Db, Group, GroupRules, Inventory};

/// Inventory by computer id, for rules that look at scanned data.
pub type InventoryIndex = HashMap<i64, Inventory>;

/// Glob match with `*` (any run) and `?` (one character), case-insensitive.
pub fn glob(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

fn contains_ci(hay: Option<&str>, needle: &str) -> bool {
    hay.unwrap_or("").to_lowercase().contains(&needle.to_lowercase())
}

impl GroupRules {
    pub fn is_empty(&self) -> bool {
        self.os.as_deref().unwrap_or("").is_empty()
            && self.name.trim().is_empty()
            && self.ou.trim().is_empty()
            && self.tags.is_empty()
            && self.ad_group.trim().is_empty()
            && self.os_detail.trim().is_empty()
            && !self.uses_inventory()
    }

    /// True when a rule needs scanned inventory data.
    pub fn uses_inventory(&self) -> bool {
        !self.app.trim().is_empty() || self.pending_reboot || self.low_disk_gb > 0.0
    }

    /// Reject rules that can never be evaluated.
    pub fn validate(&self) -> Result<()> {
        if !self.app_version.trim().is_empty() {
            if self.app.trim().is_empty() {
                bail!("a software version condition needs a software name");
            }
            parse_condition(&self.app_version)?;
        }
        if self.low_disk_gb < 0.0 {
            bail!("free disk space limit cannot be negative");
        }
        Ok(())
    }

    /// Does computer `c` (with its inventory, if scanned) match every rule?
    pub fn matches(&self, c: &Computer, inv: Option<&Inventory>) -> bool {
        if self.is_empty() {
            return false;
        }
        if let Some(os) = self.os.as_deref().filter(|o| !o.is_empty())
            && c.os != os {
                return false;
            }
        let names: Vec<&str> = self.name.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
        if !names.is_empty() && !names.iter().any(|p| glob(p, &c.name)) {
            return false;
        }
        if !self.ou.trim().is_empty() && !contains_ci(c.ad_ou.as_deref(), self.ou.trim()) {
            return false;
        }
        if !self.os_detail.trim().is_empty() && !contains_ci(c.os_detail.as_deref(), self.os_detail.trim()) {
            return false;
        }
        if !self.tags.is_empty() && !c.tags.iter().any(|t| self.tags.iter().any(|r| r.eq_ignore_ascii_case(t))) {
            return false;
        }
        let ad = self.ad_group.trim();
        if !ad.is_empty() && !c.ad_groups.iter().any(|g| g.eq_ignore_ascii_case(ad)) {
            return false;
        }
        if self.uses_inventory() {
            // Computers that were never scanned match no inventory rule.
            let Some(inv) = inv.filter(|i| i.scanned_at.is_some()) else { return false };
            if self.pending_reboot && inv.facts.get("pending_reboot").map(String::as_str) != Some("true") {
                return false;
            }
            if self.low_disk_gb > 0.0 {
                let limit = (self.low_disk_gb * 1_073_741_824.0) as u64;
                if !inv.disks.iter().any(|d| d.size > 0 && d.free < limit) {
                    return false;
                }
            }
            let app = self.app.trim();
            if !app.is_empty() {
                let has = inv.apps.iter().any(|a| glob(app, &a.name) && version_matches(&self.app_version, &a.version));
                if has == self.app_missing {
                    return false;
                }
            }
        }
        true
    }
}

/// Load the inventory that `rules` need (nothing when none of them use it).
pub fn load_inventory<'a>(db: &Db, rules: impl IntoIterator<Item = &'a GroupRules>) -> Result<InventoryIndex> {
    let (mut any, mut apps) = (false, false);
    for r in rules {
        any |= r.uses_inventory();
        apps |= !r.app.trim().is_empty();
    }
    if !any {
        return Ok(HashMap::new());
    }
    Ok(db.list_inventory(apps)?.into_iter().map(|i| (i.computer_id, i)).collect())
}

/// Lenient version parse: "130.0.6723.92", "v22.11.0", "1:2.34-0ubuntu3", "8.7 (64-bit)" -> numbers.
pub fn parse_version(s: &str) -> Vec<u64> {
    let s = s.trim();
    // A Debian epoch ("1:") is not part of the version.
    let s = match s.split_once(':') {
        Some((epoch, rest)) if !epoch.is_empty() && epoch.chars().all(|c| c.is_ascii_digit()) => rest,
        _ => s,
    };
    let Some(start) = s.find(|c: char| c.is_ascii_digit()) else { return vec![] };
    let run: String = s[start..].chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    run.split('.').map_while(|p| p.parse().ok()).collect()
}

pub fn cmp_versions(a: &[u64], b: &[u64]) -> Ordering {
    let n = a.len().max(b.len());
    (0..n)
        .map(|i| a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0)))
        .find(|o| o.is_ne())
        .unwrap_or(Ordering::Equal)
}

/// Split "<130" into ("<", [130]). No operator means "=".
fn parse_condition(cond: &str) -> Result<(&'static str, Vec<u64>)> {
    let cond = cond.trim();
    let op = ["<=", ">=", "!=", "==", "<", ">", "="].into_iter().find(|op| cond.starts_with(op)).unwrap_or("");
    let want = parse_version(&cond[op.len()..]);
    if want.is_empty() {
        bail!("version condition '{cond}' needs a version number, e.g. <130 or >=22.11");
    }
    let op = match op {
        "" | "==" => "=",
        o => o,
    };
    Ok((op, want))
}

/// Does `version` satisfy `cond` ("<130", ">=22.11", "22")? An empty condition matches anything.
pub fn version_matches(cond: &str, version: &str) -> bool {
    if cond.trim().is_empty() {
        return true;
    }
    let Ok((op, want)) = parse_condition(cond) else { return false };
    let have = parse_version(version);
    if have.is_empty() {
        return false;
    }
    match op {
        // "=22" matches 22, 22.11 and 22.11.0: only the given parts are compared.
        "=" | "!=" => {
            let eq = (have.len() >= want.len() && have[..want.len()] == want[..]) || cmp_versions(&have, &want).is_eq();
            eq == (op == "=")
        }
        "<" => cmp_versions(&have, &want).is_lt(),
        "<=" => cmp_versions(&have, &want).is_le(),
        ">" => cmp_versions(&have, &want).is_gt(),
        _ => cmp_versions(&have, &want).is_ge(),
    }
}

/// Computers in a group (from the full computer list and the inventory
/// from [`load_inventory`]).
pub fn members<'a>(g: &Group, all: &'a [Computer], inv: &InventoryIndex) -> Vec<&'a Computer> {
    if g.kind == "dynamic" {
        all.iter().filter(|c| g.rules.matches(c, inv.get(&c.id))).collect()
    } else {
        all.iter().filter(|c| g.members.iter().any(|m| m.eq_ignore_ascii_case(&c.name))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Disk, InstalledApp};

    fn comp(name: &str, os: &str, ou: &str, tags: &[&str], ad: &[&str]) -> Computer {
        Computer {
            id: 1,
            name: name.into(),
            address: None,
            os: os.into(),
            os_detail: Some("Windows Server 2022 Datacenter".into()),
            source: "ad".into(),
            ad_ou: Some(ou.into()),
            description: None,
            tags: tags.iter().map(|s| s.to_string()).collect(),
            credential_id: None,
            port: None,
            last_logon: None,
            online: None,
            last_check: None,
            ad_groups: ad.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn inv(apps: &[(&str, &str)], pending: bool, free_gb: u64) -> Inventory {
        Inventory {
            computer_id: 1,
            computer: "web01".into(),
            scanned_at: Some("2026-10-29T10:00:00Z".into()),
            last_attempt: "2026-10-29T10:00:00Z".into(),
            status: "ok".into(),
            error: None,
            facts: [("pending_reboot".to_string(), pending.to_string())].into(),
            disks: vec![Disk { mount: "C:".into(), size: 100 << 30, free: free_gb << 30, fs: "NTFS".into() }],
            app_count: apps.len() as i64,
            apps: apps
                .iter()
                .map(|(n, v)| InstalledApp { name: n.to_string(), version: v.to_string(), publisher: String::new() })
                .collect(),
        }
    }

    #[test]
    fn glob_patterns() {
        assert!(glob("web*", "WEB01.corp.local"));
        assert!(glob("app0?", "app07"));
        assert!(!glob("app0?", "app10"));
        assert!(glob("*sql*", "prod-sql-02"));
        assert!(!glob("web*", "db01"));
    }

    #[test]
    fn dynamic_rules() {
        let c = comp("web01.corp.local", "windows", "OU=Web,OU=Servers,DC=corp", &["prod"], &["IIS-Servers"]);
        let mut r = GroupRules { name: "web*, api*".into(), ..Default::default() };
        assert!(r.matches(&c, None));
        r.os = Some("linux".into());
        assert!(!r.matches(&c, None));
        let r = GroupRules { ou: "ou=servers".into(), ad_group: "iis-servers".into(), os_detail: "2022".into(), ..Default::default() };
        assert!(r.matches(&c, None));
        let r = GroupRules { tags: vec!["dev".into()], ..Default::default() };
        assert!(!r.matches(&c, None));
        assert!(!GroupRules::default().matches(&c, None), "empty rules match nothing");
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("130.0.6723.92"), [130, 0, 6723, 92]);
        assert_eq!(parse_version("v22.11.0"), [22, 11, 0]);
        assert_eq!(parse_version("1:2.34-0ubuntu3"), [2, 34]);
        assert_eq!(parse_version("8.7 (64-bit)"), [8, 7]);
        assert!(parse_version("unknown").is_empty());
        assert!(version_matches("<130", "129.0.6668.101"));
        assert!(!version_matches("<130", "130.0.1"));
        assert!(version_matches(">=22.11", "22.11.0"));
        assert!(version_matches(">22", "22.0.1"));
        assert!(version_matches("22", "22.11.0"));
        assert!(!version_matches("22", "2.2"));
        assert!(version_matches("!=24.09", "23.01"));
        assert!(version_matches("", "anything"));
        assert!(!version_matches("<130", ""));
        assert!(parse_condition("<abc").is_err());
    }

    #[test]
    fn inventory_rules() {
        let c = comp("web01", "windows", "", &[], &[]);
        let i = inv(&[("Google Chrome", "129.0.6668.101"), ("7-Zip 24.09 (x64)", "24.09")], true, 5);
        let chrome_old = GroupRules { app: "Google Chrome*".into(), app_version: "<130".into(), ..Default::default() };
        assert!(chrome_old.matches(&c, Some(&i)));
        assert!(!chrome_old.matches(&c, None), "never scanned matches no inventory rule");
        let no_npp = GroupRules { app: "Notepad++*".into(), app_missing: true, ..Default::default() };
        assert!(no_npp.matches(&c, Some(&i)));
        let no_7z = GroupRules { app: "7-Zip*".into(), app_missing: true, ..Default::default() };
        assert!(!no_7z.matches(&c, Some(&i)));
        assert!(GroupRules { pending_reboot: true, ..Default::default() }.matches(&c, Some(&i)));
        assert!(GroupRules { low_disk_gb: 10.0, ..Default::default() }.matches(&c, Some(&i)));
        assert!(!GroupRules { low_disk_gb: 2.0, ..Default::default() }.matches(&c, Some(&i)));
        let combined = GroupRules { name: "db*".into(), pending_reboot: true, ..Default::default() };
        assert!(!combined.matches(&c, Some(&i)));
        assert!(GroupRules { app_version: "<1".into(), ..Default::default() }.validate().is_err());
    }
}
