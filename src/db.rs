//! SQLite storage: computers, credentials, settings, deployment history, schedules.

use anyhow::{Result, anyhow};
use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::agent::{DiskUsage, EnrollReq, Job, JobResult, Metrics, STALE_SECS};
use crate::secrets;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS computers (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE,
    address TEXT,
    os TEXT NOT NULL DEFAULT 'unknown',
    os_detail TEXT,
    source TEXT NOT NULL DEFAULT 'manual',
    ad_ou TEXT,
    description TEXT,
    tags TEXT NOT NULL DEFAULT '',
    credential_id INTEGER,
    port INTEGER,
    last_logon TEXT,
    online INTEGER,
    last_check TEXT,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS credentials (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL,
    username TEXT NOT NULL DEFAULT '',
    secret TEXT,
    key_path TEXT,
    default_for TEXT,
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS settings (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS known_hosts (
    host TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    added_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS deployments (
    id TEXT PRIMARY KEY,
    package_id TEXT NOT NULL,
    package_name TEXT NOT NULL,
    package_yaml TEXT NOT NULL,
    variables TEXT NOT NULL,
    targets TEXT NOT NULL,
    options TEXT NOT NULL,
    status TEXT NOT NULL,
    source TEXT NOT NULL,
    created_at TEXT NOT NULL,
    scheduled_for TEXT,
    started_at TEXT,
    finished_at TEXT
);
CREATE TABLE IF NOT EXISTS deployment_targets (
    id INTEGER PRIMARY KEY,
    deployment_id TEXT NOT NULL REFERENCES deployments(id) ON DELETE CASCADE,
    computer TEXT NOT NULL,
    os TEXT NOT NULL,
    status TEXT NOT NULL,
    current_step TEXT,
    error TEXT,
    started_at TEXT,
    finished_at TEXT,
    log TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_targets_dep ON deployment_targets(deployment_id);
CREATE TABLE IF NOT EXISTS schedules (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    package_id TEXT NOT NULL,
    targets TEXT NOT NULL,
    variables TEXT NOT NULL,
    options TEXT NOT NULL,
    kind TEXT NOT NULL,
    time TEXT NOT NULL DEFAULT '02:00',
    weekdays TEXT NOT NULL DEFAULT '[]',
    interval_minutes INTEGER NOT NULL DEFAULT 0,
    enabled INTEGER NOT NULL DEFAULT 1,
    last_run TEXT,
    next_run TEXT
);
CREATE TABLE IF NOT EXISTS groups (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE,
    description TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL DEFAULT 'static',
    rules TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS group_members (
    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    computer_id INTEGER NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    PRIMARY KEY (group_id, computer_id)
);
CREATE TABLE IF NOT EXISTS agents (
    id TEXT PRIMARY KEY,
    computer TEXT NOT NULL,
    agent_key TEXT NOT NULL,
    hostname TEXT NOT NULL DEFAULT '',
    os TEXT NOT NULL DEFAULT 'unknown',
    os_version TEXT NOT NULL DEFAULT '',
    version TEXT NOT NULL DEFAULT '',
    address TEXT,
    enrolled_at TEXT NOT NULL,
    last_checkin TEXT,
    cpu_pct REAL,
    mem_used INTEGER,
    mem_total INTEGER,
    uptime_secs INTEGER,
    processes INTEGER,
    disks TEXT NOT NULL DEFAULT '[]'
);
CREATE TABLE IF NOT EXISTS agent_jobs (
    id INTEGER PRIMARY KEY,
    agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    params TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'queued',
    output TEXT NOT NULL DEFAULT '',
    data TEXT NOT NULL DEFAULT 'null',
    source TEXT NOT NULL DEFAULT 'console',
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_jobs_agent ON agent_jobs(agent_id, status);
CREATE TABLE IF NOT EXISTS agent_metrics (
    id INTEGER PRIMARY KEY,
    agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    ts TEXT NOT NULL,
    cpu_pct REAL,
    mem_used INTEGER,
    mem_total INTEGER,
    disks TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS idx_metrics_agent ON agent_metrics(agent_id, ts);
CREATE TABLE IF NOT EXISTS agent_inventory (
    agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    data TEXT NOT NULL,
    synced_at TEXT NOT NULL,
    PRIMARY KEY (agent_id, kind)
);
CREATE TABLE IF NOT EXISTS inventory (
    computer_id INTEGER PRIMARY KEY REFERENCES computers(id) ON DELETE CASCADE,
    scanned_at TEXT,
    last_attempt TEXT NOT NULL,
    status TEXT NOT NULL,
    error TEXT,
    facts TEXT NOT NULL DEFAULT '{}',
    disks TEXT NOT NULL DEFAULT '[]'
);
CREATE TABLE IF NOT EXISTS inventory_apps (
    computer_id INTEGER NOT NULL REFERENCES computers(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    version TEXT NOT NULL DEFAULT '',
    publisher TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_inv_apps_computer ON inventory_apps(computer_id);
"#;

/// Columns added after the first release: (table, column, definition).
const MIGRATIONS: &[(&str, &str, &str)] = &[("computers", "ad_groups", "TEXT NOT NULL DEFAULT ''")];

pub fn now() -> String {
    Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        for (table, col, def) in MIGRATIONS {
            let exists: bool = conn.query_row(
                &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name=?1"),
                [col],
                |r| r.get::<_, i64>(0),
            )? > 0;
            if !exists {
                conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {col} {def}"))?;
            }
        }
        Ok(Db(Arc::new(Mutex::new(conn))))
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        let conn = self.0.lock().unwrap_or_else(|e| e.into_inner());
        Ok(f(&conn)?)
    }

    // ---------------------------------------------------------------- settings

    pub fn get_setting<T: DeserializeOwned + Default>(&self, key: &str) -> Result<T> {
        let raw: Option<String> = self.with(|c| {
            c.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get(0))
                .optional()
        })?;
        Ok(match raw {
            Some(s) => serde_json::from_str(&s)?,
            None => T::default(),
        })
    }

    pub fn set_setting<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let s = serde_json::to_string(value)?;
        self.with(|c| {
            c.execute(
                "INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![key, s],
            )
        })?;
        Ok(())
    }

    // --------------------------------------------------------------- computers

    pub fn list_computers(&self) -> Result<Vec<Computer>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM computers ORDER BY name COLLATE NOCASE")?;
            st.query_map([], Computer::from_row)?.collect()
        })
    }

    pub fn get_computer(&self, name: &str) -> Result<Option<Computer>> {
        self.with(|c| {
            c.query_row("SELECT * FROM computers WHERE name=?1", [name], Computer::from_row)
                .optional()
        })
    }

    /// Insert or update by name. Fields that are `None` keep their old value.
    pub fn upsert_computer(&self, n: &NewComputer) -> Result<()> {
        let tags = n.tags.as_ref().map(|t| join_tags(t));
        let ad_groups = n.ad_groups.as_ref().map(|g| g.join("\n"));
        self.with(|c| {
            c.execute(
                "INSERT INTO computers(name,address,os,os_detail,source,ad_ou,description,tags,credential_id,port,last_logon,created_at,ad_groups)
                 VALUES(?1,?2,COALESCE(?3,'unknown'),?4,COALESCE(?5,'manual'),?6,?7,COALESCE(?8,''),?9,?10,?11,?12,COALESCE(?13,''))
                 ON CONFLICT(name) DO UPDATE SET
                   ad_groups=COALESCE(?13,ad_groups),
                   address=COALESCE(excluded.address,address),
                   os=COALESCE(?3,os),
                   os_detail=COALESCE(excluded.os_detail,os_detail),
                   source=COALESCE(?5,source),
                   ad_ou=COALESCE(excluded.ad_ou,ad_ou),
                   description=COALESCE(excluded.description,description),
                   tags=COALESCE(?8,tags),
                   credential_id=COALESCE(excluded.credential_id,credential_id),
                   port=COALESCE(excluded.port,port),
                   last_logon=COALESCE(excluded.last_logon,last_logon)",
                params![
                    n.name.trim(),
                    n.address,
                    n.os,
                    n.os_detail,
                    n.source,
                    n.ad_ou,
                    n.description,
                    tags,
                    n.credential_id,
                    n.port,
                    n.last_logon,
                    now(),
                    ad_groups
                ],
            )
        })?;
        Ok(())
    }

    /// Full update from the UI edit form (None clears a field).
    pub fn update_computer(&self, id: i64, u: &ComputerUpdate) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE computers SET address=?2, os=?3, description=?4, tags=?5, credential_id=?6, port=?7 WHERE id=?1",
                params![
                    id,
                    u.address.as_deref().filter(|s| !s.is_empty()),
                    u.os,
                    u.description,
                    join_tags(&u.tags),
                    u.credential_id,
                    u.port
                ],
            )
        })?;
        Ok(())
    }

    pub fn delete_computers(&self, ids: &[i64]) -> Result<()> {
        self.with(|c| {
            for id in ids {
                c.execute("DELETE FROM computers WHERE id=?1", [id])?;
            }
            Ok(())
        })
    }

    pub fn set_online(&self, name: &str, online: bool) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE computers SET online=?2, last_check=?3 WHERE name=?1",
                params![name, online, now()],
            )
        })?;
        Ok(())
    }

    pub fn set_computer_os(&self, name: &str, os: &str) -> Result<()> {
        self.with(|c| c.execute("UPDATE computers SET os=?2 WHERE name=?1", params![name, os]))?;
        Ok(())
    }

    // ------------------------------------------------------------------ groups

    pub fn list_groups(&self) -> Result<Vec<Group>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM groups ORDER BY name COLLATE NOCASE")?;
            let mut groups: Vec<Group> = st.query_map([], Group::from_row)?.collect::<rusqlite::Result<_>>()?;
            let mut st = c.prepare(
                "SELECT m.group_id, c.name FROM group_members m JOIN computers c ON c.id=m.computer_id ORDER BY c.name COLLATE NOCASE",
            )?;
            let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (gid, name) = row?;
                if let Some(g) = groups.iter_mut().find(|g| g.id == gid) {
                    g.members.push(name);
                }
            }
            Ok(groups)
        })
    }

    pub fn save_group(&self, g: &Group) -> Result<i64> {
        let rules = serde_json::to_string(&g.rules)?;
        self.with(|c| {
            if g.id > 0 {
                c.execute(
                    "UPDATE groups SET name=?2, description=?3, kind=?4, rules=?5 WHERE id=?1",
                    params![g.id, g.name.trim(), g.description, g.kind, rules],
                )?;
                Ok(g.id)
            } else {
                c.execute(
                    "INSERT INTO groups(name,description,kind,rules,created_at) VALUES(?1,?2,?3,?4,?5)",
                    params![g.name.trim(), g.description, g.kind, rules, now()],
                )?;
                Ok(c.last_insert_rowid())
            }
        })
    }

    pub fn delete_group(&self, id: i64) -> Result<()> {
        self.with(|c| c.execute("DELETE FROM groups WHERE id=?1", [id]))?;
        Ok(())
    }

    /// Add (or remove) computers, by name, to a static group.
    pub fn set_group_members(&self, group_id: i64, names: &[String], add: bool) -> Result<usize> {
        self.with(|c| {
            let mut n = 0;
            for name in names {
                let sql = if add {
                    "INSERT OR IGNORE INTO group_members(group_id, computer_id) SELECT ?1, id FROM computers WHERE name=?2"
                } else {
                    "DELETE FROM group_members WHERE group_id=?1 AND computer_id=(SELECT id FROM computers WHERE name=?2)"
                };
                n += c.execute(sql, params![group_id, name])?;
            }
            Ok(n)
        })
    }

    // ------------------------------------------------------------- credentials

    pub fn list_credentials(&self) -> Result<Vec<Credential>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM credentials ORDER BY name")?;
            st.query_map([], Credential::from_row)?.collect()
        })
    }

    pub fn get_credential(&self, id: i64) -> Result<Option<Credential>> {
        self.with(|c| {
            c.query_row("SELECT * FROM credentials WHERE id=?1", [id], Credential::from_row)
                .optional()
        })
    }

    pub fn default_credential(&self, kind: &str) -> Result<Option<Credential>> {
        self.with(|c| {
            c.query_row(
                "SELECT * FROM credentials WHERE default_for=?1 LIMIT 1",
                [kind],
                Credential::from_row,
            )
            .optional()
        })
    }

    pub fn add_credential(&self, n: &NewCredential) -> Result<i64> {
        let secret = match n.secret.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => Some(secrets::protect(s)?),
            None => None,
        };
        self.with(|c| {
            if let Some(d) = &n.default_for {
                c.execute("UPDATE credentials SET default_for=NULL WHERE default_for=?1", [d])?;
            }
            c.execute(
                "INSERT INTO credentials(name,kind,username,secret,key_path,default_for,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![n.name, n.kind, n.username, secret, n.key_path, n.default_for, now()],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn delete_credential(&self, id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("UPDATE computers SET credential_id=NULL WHERE credential_id=?1", [id])?;
            c.execute("DELETE FROM credentials WHERE id=?1", [id])
        })?;
        Ok(())
    }

    /// Decrypted password / passphrase for a credential.
    pub fn credential_secret(&self, id: i64) -> Result<Option<String>> {
        let enc: Option<Option<String>> = self.with(|c| {
            c.query_row("SELECT secret FROM credentials WHERE id=?1", [id], |r| r.get(0))
                .optional()
        })?;
        match enc.flatten() {
            Some(e) => Ok(Some(secrets::unprotect(&e)?)),
            None => Ok(None),
        }
    }

    // ------------------------------------------------------------- known hosts

    pub fn known_host(&self, host: &str) -> Result<Option<String>> {
        self.with(|c| {
            c.query_row("SELECT fingerprint FROM known_hosts WHERE host=?1", [host], |r| r.get(0))
                .optional()
        })
    }

    pub fn add_known_host(&self, host: &str, fp: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO known_hosts(host,fingerprint,added_at) VALUES(?1,?2,?3)",
                params![host, fp, now()],
            )
        })?;
        Ok(())
    }

    pub fn forget_known_host(&self, host: &str) -> Result<()> {
        self.with(|c| c.execute("DELETE FROM known_hosts WHERE host=?1", [host]))?;
        Ok(())
    }

    // ------------------------------------------------------------- deployments

    pub fn create_deployment(&self, d: &NewDeployment) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO deployments(id,package_id,package_name,package_yaml,variables,targets,options,status,source,created_at,scheduled_for)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    d.id,
                    d.package_id,
                    d.package_name,
                    d.package_yaml,
                    serde_json::to_string(&d.variables).unwrap(),
                    serde_json::to_string(&d.targets).unwrap(),
                    serde_json::to_string(&d.options).unwrap(),
                    if d.scheduled_for.is_some() { "scheduled" } else { "queued" },
                    d.source,
                    now(),
                    d.scheduled_for
                ],
            )
        })?;
        Ok(())
    }

    pub fn list_deployments(&self, limit: usize) -> Result<Vec<Deployment>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "{DEPLOYMENT_SELECT} ORDER BY d.created_at DESC LIMIT {limit}"
            ))?;
            st.query_map([], Deployment::from_row)?.collect()
        })
    }

    pub fn get_deployment(&self, id: &str) -> Result<Option<Deployment>> {
        self.with(|c| {
            c.query_row(&format!("{DEPLOYMENT_SELECT} WHERE d.id=?1"), [id], Deployment::from_row)
                .optional()
        })
    }

    pub fn due_scheduled_deployments(&self) -> Result<Vec<String>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id FROM deployments WHERE status='scheduled' AND scheduled_for<=?1",
            )?;
            st.query_map([now()], |r| r.get(0))?.collect()
        })
    }

    /// Deployments left running/queued by a previous process are marked failed.
    pub fn fail_interrupted(&self) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE deployment_targets SET status='failed', error='console restarted during deployment', finished_at=?1
                 WHERE status IN ('pending','running')",
                [now()],
            )?;
            c.execute(
                "UPDATE deployments SET status='failed', finished_at=?1 WHERE status IN ('queued','running')",
                [now()],
            )
        })?;
        Ok(())
    }

    pub fn set_deployment_status(&self, id: &str, status: &str) -> Result<()> {
        self.with(|c| {
            let col = match status {
                "running" => "started_at",
                _ => "finished_at",
            };
            c.execute(
                &format!("UPDATE deployments SET status=?2, {col}=COALESCE({col}, ?3) WHERE id=?1"),
                params![id, status, now()],
            )
        })?;
        Ok(())
    }

    pub fn delete_deployment(&self, id: &str) -> Result<()> {
        self.with(|c| c.execute("DELETE FROM deployments WHERE id=?1", [id]))?;
        Ok(())
    }

    pub fn add_target(&self, dep: &str, computer: &str, os: &str) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO deployment_targets(deployment_id,computer,os,status) VALUES(?1,?2,?3,'pending')",
                params![dep, computer, os],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn list_targets(&self, dep: &str, with_log: bool) -> Result<Vec<TargetRow>> {
        self.with(|c| {
            let log = if with_log { "log" } else { "''" };
            let mut st = c.prepare(&format!(
                "SELECT id,deployment_id,computer,os,status,current_step,error,started_at,finished_at,{log}
                 FROM deployment_targets WHERE deployment_id=?1 ORDER BY id"
            ))?;
            st.query_map([dep], TargetRow::from_row)?.collect()
        })
    }

    pub fn target_log(&self, target_id: i64) -> Result<String> {
        self.with(|c| {
            c.query_row("SELECT log FROM deployment_targets WHERE id=?1", [target_id], |r| r.get(0))
        })
    }

    pub fn update_target(
        &self,
        id: i64,
        status: &str,
        step: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let terminal = !matches!(status, "pending" | "running");
        self.with(|c| {
            c.execute(
                "UPDATE deployment_targets SET status=?2, current_step=?3, error=?4,
                    started_at=CASE WHEN ?2='running' THEN COALESCE(started_at, ?5) ELSE started_at END,
                    finished_at=CASE WHEN ?6 THEN ?5 ELSE finished_at END
                 WHERE id=?1",
                params![id, status, step, error, now(), terminal],
            )
        })?;
        Ok(())
    }

    pub fn append_log(&self, target_id: i64, text: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE deployment_targets SET log = log || ?2 WHERE id=?1",
                params![target_id, text],
            )
        })?;
        Ok(())
    }

    // --------------------------------------------------------------- schedules

    pub fn list_schedules(&self) -> Result<Vec<Schedule>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM schedules ORDER BY name")?;
            st.query_map([], Schedule::from_row)?.collect()
        })
    }

    pub fn save_schedule(&self, s: &Schedule) -> Result<i64> {
        let targets = serde_json::to_string(&s.targets)?;
        let variables = serde_json::to_string(&s.variables)?;
        let options = serde_json::to_string(&s.options)?;
        let weekdays = serde_json::to_string(&s.weekdays)?;
        self.with(|c| {
            let args = params![
                s.name,
                s.package_id,
                targets,
                variables,
                options,
                s.kind,
                s.time,
                weekdays,
                s.interval_minutes,
                s.enabled,
                s.next_run,
                s.id,
            ];
            if s.id > 0 {
                c.execute(
                    "UPDATE schedules SET name=?1,package_id=?2,targets=?3,variables=?4,options=?5,kind=?6,time=?7,weekdays=?8,interval_minutes=?9,enabled=?10,next_run=?11 WHERE id=?12",
                    args,
                )?;
                Ok(s.id)
            } else {
                c.execute(
                    "INSERT INTO schedules(name,package_id,targets,variables,options,kind,time,weekdays,interval_minutes,enabled,next_run) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                    &args[..11],
                )?;
                Ok(c.last_insert_rowid())
            }
        })
    }

    pub fn mark_schedule_run(&self, id: i64, next_run: Option<&str>) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE schedules SET last_run=?2, next_run=?3 WHERE id=?1",
                params![id, now(), next_run],
            )
        })?;
        Ok(())
    }

    pub fn delete_schedule(&self, id: i64) -> Result<()> {
        self.with(|c| c.execute("DELETE FROM schedules WHERE id=?1", [id]))?;
        Ok(())
    }

    // ------------------------------------------------------------------ agents

    /// Create an agent record (called on enrollment) and return its id. The
    /// caller is responsible for upserting the matching `computers` row.
    pub fn create_agent(&self, id: &str, key: &str, req: &EnrollReq, address: Option<&str>) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO agents(id,computer,agent_key,hostname,os,os_version,version,address,enrolled_at,last_checkin)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)",
                params![id, req.hostname, key, req.hostname, req.os, req.os_version, req.agent_version, address, now()],
            )
        })?;
        Ok(())
    }

    /// If an agent already exists for this computer name, return its id so the
    /// agent re-enrolls into the same record instead of piling up duplicates.
    pub fn agent_id_for_computer(&self, computer: &str) -> Result<Option<String>> {
        self.with(|c| {
            c.query_row("SELECT id FROM agents WHERE computer=?1 COLLATE NOCASE", [computer], |r| r.get(0))
                .optional()
        })
    }

    /// Re-enroll: refresh the key and facts of an existing agent record.
    pub fn reenroll_agent(&self, id: &str, key: &str, req: &EnrollReq, address: Option<&str>) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE agents SET agent_key=?2, hostname=?3, os=?4, os_version=?5, version=?6, address=?7, last_checkin=?8 WHERE id=?1",
                params![id, key, req.hostname, req.os, req.os_version, req.agent_version, address, now()],
            )
        })?;
        Ok(())
    }

    /// The stored key for an agent, for authenticating its requests.
    pub fn agent_key(&self, id: &str) -> Result<Option<String>> {
        self.with(|c| {
            c.query_row("SELECT agent_key FROM agents WHERE id=?1", [id], |r| r.get(0)).optional()
        })
    }

    pub fn get_agent(&self, id: &str) -> Result<Option<Agent>> {
        self.with(|c| {
            c.query_row("SELECT * FROM agents WHERE id=?1", [id], Agent::from_row).optional()
        })
    }

    pub fn list_agents(&self) -> Result<Vec<Agent>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM agents ORDER BY computer COLLATE NOCASE")?;
            st.query_map([], Agent::from_row)?.collect()
        })
    }

    pub fn delete_agent(&self, id: &str) -> Result<()> {
        self.with(|c| c.execute("DELETE FROM agents WHERE id=?1", [id]))?;
        Ok(())
    }

    /// Record a check-in: bump `last_checkin`, store the latest metrics snapshot
    /// on the agent row and append a history point.
    pub fn agent_checkin(&self, id: &str, version: &str, metrics: Option<&Metrics>) -> Result<()> {
        self.with(|c| {
            match metrics {
                Some(m) => {
                    let disks = serde_json::to_string(&m.disks).unwrap_or_else(|_| "[]".into());
                    // SQLite integers are i64; u64 metrics are cast on the way in.
                    let (used, total, uptime) = (m.mem_used as i64, m.mem_total as i64, m.uptime_secs as i64);
                    c.execute(
                        "UPDATE agents SET last_checkin=?2, version=CASE WHEN ?3<>'' THEN ?3 ELSE version END,
                            cpu_pct=?4, mem_used=?5, mem_total=?6, uptime_secs=?7, processes=?8, disks=?9 WHERE id=?1",
                        params![id, now(), version, m.cpu_pct, used, total, uptime, m.processes, disks],
                    )?;
                    c.execute(
                        "INSERT INTO agent_metrics(agent_id,ts,cpu_pct,mem_used,mem_total,disks) VALUES(?1,?2,?3,?4,?5,?6)",
                        params![id, now(), m.cpu_pct, used, total, disks],
                    )?;
                }
                None => {
                    c.execute(
                        "UPDATE agents SET last_checkin=?2, version=CASE WHEN ?3<>'' THEN ?3 ELSE version END WHERE id=?1",
                        params![id, now(), version],
                    )?;
                }
            }
            Ok(())
        })
    }

    /// Keep at most `keep` metric points per agent (called periodically).
    pub fn prune_agent_metrics(&self, keep: usize) -> Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM agent_metrics WHERE id IN (
                    SELECT id FROM (
                        SELECT id, ROW_NUMBER() OVER (PARTITION BY agent_id ORDER BY ts DESC) AS rn FROM agent_metrics
                    ) WHERE rn > ?1
                )",
                [keep as i64],
            )
        })?;
        Ok(())
    }

    pub fn agent_metrics_history(&self, id: &str, limit: usize) -> Result<Vec<MetricPoint>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT ts,cpu_pct,mem_used,mem_total,disks FROM agent_metrics WHERE agent_id=?1 ORDER BY ts DESC LIMIT ?2",
            )?;
            let mut v: Vec<MetricPoint> = st.query_map(params![id, limit as i64], MetricPoint::from_row)?.collect::<rusqlite::Result<_>>()?;
            v.reverse();
            Ok(v)
        })
    }

    // -------------------------------------------------------------- agent jobs

    pub fn queue_job(&self, agent_id: &str, kind: &str, params: &serde_json::Value, source: &str) -> Result<i64> {
        let p = serde_json::to_string(params)?;
        self.with(|c| {
            c.execute(
                "INSERT INTO agent_jobs(agent_id,kind,params,source,created_at) VALUES(?1,?2,?3,?4,?5)",
                params![agent_id, kind, p, source, now()],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    /// Hand the agent its queued jobs, marking them running.
    pub fn claim_jobs(&self, agent_id: &str) -> Result<Vec<Job>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT id,kind,params FROM agent_jobs WHERE agent_id=?1 AND status='queued' ORDER BY id")?;
            let jobs: Vec<Job> = st
                .query_map([agent_id], |r| {
                    Ok(Job {
                        id: r.get(0)?,
                        kind: r.get(1)?,
                        params: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or(serde_json::Value::Null),
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            for j in &jobs {
                c.execute(
                    "UPDATE agent_jobs SET status='running', started_at=?2 WHERE id=?1",
                    params![j.id, now()],
                )?;
            }
            Ok(jobs)
        })
    }

    pub fn complete_job(&self, r: &JobResult) -> Result<()> {
        let data = serde_json::to_string(&r.data).unwrap_or_else(|_| "null".into());
        self.with(|c| {
            c.execute(
                "UPDATE agent_jobs SET status=?2, output=?3, data=?4, finished_at=?5 WHERE id=?1",
                params![r.job_id, r.status, r.output, data, now()],
            )
        })?;
        Ok(())
    }

    pub fn list_jobs(&self, agent_id: &str, limit: usize) -> Result<Vec<AgentJob>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM agent_jobs WHERE agent_id=?1 ORDER BY id DESC LIMIT ?2")?;
            st.query_map(params![agent_id, limit as i64], AgentJob::from_row)?.collect()
        })
    }

    pub fn get_job(&self, id: i64) -> Result<Option<AgentJob>> {
        self.with(|c| {
            c.query_row("SELECT * FROM agent_jobs WHERE id=?1", [id], AgentJob::from_row).optional()
        })
    }

    /// Jobs left running when the console restarted are marked failed.
    pub fn fail_interrupted_jobs(&self) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE agent_jobs SET status='failed', output='console restarted before the agent returned a result', finished_at=?1 WHERE status='running'",
                [now()],
            )
        })?;
        Ok(())
    }

    // --------------------------------------------------------- agent inventory

    pub fn set_agent_inventory(&self, agent_id: &str, kind: &str, data: &serde_json::Value) -> Result<()> {
        let d = serde_json::to_string(data)?;
        self.with(|c| {
            c.execute(
                "INSERT INTO agent_inventory(agent_id,kind,data,synced_at) VALUES(?1,?2,?3,?4)
                 ON CONFLICT(agent_id,kind) DO UPDATE SET data=excluded.data, synced_at=excluded.synced_at",
                params![agent_id, kind, d, now()],
            )
        })?;
        Ok(())
    }

    pub fn get_agent_inventory(&self, agent_id: &str, kind: &str) -> Result<Option<(serde_json::Value, String)>> {
        let row: Option<(String, String)> = self.with(|c| {
            c.query_row(
                "SELECT data, synced_at FROM agent_inventory WHERE agent_id=?1 AND kind=?2",
                params![agent_id, kind],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
        })?;
        Ok(row.map(|(d, ts)| (serde_json::from_str(&d).unwrap_or(serde_json::Value::Null), ts)))
    }

    // --------------------------------------------------------------- inventory

    /// Store a successful scan, replacing the computer's previous inventory.
    /// The OS description is filled in when AD did not provide one.
    pub fn save_inventory(&self, computer_id: i64, scan: &Scan) -> Result<()> {
        let facts = serde_json::to_string(&scan.facts)?;
        let disks = serde_json::to_string(&scan.disks)?;
        let os_detail = scan.facts.get("os_name").cloned();
        self.with(|c| {
            let tx = c.unchecked_transaction()?;
            tx.execute(
                "INSERT INTO inventory(computer_id,scanned_at,last_attempt,status,error,facts,disks) VALUES(?1,?2,?2,'ok',NULL,?3,?4)
                 ON CONFLICT(computer_id) DO UPDATE SET scanned_at=?2, last_attempt=?2, status='ok', error=NULL, facts=?3, disks=?4",
                params![computer_id, now(), facts, disks],
            )?;
            tx.execute("DELETE FROM inventory_apps WHERE computer_id=?1", [computer_id])?;
            {
                let mut st = tx.prepare("INSERT INTO inventory_apps(computer_id,name,version,publisher) VALUES(?1,?2,?3,?4)")?;
                for a in &scan.apps {
                    st.execute(params![computer_id, a.name, a.version, a.publisher])?;
                }
            }
            tx.execute(
                "UPDATE computers SET online=1, last_check=?2, os_detail=COALESCE(NULLIF(os_detail,''), ?3) WHERE id=?1",
                params![computer_id, now(), os_detail],
            )?;
            tx.commit()
        })?;
        Ok(())
    }

    /// Record a failed or offline scan; the last good inventory is kept.
    pub fn inventory_failed(&self, computer_id: i64, status: &str, error: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO inventory(computer_id,last_attempt,status,error) VALUES(?1,?2,?3,?4)
                 ON CONFLICT(computer_id) DO UPDATE SET last_attempt=?2, status=?3, error=?4",
                params![computer_id, now(), status, error],
            )
        })?;
        Ok(())
    }

    /// Inventory of every scanned computer; `with_apps` also loads the software lists.
    pub fn list_inventory(&self, with_apps: bool) -> Result<Vec<Inventory>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("{INVENTORY_SELECT} ORDER BY c.name COLLATE NOCASE"))?;
            let mut list: Vec<Inventory> = st.query_map([], Inventory::from_row)?.collect::<rusqlite::Result<_>>()?;
            if with_apps {
                let mut by_id: HashMap<i64, usize> = list.iter().enumerate().map(|(i, v)| (v.computer_id, i)).collect();
                let mut st = c.prepare("SELECT computer_id,name,version,publisher FROM inventory_apps ORDER BY name COLLATE NOCASE")?;
                let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, InstalledApp::from_row(r, 1)?)))?;
                for row in rows {
                    let (id, app) = row?;
                    if let Some(i) = by_id.get_mut(&id) {
                        list[*i].apps.push(app);
                    }
                }
            }
            Ok(list)
        })
    }

    pub fn get_inventory(&self, computer: &str) -> Result<Option<Inventory>> {
        self.with(|c| {
            let inv = c
                .query_row(&format!("{INVENTORY_SELECT} WHERE c.name=?1"), [computer], Inventory::from_row)
                .optional()?;
            let Some(mut inv) = inv else { return Ok(None) };
            let mut st = c.prepare(
                "SELECT name,version,publisher FROM inventory_apps WHERE computer_id=?1 ORDER BY name COLLATE NOCASE",
            )?;
            inv.apps = st.query_map([inv.computer_id], |r| InstalledApp::from_row(r, 0))?.collect::<rusqlite::Result<_>>()?;
            Ok(Some(inv))
        })
    }

    /// Installed software across all computers: (name, version, publisher, computer count).
    pub fn software_summary(&self) -> Result<Vec<(String, String, String, i64)>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT name, version, MAX(publisher), COUNT(DISTINCT computer_id) FROM inventory_apps
                 GROUP BY name COLLATE NOCASE, version ORDER BY name COLLATE NOCASE",
            )?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect()
        })
    }

    /// Computers with software `name` installed: (computer, version).
    pub fn software_computers(&self, name: &str) -> Result<Vec<(String, String)>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT c.name, a.version FROM inventory_apps a JOIN computers c ON c.id=a.computer_id
                 WHERE a.name=?1 COLLATE NOCASE ORDER BY c.name COLLATE NOCASE",
            )?;
            st.query_map([name], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
        })
    }
}

