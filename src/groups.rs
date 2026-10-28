//! Computer groups: static member lists and dynamic rule-based groups.

use crate::db::{Computer, Group, GroupRules};

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
    }

    pub fn matches(&self, c: &Computer) -> bool {
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
        true
    }
}

/// Computers in a group (from the full computer list).
pub fn members<'a>(g: &Group, all: &'a [Computer]) -> Vec<&'a Computer> {
    if g.kind == "dynamic" {
        all.iter().filter(|c| g.rules.matches(c)).collect()
    } else {
        all.iter().filter(|c| g.members.iter().any(|m| m.eq_ignore_ascii_case(&c.name))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(r.matches(&c));
        r.os = Some("linux".into());
        assert!(!r.matches(&c));
        let r = GroupRules { ou: "ou=servers".into(), ad_group: "iis-servers".into(), os_detail: "2022".into(), ..Default::default() };
        assert!(r.matches(&c));
        let r = GroupRules { tags: vec!["dev".into()], ..Default::default() };
        assert!(!r.matches(&c));
        assert!(!GroupRules::default().matches(&c), "empty rules match nothing");
    }
}
