//! Writer lock acquisition.
//!
//! One binary-managed writer at a time, enforced by an exclusive advisory lock
//! on `.db/lock`. This module is the only place that lock is taken, so the
//! waiting policy is stated once rather than diverging between the two call
//! sites that need it (opening a database that may write, and committing a
//! transaction).
//!
//! # Why polling rather than a blocking lock
//!
//! `fs2` offers `lock_exclusive`, which blocks, and `try_lock_exclusive`, which
//! does not. Neither is a *bounded* wait, and an unbounded block is the wrong
//! default for a tool invoked from scripts: a stuck writer would hang every
//! caller with no diagnostic and no way out but a signal. Waiting is therefore
//! built from `try_lock_exclusive` against a deadline.
//!
//! Polling has a real cost -- the lock can be free for most of a sleep interval
//! before the waiter notices -- so the schedule is chosen to keep that cost
//! small while not spinning: a short initial delay, exponential growth, and a
//! cap well under a typical commit so a freed lock is claimed promptly.
//!
//! # Why the delay is jittered
//!
//! Several writers refused at the same instant would otherwise retry in
//! lockstep, colliding again on every round. Full jitter -- a uniform draw from
//! `[0, delay]` rather than `delay` itself -- spreads them out, which is the
//! standard remedy for the convoy this would otherwise create. The draw comes
//! from the process's own clock and address space rather than a random
//! dependency, because the requirement is decorrelation between processes, not
//! unpredictability.

use crate::diagnostic::{DbError, Result};
use fs2::FileExt;
use std::{
    fs::{self, File},
    path::Path,
    time::{Duration, Instant},
};

/// Shortest pause between attempts.
///
/// Below this the waiter burns measurable CPU against a lock held for a typical
/// commit, and gains nothing: the syscall itself costs more than the latency it
/// would save.
const INITIAL_DELAY: Duration = Duration::from_millis(2);

/// Longest pause between attempts.
///
/// The worst case for noticing a freed lock. Kept well under the cost of a
/// small commit so that a waiter reacts promptly rather than sitting out an
/// interval that the holder has already finished.
const MAX_DELAY: Duration = Duration::from_millis(50);

/// How a refused acquisition should be described.
///
/// The two call sites contend for the same lock but are refused doing different
/// things, and a caller reading the message needs to know which. Neither is a
/// different *kind* of failure: both are `LOCK_CONTENDED`, both are safe to
/// retry, and both mean nothing was written.
#[derive(Debug, Clone, Copy)]
pub enum Holder {
    /// Refused while opening a database that may refresh derived state.
    Observing,
    /// Refused while committing a transaction.
    Committing,
}

impl Holder {
    fn message(self, waited: Option<Duration>) -> String {
        let subject = match self {
            Self::Observing => "another writer is validating or changing the database",
            Self::Committing => "another writer holds the database lock",
        };
        match waited {
            // A bare refusal and a refusal after waiting are different facts. A
            // caller that passed `--wait` and still failed needs to know the
            // wait elapsed rather than that it was never honoured.
            Some(waited) => format!(
                "{subject}; still held after waiting {:.3}s",
                waited.as_secs_f64()
            ),
            None => subject.to_string(),
        }
    }
}

/// How long to wait for the writer lock before giving up.
///
/// `Duration::ZERO` is a deliberate value and not a synonym for "no wait
/// configured": it means try once and report contention immediately, which is
/// what a CI job wanting a fast, deterministic failure asks for. There is no
/// "wait forever" -- every wait is bounded, because an unbounded one turns a
/// stuck writer into a hung caller.
pub type Budget = Duration;

/// Open `.db/lock` and take it exclusively, waiting up to `budget`.
///
/// The returned `File` owns the lock: dropping it releases the lock, so callers
/// hold it for exactly as long as they hold the file. The lock is advisory and
/// released by the kernel if the process dies, so a crashed writer never
/// strands it.
pub fn acquire(path: &Path, budget: Budget, holder: Holder) -> Result<File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| DbError::io(path, error))?;

    // The first attempt is made before consulting the clock at all, so an
    // uncontended lock costs one syscall whatever the budget says.
    if file.try_lock_exclusive().is_ok() {
        return Ok(file);
    }
    if budget.is_zero() {
        return Err(contended(holder, None));
    }

    let started = Instant::now();
    let mut delay = INITIAL_DELAY;
    loop {
        let elapsed = started.elapsed();
        let Some(remaining) = budget.checked_sub(elapsed) else {
            return Err(contended(holder, Some(elapsed)));
        };
        if remaining.is_zero() {
            return Err(contended(holder, Some(elapsed)));
        }
        // Never sleep past the deadline: a caller that asked for 100ms must not
        // be made to wait 150 because that was the next step in the schedule.
        std::thread::sleep(jitter(delay).min(remaining));
        if file.try_lock_exclusive().is_ok() {
            return Ok(file);
        }
        delay = (delay * 2).min(MAX_DELAY);
    }
}

fn contended(holder: Holder, waited: Option<Duration>) -> DbError {
    DbError::new("LOCK_CONTENDED", holder.message(waited), 3)
}