fn json_col<T: DeserializeOwned + Default>(row: &Row, col: &str) -> rusqlite::Result<T> {
    let s: String = row.get(col)?;
    Ok(serde_json::from_str(&s).unwrap_or_default())
}

pub fn split_tags(s: &str) -> Vec<String> {
    s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect()
}

fn join_tags(t: &[String]) -> String {
    let mut v: Vec<String> = t.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    v.sort();
    v.dedup();
    v.join(",")
}

// ------------------------------------------------------------------- models

#[derive(Debug, Clone, Serialize)]
pub struct Computer {
    pub id: i64,
    pub name: String,
    pub address: Option<String>,
    /// "windows", "linux" or "unknown"
    pub os: String,
    pub os_detail: Option<String>,
    pub source: String,
    pub ad_ou: Option<String>,
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub credential_id: Option<i64>,
    pub port: Option<u16>,
    pub last_logon: Option<String>,
    pub online: Option<bool>,
    pub last_check: Option<String>,
    /// Names of the AD security groups the computer is a member of.
    pub ad_groups: Vec<String>,
}

impl Computer {
    fn from_row(r: &Row) -> rusqlite::Result<Computer> {
        Ok(Computer {
            id: r.get("id")?,
            name: r.get("name")?,
            address: r.get("address")?,
            os: r.get("os")?,
            os_detail: r.get("os_detail")?,
            source: r.get("source")?,
            ad_ou: r.get("ad_ou")?,
            description: r.get("description")?,
            tags: split_tags(&r.get::<_, String>("tags")?),
            credential_id: r.get("credential_id")?,
            port: r.get("port")?,
            last_logon: r.get("last_logon")?,
            online: r.get("online")?,
            last_check: r.get("last_check")?,
            ad_groups: r.get::<_, String>("ad_groups")?.lines().filter(|l| !l.is_empty()).map(String::from).collect(),
        })
    }

