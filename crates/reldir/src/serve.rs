//! `reldir serve`: the folder monitor.
//!
//! It watches the database's folder and keeps a numbered log of the paths
//! that change. Any process that opens the database -- the CLI, the MCP
//! server, the Python and Node libraries -- asks it what changed since the
//! mark its mirror stored, and examines those paths instead of every file.
//! See `reldir_core::monitor` for the protocol.
//!
//! It listens on the loopback interface, on a port it publishes with a
//! secret in `.db/serve.json`, which only the folder's owner can read.

use crate::command::{Context, Intent};
use crate::output::{Finish, Sink};
use notify::{EventKind, RecursiveMode, Watcher};
use reldir_core::{
    code::Code,
    diagnostic::{DbError, Result},
    monitor::{COOKIE_PREFIX, Changes, Handle, Mark, Request},
};
use serde_json::{Map, json};
use std::{
    collections::{BTreeSet, HashSet, VecDeque},
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, atomic::AtomicBool, atomic::Ordering},
    time::{Duration, Instant},
};

/// The most paths the log keeps; older ones are forgotten, and a mark from
/// before them is answered "examine everything".
const LOG_LIMIT: usize = 1 << 20;

/// How long a request waits for its cookie to be seen.
const COOKIE_WAIT: Duration = Duration::from_secs(2);

/// The paths under `.db/` that are reldir's own bookkeeping: never news.
fn internal(relative: &str) -> bool {
    let Some(rest) = relative.strip_prefix(".db/") else {
        return relative == ".db";
    };
    !(rest.starts_with("schema/") || rest == "config" || rest == "format" || rest == "schema")
}

struct Log {
    epoch: String,
    /// The token of the first path the log still holds.
    base: u64,
    paths: VecDeque<String>,
    cookies: HashSet<String>,
}

impl Log {
    fn now(&self) -> Mark {
        Mark {
            epoch: self.epoch.clone(),
            token: self.base + self.paths.len() as u64,
        }
    }

    /// Start over: every mark from before means "examine everything".
    fn restart(&mut self) {
        self.epoch = new_epoch();
        self.base = 0;
        self.paths.clear();
    }

    fn push(&mut self, path: String) {
        self.paths.push_back(path);
        if self.paths.len() > LOG_LIMIT {
            let forget = self.paths.len() / 2;
            self.paths.drain(..forget);
            self.base += forget as u64;
        }
    }

    fn answer(&self, since: Option<&Mark>) -> Changes {
        let now = self.now();
        match since {
            Some(mark)
                if mark.epoch == self.epoch
                    && mark.token >= self.base
                    && mark.token <= now.token =>
            {
                let start = (mark.token - self.base) as usize;
                let paths: BTreeSet<String> = self.paths.iter().skip(start).cloned().collect();
                Changes {
                    now,
                    whole: false,
                    paths: paths.into_iter().collect(),
                }
            }
            _ => Changes {
                now,
                whole: true,
                paths: vec![],
            },
        }
    }
}

fn new_epoch() -> String {
    ulid::Ulid::new().to_string()
}

fn random_secret() -> String {
    format!("{}{}", ulid::Ulid::new(), ulid::Ulid::new())
}

/// The path relative to the root, with `/`; `None` outside it.
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rest = path.strip_prefix(root).ok()?;
    let text = rest.to_string_lossy().replace('\\', "/");
    (!text.is_empty()).then_some(text)
}

/// Set by a signal to stop serving.
static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
fn on_signals() {
    extern "C" fn stop(_: libc::c_int) {
        STOP.store(true, Ordering::SeqCst);
    }
    // SAFETY: the handler only stores to an atomic, which is signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
fn on_signals() {}

/// Write the handle where askers find it, readable by the owner alone.
fn publish(path: &Path, handle: &Handle) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(handle).expect("a handle serializes");
    let staged = path.with_extension("json.new");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&staged)
        .map_err(|error| DbError::io(&staged, error))?;
    file.write_all(&bytes)
        .map_err(|error| DbError::io(&staged, error))?;
    std::fs::rename(&staged, path).map_err(|error| DbError::io(path, error))
}

fn answer(
    stream: TcpStream,
    secret: &str,
    log: &Mutex<Log>,
    seen: &Condvar,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new(std::io::Read::take(&stream, 64 * 1024)).read_line(&mut line)?;
    let request: Request = serde_json::from_str(&line).map_err(std::io::Error::other)?;
    if !constant_time_eq(request.secret.as_bytes(), secret.as_bytes()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "wrong secret",
        ));
    }
    let deadline = Instant::now() + COOKIE_WAIT;
    let mut held = log.lock().expect("the log is never poisoned");
    let changes = loop {
        if held.cookies.remove(&request.cookie) {
            break held.answer(request.since.as_ref());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            // The cookie was not seen in time: what changed cannot be vouched
            // for, so everything is examined.
            break Changes {
                now: held.now(),
                whole: true,
                paths: vec![],
            };
        }
        held = seen
            .wait_timeout(held, left)
            .expect("the log is never poisoned")
            .0;
    };
    drop(held);
    let mut reply = serde_json::to_vec(&changes).map_err(std::io::Error::other)?;
    reply.push(b'\n');
    (&stream).write_all(&reply)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

