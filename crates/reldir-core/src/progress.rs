//! Progress of long operations, reported to whoever embeds the engine.
//!
//! The engine never writes to a terminal: a library that printed would garble
//! the output of every program embedding it. Long operations -- scanning a
//! table, inferring schemas -- instead report through a [`Reporter`] the
//! embedding program installs once. The command line installs one that draws a
//! progress line on a terminal; a binding may forward to its host language; with
//! none installed, progress is simply not reported.

use std::sync::{Arc, OnceLock, RwLock};

/// Receives progress events. Implementations must be cheap: `advance` is called
/// once per file of a scan.
pub trait Reporter: Send + Sync {
    /// An operation over `total` items began.
    fn begin(&self, label: &'static str, total: usize);
    /// `done` of the operation's items are finished.
    fn advance(&self, label: &'static str, done: usize, total: usize);
    /// The operation ended, however it ended.
    fn end(&self, label: &'static str);
}

fn installed() -> &'static RwLock<Option<Arc<dyn Reporter>>> {
    static REPORTER: OnceLock<RwLock<Option<Arc<dyn Reporter>>>> = OnceLock::new();
    REPORTER.get_or_init(|| RwLock::new(None))
}

/// Install the process's reporter, replacing any installed before.
pub fn install(reporter: Arc<dyn Reporter>) {
    // A poisoned lock means a reporter panicked mid-call; the slot itself is a
    // plain `Option`, so taking it over is sound.
    let mut slot = installed()
        .write()
        .unwrap_or_else(|poison| poison.into_inner());
    *slot = Some(reporter);
}

fn current() -> Option<Arc<dyn Reporter>> {
    installed()
        .read()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone()
}

/// One long operation in progress. Ends when dropped, so an operation that
/// fails part way still clears whatever its reporter drew.
pub struct Progress {
    label: &'static str,
    total: usize,
    done: usize,
    reporter: Option<Arc<dyn Reporter>>,
}

impl Progress {
    pub fn new(label: &'static str, total: usize) -> Self {
        let reporter = current();
        if let Some(reporter) = &reporter {
            reporter.begin(label, total);
        }
        Self {
            label,
            total,
            done: 0,
            reporter,
        }
    }

    pub fn advance(&mut self) {
        self.done += 1;
        if let Some(reporter) = &self.reporter {
            reporter.advance(self.label, self.done, self.total);
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(reporter) = &self.reporter {
            reporter.end(self.label);
        }
    }
}
