//! Linux targets over SSH (pure Rust client, password or key auth).
//!
//! Scripts are copied to /tmp and run through `sudo` unless the login user
//! is root. Host keys are pinned on first use (TOFU) in the database.

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use russh::client::{self, Handle};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};
use russh::{ChannelMsg, Disconnect};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncRead;

use super::{LineBuffer, LogFn, Transport, sh_quote};
use crate::db::Db;
use crate::package::Shell;

pub struct Ssh {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub key_path: Option<String>,
    pub db: Db,
    session: tokio::sync::Mutex<Option<Handle<HostKeyCheck>>>,
}

pub struct HostKeyCheck {
    expected: Option<String>,
    seen: Arc<Mutex<Option<String>>>,
}

impl client::Handler for HostKeyCheck {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let fp = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256),
            PublicKeyOrCertificate::Certificate(c) => c.public_key().fingerprint(HashAlg::Sha256),
        }
        .to_string();
        *self.seen.lock().unwrap() = Some(fp.clone());
        Ok(self.expected.as_ref().is_none_or(|e| *e == fp))
    }
}

impl Ssh {
    pub fn new(
        host: String,
        port: u16,
        user: String,
        password: Option<String>,
        key_path: Option<String>,
        db: Db,
    ) -> Ssh {
        Ssh { host, port, user, password, key_path, db, session: tokio::sync::Mutex::new(None) }
    }

    fn host_id(&self) -> String {
        format!("{}:{}", self.host.to_ascii_lowercase(), self.port)
    }

    async fn connect(&self) -> Result<Handle<HostKeyCheck>> {
        let host_id = self.host_id();
        let expected = self.db.known_host(&host_id)?;
        let seen = Arc::new(Mutex::new(None));
        let handler = HostKeyCheck { expected: expected.clone(), seen: seen.clone() };
        let config = Arc::new(client::Config {
            inactivity_timeout: Some(Duration::from_secs(3600)),
            keepalive_interval: Some(Duration::from_secs(30)),
            ..Default::default()
        });
        let connecting = client::connect(config, (self.host.as_str(), self.port), handler);
        let result = tokio::time::timeout(Duration::from_secs(20), connecting)
            .await
            .map_err(|_| anyhow!("SSH connection to {} timed out", self.host))?;
        let seen = seen.lock().unwrap().clone();
        let mut session = match result {
            Ok(s) => s,
            Err(e) => {
                if let (Some(exp), Some(got)) = (&expected, &seen)
                    && exp != got {
                        bail!(
                            "SSH host key for {host_id} CHANGED (expected {exp}, got {got}). \
                             If the server was rebuilt, forget its key in Settings and retry."
                        );
                    }
                return Err(anyhow!("SSH connection to {} failed: {e}", self.host));
            }
        };
        if expected.is_none()
            && let Some(fp) = &seen {
                self.db.add_known_host(&host_id, fp)?;
            }

        let auth = if let Some(path) = &self.key_path {
            let key = load_secret_key(path, self.password.as_deref())
                .with_context(|| format!("loading SSH key {path}"))?;
            let hash = session.best_supported_rsa_hash().await?.flatten();
            session
                .authenticate_publickey(&self.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await?
        } else if let Some(pw) = &self.password {
            session.authenticate_password(&self.user, pw).await?
        } else {
            bail!("SSH credential for {} has neither a password nor a key", self.user);
        };
        if !auth.success() {
            bail!("SSH authentication failed for {}@{}", self.user, self.host);
        }
        Ok(session)
    }

    /// Run one command; optional `stdin` is sent then closed.
    async fn exec<R: AsyncRead + Unpin>(
        &self,
        command: &str,
        stdin: Option<R>,
        timeout: Duration,
        log: &LogFn,
    ) -> Result<i32> {
        let mut channel = {
            let mut guard = self.session.lock().await;
            if guard.as_ref().is_none_or(|h| h.is_closed()) {
                *guard = Some(self.connect().await?);
            }
            guard.as_ref().unwrap().channel_open_session().await?
        };
        channel.exec(true, command).await?;
        if let Some(data) = stdin {
            channel.data(data).await?;
        }
        channel.eof().await?;

        let mut out = LineBuffer::default();
        let mut code = None;
        let read = async {
            while let Some(msg) = channel.wait().await {
                match msg {
                    ChannelMsg::Data { data } => out.push(&data, log),
                    ChannelMsg::ExtendedData { data, .. } => out.push(&data, log),
                    ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status as i32),
                    _ => {}
                }
            }
        };
        let timed_out = tokio::time::timeout(timeout, read).await.is_err();
        out.flush(log);
        if timed_out {
            let _ = channel.close().await;
            bail!("timed out after {}s", timeout.as_secs());
        }
        code.ok_or_else(|| anyhow!("connection closed before the command finished"))
    }

    fn is_root(&self) -> bool {
        self.user == "root"
    }
}

#[async_trait]
impl Transport for Ssh {
    async fn run(&self, script: &str, shell: Shell, timeout: Duration, log: &LogFn) -> Result<i32> {
        let interp = match shell {
            Shell::Bash => "bash",
            Shell::Sh => "sh",
            other => bail!("{other:?} scripts cannot run on Linux"),
        };
        let path = format!("/tmp/psccm-{}.sh", uuid::Uuid::new_v4());
        let q = sh_quote(&path);
        let upload = format!("umask 077 && cat > {q}");
        let silent: LogFn = Arc::new(|_| {});
        if self.exec(&upload, Some(script.as_bytes()), Duration::from_secs(60), log).await? != 0 {
            bail!("could not write script to {path}");
        }

        // The script's stdin is /dev/null; sudo reads the password (if any)
        // from the channel's stdin before that.
        let (command, stdin) = if self.is_root() {
            (format!("{interp} {q} </dev/null"), None)
        } else if let Some(pw) = self.password.as_ref().filter(|_| self.key_path.is_none()) {
            (
                format!("sudo -S -p '' {interp} -c 'exec {interp} \"$0\" </dev/null' {q}"),
                Some(format!("{pw}\n")),
            )
        } else {
            (format!("sudo -n {interp} {q} </dev/null"), None)
        };
        let command = format!("{command}; rc=$?; rm -f {q}; exit $rc");
        let result = self.exec(&command, stdin.as_deref().map(str::as_bytes), timeout, log).await;
        if result.is_err() {
            let _ = self.exec(&format!("rm -f {q}"), None::<&[u8]>, Duration::from_secs(15), &silent).await;
        }
        result
    }

    async fn upload(&self, local: &Path, remote: &str, timeout: Duration, log: &LogFn) -> Result<()> {
        let file = tokio::fs::File::open(local)
            .await
            .with_context(|| format!("opening {}", local.display()))?;
        let q = sh_quote(remote);
        let cmd = format!("umask 077 && mkdir -p \"$(dirname {q})\" && cat > {q}");
        match self.exec(&cmd, Some(file), timeout, log).await? {
            0 => Ok(()),
            c => bail!("upload to {remote} failed with exit code {c}"),
        }
    }

    async fn reset(&self) {
        if let Some(h) = self.session.lock().await.take() {
            let _ = h.disconnect(Disconnect::ByApplication, "", "en").await;
        }
    }
}