/// Watch the folder and answer what changed, until stopped.
pub fn serve(context: &Context, sink: &mut dyn Sink) -> Result<Finish> {
    // The database is established first, as by any command that reads it.
    let database = context.open_database(sink, Intent::Read)?;
    let root = database.root.clone();
    drop(database);
    let handle_path = reldir_core::monitor::handle_path(&root);
    if let Some(running) = reldir_core::monitor::find(&root)
        && TcpStream::connect_timeout(
            &([127, 0, 0, 1], running.port).into(),
            Duration::from_millis(250),
        )
        .is_ok()
    {
        return Err(DbError::new(
            Code::LOCK_CONTENDED,
            format!(
                "a monitor already serves this database (process {}, port {})",
                running.pid, running.port
            ),
        ));
    }
    let log = Arc::new(Mutex::new(Log {
        epoch: new_epoch(),
        base: 0,
        paths: VecDeque::new(),
        cookies: HashSet::new(),
    }));
    let seen = Arc::new(Condvar::new());
    let watch_root = root.clone();
    let events_log = Arc::clone(&log);
    let events_seen = Arc::clone(&seen);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let mut held = events_log.lock().expect("the log is never poisoned");
        match event {
            Ok(event) => {
                if event.need_rescan() || matches!(event.kind, EventKind::Other) {
                    held.restart();
                }
                // Reading a file is not changing it: every command reads the
                // folder, and its reads are no news.
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                for path in &event.paths {
                    let Some(relative) = relative(&watch_root, path) else {
                        continue;
                    };
                    if let Some(cookie) = relative
                        .strip_prefix(".db/cache/")
                        .filter(|name| name.starts_with(COOKIE_PREFIX))
                    {
                        if matches!(event.kind, EventKind::Create(_)) {
                            held.cookies.insert(cookie.to_string());
                        }
                        continue;
                    }
                    if !internal(&relative) {
                        held.push(relative);
                    }
                }
            }
            // An event lost is a change unaccounted for.
            Err(_) => held.restart(),
        }
        drop(held);
        events_seen.notify_all();
    })
    .map_err(|error| DbError::new(Code::IO_ERROR, format!("cannot watch the folder: {error}")))?;
    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|error| {
            DbError::new(
                Code::IO_ERROR,
                format!("cannot watch {}: {error}", root.display()),
            )
        })?;
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| {
        DbError::new(
            Code::IO_ERROR,
            format!("cannot listen on the loopback interface: {error}"),
        )
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|error| DbError::new(Code::IO_ERROR, error.to_string()))?;
    let port = listener
        .local_addr()
        .map_err(|error| DbError::new(Code::IO_ERROR, error.to_string()))?
        .port();
    let secret = random_secret();
    let epoch = log.lock().expect("the log is never poisoned").epoch.clone();
    publish(
        &handle_path,
        &Handle {
            port,
            secret: secret.clone(),
            pid: std::process::id(),
            epoch,
        },
    )?;
    let mut record = Map::new();
    record.insert("kind".into(), json!("monitor"));
    record.insert("root".into(), json!(root));
    record.insert("port".into(), json!(port));
    record.insert("pid".into(), json!(std::process::id()));
    sink.record(record)?;
    sink.note(&format!(
        "watching {}; every process that opens the database reads only what changed. Stop with Ctrl-C.",
        root.display()
    ));
    on_signals();
    while !STOP.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let (secret, log, seen) = (secret.clone(), Arc::clone(&log), Arc::clone(&seen));
                std::thread::spawn(move || {
                    // A request that fails is the asker's to notice: it then
                    // examines the folder whole.
                    let _ = stream.set_nonblocking(false);
                    let _ = answer(stream, &secret, &log, &seen);
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                remove_handle(&handle_path);
                return Err(DbError::new(
                    Code::IO_ERROR,
                    format!("the monitor stopped listening: {error}"),
                ));
            }
        }
    }
    drop(watcher);
    remove_handle(&handle_path);
    Ok(Finish::ok("stopped watching"))
}

/// Withdraw the handle, if it is still this monitor's.
fn remove_handle(path: &PathBuf) {
    let ours = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Handle>(&bytes).ok())
        .is_some_and(|handle| handle.pid == std::process::id());
    if ours {
        let _ = std::fs::remove_file(path);
    }
}
