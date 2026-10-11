//! The folder monitor, from the side of whoever opens the database.
//!
//! `reldir serve` watches the folder and keeps a numbered log of every path
//! that changed. A process opening the database asks it which paths changed
//! since the token its mirror last stored, and reads those alone instead of
//! examining every file -- git's fsmonitor protocol. Before answering, the
//! monitor sees a cookie file the asker writes, so every change the asker
//! could have made, or seen, before asking is in the answer.
//!
//! The monitor is an accelerator, never an authority on its own: no monitor,
//! a monitor that does not answer, a token from another of its runs, or a
//! log it had to cut short all mean the folder is examined whole, as it is
//! without one.

use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    time::Duration,
};

/// Where the monitor says how to reach it.
pub fn handle_path(root: &Path) -> PathBuf {
    root.join(".db").join("serve.json")
}

/// The prefix of the cookie files askers write, under `.db/cache/`.
pub const COOKIE_PREFIX: &str = "monitor-cookie-";

/// How to reach a running monitor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handle {
    pub port: u16,
    /// Shown by every request: only a process that can read `.db/` may ask.
    pub secret: String,
    pub pid: u32,
    /// Names one run of the monitor; a token from another run means nothing.
    pub epoch: String,
}

/// A point in a monitor's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    pub epoch: String,
    pub token: u64,
}

impl Mark {
    /// The mark as the mirror keeps it.
    pub fn encode(&self) -> String {
        format!("{} {}", self.epoch, self.token)
    }

    pub fn decode(text: &str) -> Option<Self> {
        let (epoch, token) = text.split_once(' ')?;
        Some(Self {
            epoch: epoch.to_string(),
            token: token.parse().ok()?,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub secret: String,
    pub since: Option<Mark>,
    /// The cookie file's name: the monitor answers once it has seen it.
    pub cookie: String,
}

/// What changed since a mark.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Changes {
    /// Where the log stands now: the mark to ask from next time.
    pub now: Mark,
    /// The log cannot say what changed since the mark asked about: examine
    /// everything.
    pub whole: bool,
    /// Paths that changed, relative to the root, with `/` separators.
    pub paths: Vec<String>,
}

/// The running monitor of the database at `root`, if one says it runs.
pub fn find(root: &Path) -> Option<Handle> {
    let text = std::fs::read(handle_path(root)).ok()?;
    serde_json::from_slice(&text).ok()
}

impl Handle {
    /// What changed since `since`: an error when the monitor cannot be
    /// asked, which its absence explains.
    pub fn changes(&self, root: &Path, since: Option<Mark>) -> std::io::Result<Changes> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let cookie = format!(
            "{COOKIE_PREFIX}{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let cookie_path = root.join(".db").join("cache").join(&cookie);
        let address = SocketAddr::from(([127, 0, 0, 1], self.port));
        let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(250))?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        // Written after connecting, so a monitor that is gone costs nothing
        // but the refused connection.
        std::fs::write(&cookie_path, b"")?;
        let answer = (|| -> std::io::Result<Changes> {
            let request = Request {
                secret: self.secret.clone(),
                since,
                cookie,
            };
            let mut line = serde_json::to_vec(&request).map_err(std::io::Error::other)?;
            line.push(b'\n');
            stream.write_all(&line)?;
            let mut reply = String::new();
            BufReader::new(&stream).read_line(&mut reply)?;
            serde_json::from_str(&reply).map_err(std::io::Error::other)
        })();
        let _ = std::fs::remove_file(&cookie_path);
        answer
    }
}