/// A uniform draw from `[0, delay]`.
///
/// Decorrelates waiters that were refused together. The source is the monotonic
/// clock's sub-nanosecond noise mixed with a per-process address, which differ
/// between concurrent processes -- the only property this needs. It is not a
/// general-purpose random number generator and nothing here depends on it being
/// one.
fn jitter(delay: Duration) -> Duration {
    let nanos = delay.as_nanos() as u64;
    if nanos == 0 {
        return Duration::ZERO;
    }
    let clock = Instant::now();
    let entropy = {
        let address = &clock as *const Instant as u64;
        let tick = clock.elapsed().subsec_nanos() as u64;
        // A cheap integer mix so that neighbouring clock readings do not map to
        // neighbouring delays.
        let mut x = address ^ tick.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        x ^= x >> 33;
        x = x.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        x ^= x >> 33;
        x
    };
    Duration::from_nanos(entropy % (nanos + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An uncontended lock is taken immediately, whatever the budget says.
    #[test]
    fn test1200_an_uncontended_lock_is_taken_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let started = Instant::now();
        let _held = acquire(&path, Duration::from_secs(30), Holder::Committing)
            .expect("an uncontended lock must be granted");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "a free lock must not consult the budget"
        );
    }

    /// A zero budget means one attempt. It must refuse a held lock promptly
    /// rather than treating zero as "wait forever" or as a missing value.
    #[test]
    fn test1201_a_zero_budget_refuses_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let _held = acquire(&path, Duration::ZERO, Holder::Committing).unwrap();

        let started = Instant::now();
        let error = acquire(&path, Duration::ZERO, Holder::Committing)
            .expect_err("a held lock must be refused");
        assert_eq!(error.diagnostic.code, "LOCK_CONTENDED");
        assert_eq!(error.exit_code(), 3);
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "a zero budget must not sleep"
        );
        // A refusal that never waited must not claim it waited.
        assert!(
            !error.diagnostic.message.contains("after waiting"),
            "{}",
            error.diagnostic.message
        );
    }

    /// A budget that expires reports contention, having actually waited about
    /// that long -- not returning early and not overshooting.
    #[test]
    fn test1202_an_expired_budget_waits_for_it_then_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let _held = acquire(&path, Duration::ZERO, Holder::Committing).unwrap();

        let budget = Duration::from_millis(300);
        let started = Instant::now();
        let error =
            acquire(&path, budget, Holder::Committing).expect_err("a held lock must be refused");
        let elapsed = started.elapsed();

        assert_eq!(error.diagnostic.code, "LOCK_CONTENDED");
        assert!(
            elapsed >= budget,
            "must wait the whole budget, waited {elapsed:?}"
        );
        assert!(
            elapsed < budget * 4,
            "must not overshoot the budget materially, waited {elapsed:?}"
        );
        assert!(
            error.diagnostic.message.contains("after waiting"),
            "a refusal that waited must say so: {}",
            error.diagnostic.message
        );
    }

    /// The point of the whole module: a waiter acquires the lock once the
    /// holder releases it, rather than failing because it was busy at the
    /// instant of the first attempt.
    #[test]
    fn test1203_a_waiter_acquires_the_lock_once_it_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let held = acquire(&path, Duration::ZERO, Holder::Committing).unwrap();

        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            drop(held);
        });

        let started = Instant::now();
        let acquired = acquire(&path, Duration::from_secs(10), Holder::Committing)
            .expect("a waiter must win the lock once it is free");
        let elapsed = started.elapsed();
        releaser.join().unwrap();
        drop(acquired);

        assert!(
            elapsed >= Duration::from_millis(100),
            "cannot have acquired before the holder released, took {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "must notice the release promptly, took {elapsed:?}"
        );
    }

    /// Exactly one of many waiters may hold the lock at a time. Each either
    /// holds it alone or is refused; none observes a shared hold.
    #[test]
    fn test1204_concurrent_waiters_are_serialised_and_never_overlap() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let inside = Arc::new(AtomicUsize::new(0));
        let overlaps = Arc::new(AtomicUsize::new(0));
        let acquisitions = Arc::new(AtomicUsize::new(0));

        let mut handles = vec![];
        for _ in 0..8 {
            let path = path.clone();
            let inside = Arc::clone(&inside);
            let overlaps = Arc::clone(&overlaps);
            let acquisitions = Arc::clone(&acquisitions);
            handles.push(std::thread::spawn(move || {
                for _ in 0..4 {
                    let held = acquire(&path, Duration::from_secs(20), Holder::Committing)
                        .expect("a generous budget must eventually win the lock");
                    acquisitions.fetch_add(1, Ordering::SeqCst);
                    if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                        overlaps.fetch_add(1, Ordering::SeqCst);
                    }
                    std::thread::sleep(Duration::from_millis(2));
                    inside.fetch_sub(1, Ordering::SeqCst);
                    drop(held);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(
            overlaps.load(Ordering::SeqCst),
            0,
            "two holders were inside the lock at once"
        );
        assert_eq!(
            acquisitions.load(Ordering::SeqCst),
            32,
            "every waiter with a sufficient budget must make progress"
        );
    }

    /// Jitter must stay inside its interval. A draw above the delay would make
    /// a waiter overshoot its deadline; one that is always the maximum would
    /// not decorrelate anything.
    #[test]
    fn test1205_jitter_stays_within_its_interval_and_varies() {
        let delay = Duration::from_millis(40);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..256 {
            let drawn = jitter(delay);
            assert!(drawn <= delay, "jitter {drawn:?} exceeded its delay");
            seen.insert(drawn.as_nanos());
        }
        assert!(
            seen.len() > 1,
            "a constant jitter decorrelates nothing: {} distinct draws",
            seen.len()
        );
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
    }
}