    /// Host name or address to connect to.
    pub fn connect_host(&self) -> &str {
        self.address.as_deref().filter(|a| !a.is_empty()).unwrap_or(&self.name)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct NewComputer {
    pub name: String,
    pub address: Option<String>,
    pub os: Option<String>,
    pub os_detail: Option<String>,
    pub source: Option<String>,
    pub ad_ou: Option<String>,
    pub description: Option<String>,
    pub tags: Option<Vec<String>>,
    pub credential_id: Option<i64>,
    pub port: Option<u16>,
    pub last_logon: Option<String>,
    pub ad_groups: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct ComputerUpdate {
    pub address: Option<String>,
    pub os: String,
    pub description: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub credential_id: Option<i64>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Credential {
    pub id: i64,
    pub name: String,
    /// "windows" (WinRM), "ssh" or "ldap"
    pub kind: String,
    pub username: String,
    pub has_secret: bool,
    pub key_path: Option<String>,
    pub default_for: Option<String>,
}

impl Credential {
    fn from_row(r: &Row) -> rusqlite::Result<Credential> {
        Ok(Credential {
            id: r.get("id")?,
            name: r.get("name")?,
            kind: r.get("kind")?,
            username: r.get("username")?,
            has_secret: r.get::<_, Option<String>>("secret")?.is_some(),
            key_path: r.get("key_path")?,
            default_for: r.get("default_for")?,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct NewCredential {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub username: String,
    pub secret: Option<String>,
    pub key_path: Option<String>,
    pub default_for: Option<String>,
}

/// Which computers a deployment or schedule targets. Tags are resolved when
/// the deployment starts, so new AD computers are picked up automatically.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetSpec {
    #[serde(default)]
    pub computers: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    /// Computer group names.
    #[serde(default)]
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployOptions {
    /// Targets worked on in parallel.
    #[serde(default = "default_concurrency")]
    pub concurrency: usize,
    /// Rolling deployment: targets per batch (0 = no batching).
    #[serde(default)]
    pub batch_size: usize,
    /// Stop starting new targets after this many failures.
    #[serde(default)]
    pub max_failures: Option<usize>,
    /// Skip targets that already have the package (run `detect`). Default true.
    #[serde(default = "yes")]
    pub use_detect: bool,
}

fn default_concurrency() -> usize {
    10
}
fn yes() -> bool {
    true
}

impl Default for DeployOptions {
    fn default() -> Self {
        DeployOptions { concurrency: 10, batch_size: 0, max_failures: None, use_detect: true }
    }
}

pub struct NewDeployment {
    pub id: String,
    pub package_id: String,
    pub package_name: String,
    pub package_yaml: String,
    pub variables: BTreeMap<String, String>,
    pub targets: TargetSpec,
    pub options: DeployOptions,
    pub source: String,
    pub scheduled_for: Option<String>,
}

const DEPLOYMENT_SELECT: &str = "SELECT d.*,
    (SELECT COUNT(*) FROM deployment_targets t WHERE t.deployment_id=d.id) AS n_total,
    (SELECT COUNT(*) FROM deployment_targets t WHERE t.deployment_id=d.id AND t.status='success') AS n_success,
    (SELECT COUNT(*) FROM deployment_targets t WHERE t.deployment_id=d.id AND t.status='skipped') AS n_skipped,
    (SELECT COUNT(*) FROM deployment_targets t WHERE t.deployment_id=d.id AND t.status IN ('failed','offline')) AS n_failed
    FROM deployments d";

#[derive(Debug, Clone, Serialize)]
pub struct Deployment {
    pub id: String,
    pub package_id: String,
    pub package_name: String,
    #[serde(skip)]
    pub package_yaml: String,
    pub variables: BTreeMap<String, String>,
    pub targets: TargetSpec,
    pub options: DeployOptions,
    pub status: String,
    pub source: String,
    pub created_at: String,
    pub scheduled_for: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub n_total: i64,
    pub n_success: i64,
    pub n_skipped: i64,
    pub n_failed: i64,
}

impl Deployment {
    fn from_row(r: &Row) -> rusqlite::Result<Deployment> {
        Ok(Deployment {
            id: r.get("id")?,
            package_id: r.get("package_id")?,
            package_name: r.get("package_name")?,
            package_yaml: r.get("package_yaml")?,
            variables: json_col(r, "variables")?,
            targets: json_col(r, "targets")?,
            options: json_col(r, "options")?,
            status: r.get("status")?,
            source: r.get("source")?,
            created_at: r.get("created_at")?,
            scheduled_for: r.get("scheduled_for")?,
            started_at: r.get("started_at")?,
            finished_at: r.get("finished_at")?,
            n_total: r.get("n_total")?,
            n_success: r.get("n_success")?,
            n_skipped: r.get("n_skipped")?,
            n_failed: r.get("n_failed")?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetRow {
    pub id: i64,
    pub deployment_id: String,
    pub computer: String,
    pub os: String,
    /// pending, running, success, skipped, failed, offline, cancelled
    pub status: String,
    pub current_step: Option<String>,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub log: String,
}

impl TargetRow {
    fn from_row(r: &Row) -> rusqlite::Result<TargetRow> {
        Ok(TargetRow {
            id: r.get(0)?,
            deployment_id: r.get(1)?,
            computer: r.get(2)?,
            os: r.get(3)?,
            status: r.get(4)?,
            current_step: r.get(5)?,
            error: r.get(6)?,
            started_at: r.get(7)?,
            finished_at: r.get(8)?,
            log: r.get(9)?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub package_id: String,
    #[serde(default)]
    pub targets: TargetSpec,
    #[serde(default)]
    pub variables: BTreeMap<String, String>,
    #[serde(default)]
    pub options: DeployOptions,
    /// "once", "daily", "weekly" or "interval"
    pub kind: String,
    /// "HH:MM" local time (daily/weekly), or RFC 3339 / "YYYY-MM-DD HH:MM" (once)
    #[serde(default)]
    pub time: String,
    /// 0 = Monday … 6 = Sunday (weekly)
    #[serde(default)]
    pub weekdays: Vec<u32>,
    #[serde(default)]
    pub interval_minutes: i64,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub last_run: Option<String>,
    #[serde(default)]
    pub next_run: Option<String>,
}

impl Schedule {
    fn from_row(r: &Row) -> rusqlite::Result<Schedule> {
        Ok(Schedule {
            id: r.get("id")?,
            name: r.get("name")?,
            package_id: r.get("package_id")?,
            targets: json_col(r, "targets")?,
            variables: json_col(r, "variables")?,
            options: json_col(r, "options")?,
            kind: r.get("kind")?,
            time: r.get("time")?,
            weekdays: json_col(r, "weekdays")?,
            interval_minutes: r.get("interval_minutes")?,
            enabled: r.get("enabled")?,
            last_run: r.get("last_run")?,
            next_run: r.get("next_run")?,
        })
    }
}

pub fn not_found(what: &str) -> anyhow::Error {
    anyhow!("{what} not found")
}

/// A resident agent and its latest resource snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct Agent {
    pub id: String,
    pub computer: String,
    #[serde(skip)]
    pub agent_key: String,
    pub hostname: String,
    pub os: String,
    pub os_version: String,
    pub version: String,
    pub address: Option<String>,
    pub enrolled_at: String,
    pub last_checkin: Option<String>,
    /// True when the last check-in is recent (see [`STALE_SECS`]).
    pub online: bool,
    pub cpu_pct: Option<f32>,
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    pub uptime_secs: Option<u64>,
    pub processes: Option<u32>,
    pub disks: Vec<DiskUsage>,
}

impl Agent {
    fn from_row(r: &Row) -> rusqlite::Result<Agent> {
        let last_checkin: Option<String> = r.get("last_checkin")?;
        let online = last_checkin
            .as_deref()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .is_some_and(|t| (Utc::now() - t.with_timezone(&Utc)).num_seconds() < STALE_SECS);
        Ok(Agent {
            id: r.get("id")?,
            computer: r.get("computer")?,
            agent_key: r.get("agent_key")?,
            hostname: r.get("hostname")?,
            os: r.get("os")?,
            os_version: r.get("os_version")?,
            version: r.get("version")?,
            address: r.get("address")?,
            enrolled_at: r.get("enrolled_at")?,
            last_checkin,
            online,
            cpu_pct: r.get("cpu_pct")?,
            mem_used: r.get::<_, Option<i64>>("mem_used")?.map(|v| v as u64),
            mem_total: r.get::<_, Option<i64>>("mem_total")?.map(|v| v as u64),
            uptime_secs: r.get::<_, Option<i64>>("uptime_secs")?.map(|v| v as u64),
            processes: r.get::<_, Option<i64>>("processes")?.map(|v| v as u32),
            disks: serde_json::from_str(&r.get::<_, String>("disks")?).unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentJob {
    pub id: i64,
    pub agent_id: String,
    pub kind: String,
    pub params: serde_json::Value,
    pub status: String,
    pub output: String,
    pub data: serde_json::Value,
    pub source: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

impl AgentJob {
    fn from_row(r: &Row) -> rusqlite::Result<AgentJob> {
        Ok(AgentJob {
            id: r.get("id")?,
            agent_id: r.get("agent_id")?,
            kind: r.get("kind")?,
            params: serde_json::from_str(&r.get::<_, String>("params")?).unwrap_or(serde_json::Value::Null),
            status: r.get("status")?,
            output: r.get("output")?,
            data: serde_json::from_str(&r.get::<_, String>("data")?).unwrap_or(serde_json::Value::Null),
            source: r.get("source")?,
            created_at: r.get("created_at")?,
            started_at: r.get("started_at")?,
            finished_at: r.get("finished_at")?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MetricPoint {
    pub ts: String,
    pub cpu_pct: Option<f32>,
    pub mem_used: Option<u64>,
    pub mem_total: Option<u64>,
    pub disks: Vec<DiskUsage>,
}

impl MetricPoint {
    fn from_row(r: &Row) -> rusqlite::Result<MetricPoint> {
        Ok(MetricPoint {
            ts: r.get(0)?,
            cpu_pct: r.get(1)?,
            mem_used: r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
            mem_total: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
            disks: serde_json::from_str(&r.get::<_, String>(4)?).unwrap_or_default(),
        })
    }
}

/// A named set of computers: either a fixed member list ("static") or
/// everything matching `rules` ("dynamic", re-evaluated whenever used).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// "static" or "dynamic"
    #[serde(default = "static_kind")]
    pub kind: String,
    #[serde(default)]
    pub rules: GroupRules,
    /// Static members (computer names); filled when listing.
    #[serde(default)]
    pub members: Vec<String>,
}

fn static_kind() -> String {
    "static".into()
}

impl Group {
    fn from_row(r: &Row) -> rusqlite::Result<Group> {
        Ok(Group {
            id: r.get("id")?,
            name: r.get("name")?,
            description: r.get("description")?,
            kind: r.get("kind")?,
            rules: json_col(r, "rules")?,
            members: vec![],
        })
    }
}

/// Conditions of a dynamic group; all filled-in conditions must match.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupRules {
    /// "windows" or "linux"
    #[serde(default)]
    pub os: Option<String>,
    /// Name patterns with * and ?, comma separated (any may match), e.g. "web*, app0?"
    #[serde(default)]
    pub name: String,
    /// Text the computer's AD OU path must contain, e.g. "OU=Servers"
    #[serde(default)]
    pub ou: String,
    /// Computers with any of these tags
    #[serde(default)]
    pub tags: Vec<String>,
    /// Member of this AD security group (name)
    #[serde(default)]
    pub ad_group: String,
    /// Text the OS description must contain, e.g. "Server 2022"
    #[serde(default)]
    pub os_detail: String,
    /// Installed software name pattern with * and ? (from inventory), e.g. "Google Chrome*"
    #[serde(default)]
    pub app: String,
    /// Version condition for `app`, e.g. "<130" or ">=22.11" (empty = any version)
    #[serde(default)]
    pub app_version: String,
    /// Match scanned computers that do NOT have `app` (at that version)
    #[serde(default)]
    pub app_missing: bool,
    /// Inventory reports a pending reboot
    #[serde(default)]
    pub pending_reboot: bool,
    /// A fixed disk has less than this many GB free (0 = off)
    #[serde(default)]
    pub low_disk_gb: f64,
}

/// Result of one inventory scan, as parsed from the scan script's output.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scan {
    pub facts: BTreeMap<String, String>,
    pub disks: Vec<Disk>,
    pub apps: Vec<InstalledApp>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Disk {
    /// Drive letter ("C:") or mount point ("/var")
    pub mount: String,
    pub size: u64,
    pub free: u64,
    #[serde(default)]
    pub fs: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstalledApp {
    pub name: String,
    pub version: String,
    pub publisher: String,
}

impl InstalledApp {
    fn from_row(r: &Row, first: usize) -> rusqlite::Result<InstalledApp> {
        Ok(InstalledApp { name: r.get(first)?, version: r.get(first + 1)?, publisher: r.get(first + 2)? })
    }
}

const INVENTORY_SELECT: &str = "SELECT i.*, c.name AS computer,
    (SELECT COUNT(*) FROM inventory_apps a WHERE a.computer_id=i.computer_id) AS app_count
    FROM inventory i JOIN computers c ON c.id=i.computer_id";

/// The stored inventory of one computer.
#[derive(Debug, Clone, Serialize)]
pub struct Inventory {
    pub computer_id: i64,
    pub computer: String,
    /// Time of the last successful scan (None = never succeeded).
    pub scanned_at: Option<String>,
    pub last_attempt: String,
    /// "ok", "failed" or "offline" (of the last attempt)
    pub status: String,
    pub error: Option<String>,
    pub facts: BTreeMap<String, String>,
    pub disks: Vec<Disk>,
    pub app_count: i64,
    /// Filled only when asked for.
    pub apps: Vec<InstalledApp>,
}

impl Inventory {
    fn from_row(r: &Row) -> rusqlite::Result<Inventory> {
        Ok(Inventory {
            computer_id: r.get("computer_id")?,
            computer: r.get("computer")?,
            scanned_at: r.get("scanned_at")?,
            last_attempt: r.get("last_attempt")?,
            status: r.get("status")?,
            error: r.get("error")?,
            facts: json_col(r, "facts")?,
            disks: json_col(r, "disks")?,
            app_count: r.get("app_count")?,
            apps: vec![],
        })
    }
}
