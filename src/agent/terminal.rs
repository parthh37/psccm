//! Interactive web terminal.
//!
//! Console side ([`Hub`]): a browser WebSocket opens a session; the console
//! queues control + stdin for the agent and buffers the agent's stdout for the
//! browser. The agent picks up the control messages on its check-in (fast-poll
//! while a session is open) and streams output back.
//!
//! Target side ([`shell`]): the agent spawns a child shell with piped stdio and
//! streams bytes, optionally under a chosen identity (SYSTEM, an impersonated
//! logged-on user, or a password runas). See [`shell`] for the platform code.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use super::{RunAs, TerminalChunk, TerminalCtl};

pub mod shell;

/// Most terminal output held for a browser (oldest output is dropped first).
const MAX_BUFFERED: usize = 1 << 20;

/// Per-agent wakeups and the set of live terminal sessions.
#[derive(Default)]
pub struct Hub {
    /// agent_id -> notifier that wakes that agent's held check-in.
    notifiers: Mutex<HashMap<String, Arc<Notify>>>,
    /// session_id -> live session state.
    sessions: Mutex<HashMap<String, Session>>,
}

struct Session {
    agent_id: String,
    /// Control messages (open / input / close) waiting for the agent.
    to_agent: VecDeque<TerminalCtl>,
    /// Output lines waiting for the browser.
    to_browser: VecDeque<String>,
    /// Woken when new output (or a close) arrives for the browser.
    browser_wake: Arc<Notify>,
    /// False once the agent (or the browser) has closed it.
    open: bool,
}

impl Hub {
    /// The notifier for an agent, creating it on first use.
    pub fn notifier(&self, agent_id: &str) -> Arc<Notify> {
        let mut n = self.notifiers.lock().unwrap();
        n.entry(agent_id.to_string()).or_default().clone()
    }

    /// Wake an agent's held check-in (a job was queued or input is pending).
    pub fn wake(&self, agent_id: &str) {
        self.notifier(agent_id).notify_one();
    }

    // --------------------------------------------------------- browser side

    /// Start a session and tell the agent to open a shell. Returns the id.
    pub fn open_session(&self, agent_id: &str, shell: &str, run_as: RunAs) -> String {
        let session_id = uuid::Uuid::new_v4().simple().to_string();
        let session = Session {
            agent_id: agent_id.to_string(),
            to_agent: VecDeque::from([TerminalCtl {
                session_id: session_id.clone(),
                action: "open".into(),
                shell: Some(shell.to_string()),
                run_as: Some(run_as),
                data: String::new(),
            }]),
            to_browser: VecDeque::new(),
            browser_wake: Arc::new(Notify::new()),
            open: true,
        };
        self.sessions.lock().unwrap().insert(session_id.clone(), session);
        self.wake(agent_id);
        session_id
    }

    /// Queue keystrokes for the agent's shell.
    pub fn send_input(&self, session_id: &str, data: String) -> bool {
        let agent_id = {
            let mut s = self.sessions.lock().unwrap();
            let Some(sess) = s.get_mut(session_id) else { return false };
            sess.to_agent.push_back(TerminalCtl {
                session_id: session_id.to_string(),
                action: "input".into(),
                shell: None,
                run_as: None,
                data,
            });
            sess.agent_id.clone()
        };
        self.wake(&agent_id);
        true
    }

    /// Ask the agent to close the shell and drop the session.
    pub fn close_session(&self, session_id: &str) {
        let agent_id = {
            let mut s = self.sessions.lock().unwrap();
            let Some(sess) = s.get_mut(session_id) else { return };
            sess.open = false;
            sess.to_agent.push_back(TerminalCtl {
                session_id: session_id.to_string(),
                action: "close".into(),
                shell: None,
                run_as: None,
                data: String::new(),
            });
            sess.browser_wake.notify_waiters();
            sess.agent_id.clone()
        };
        self.wake(&agent_id);
    }

    /// Notifier the browser task waits on for new output.
    pub fn browser_wake(&self, session_id: &str) -> Option<Arc<Notify>> {
        self.sessions.lock().unwrap().get(session_id).map(|s| s.browser_wake.clone())
    }

    /// Drain buffered output for the browser; `bool` is whether the session is still open.
    pub fn take_browser_output(&self, session_id: &str) -> (Vec<String>, bool) {
        let mut s = self.sessions.lock().unwrap();
        match s.get_mut(session_id) {
            Some(sess) => (sess.to_browser.drain(..).collect(), sess.open),
            None => (Vec::new(), false),
        }
    }

    /// Forget a session entirely (browser disconnected and it is closed).
    pub fn drop_session(&self, session_id: &str) {
        self.sessions.lock().unwrap().remove(session_id);
    }

    // ----------------------------------------------------------- agent side

    /// Whether the agent has any open terminal session (drives fast-poll).
    pub fn agent_has_terminal(&self, agent_id: &str) -> bool {
        self.sessions.lock().unwrap().values().any(|s| s.agent_id == agent_id && s.open)
    }

    /// Control messages the agent should act on now (drained).
    pub fn take_agent_ctls(&self, agent_id: &str) -> Vec<TerminalCtl> {
        let mut out = Vec::new();
        let mut s = self.sessions.lock().unwrap();
        for sess in s.values_mut().filter(|s| s.agent_id == agent_id) {
            out.extend(sess.to_agent.drain(..));
        }
        out
    }

    /// Output (or a close) the agent reported for a session. Chunks for a
    /// session that belongs to another agent are ignored.
    pub fn push_output(&self, agent_id: &str, chunk: &TerminalChunk) {
        let mut s = self.sessions.lock().unwrap();
        if let Some(sess) = s.get_mut(&chunk.session_id).filter(|s| s.agent_id == agent_id) {
            if !chunk.data.is_empty() {
                sess.to_browser.push_back(chunk.data.clone());
                // A browser that stopped reading must not grow memory forever.
                let mut total: usize = sess.to_browser.iter().map(String::len).sum();
                while total > MAX_BUFFERED && sess.to_browser.len() > 1 {
                    total -= sess.to_browser.pop_front().map_or(0, |c| c.len());
                }
            }
            if chunk.closed {
                sess.open = false;
            }
            sess.browser_wake.notify_waiters();
        }
    }
}
