//! `cargo xtask scale`: build a dataset of a given size, run the release
//! binary over it, and hold each operation to its target.
//!
//! ```text
//! cargo build --release -p reldir
//! cargo xtask scale --rows 100000 [--binary target/release/reldir] [--dir /tmp/scale]
//! ```
//!
//! Two tables, `users` and `posts` (two posts per user, each referencing its
//! author), written as an application would write them. The operations are
//! timed with the folder monitor (`reldir serve`) running, as a large
//! installation runs, and the plain reads once more without it. Each target
//! scales with the row count where the work must; a missed target fails the
//! run.

use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Options {
    rows: usize,
    binary: PathBuf,
    dir: PathBuf,
}

fn options() -> Result<Options, String> {
    let mut arguments = std::env::args().skip(1);
    match arguments.next().as_deref() {
        Some("scale") => {}
        other => return Err(format!("unknown task {other:?}; the task is `scale`")),
    }
    let mut options = Options {
        rows: 100_000,
        binary: PathBuf::from("target/release/reldir"),
        dir: std::env::temp_dir().join("reldir-scale"),
    };
    while let Some(flag) = arguments.next() {
        let value = arguments
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--rows" => options.rows = value.parse().map_err(|_| format!("--rows {value}"))?,
            "--binary" => options.binary = PathBuf::from(value),
            "--dir" => options.dir = PathBuf::from(value),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    if let Ok(target) = std::env::var("CARGO_TARGET_DIR")
        && options.binary == Path::new("target/release/reldir")
    {
        options.binary = PathBuf::from(target).join("release/reldir");
    }
    Ok(options)
}

/// Write the dataset: users and their posts.
fn generate(root: &Path, users: usize) -> std::io::Result<()> {
    if root.exists() {
        std::fs::remove_dir_all(root)?;
    }
    std::fs::create_dir_all(root.join("users"))?;
    std::fs::create_dir_all(root.join("posts"))?;
    for user in 0..users {
        let row = json!({"id": format!("u{user}"), "name": format!("User {user}"), "age": 18 + user % 60});
        std::fs::write(
            root.join(format!("users/u{user}.json")),
            serde_json::to_string_pretty(&row).unwrap() + "\n",
        )?;
        for post in 0..2 {
            let row = json!({
                "id": format!("p{user}-{post}"),
                "user_id": format!("u{user}"),
                "title": format!("Post {post} by {user}"),
                "published": format!("2024-01-{:02}T10:00:00+02:00", 1 + (user + post) % 28)
            });
            std::fs::write(
                root.join(format!("posts/p{user}-{post}.json")),
                serde_json::to_string_pretty(&row).unwrap() + "\n",
            )?;
        }
    }
    Ok(())
}

/// Run the binary; how long it took, and its result document.
fn run(options: &Options, arguments: &[&str]) -> (Duration, Value) {
    let started = Instant::now();
    let output = Command::new(&options.binary)
        .arg("--db")
        .arg(&options.dir)
        .args(["--format", "json"])
        .args(arguments)
        .env_remove("RELDIR_DB")
        .output()
        .unwrap_or_else(|error| panic!("cannot run {}: {error}", options.binary.display()));
    let elapsed = started.elapsed();
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    if envelope["ok"] != true {
        panic!(
            "`reldir {}` failed: {}\n{}",
            arguments.join(" "),
            envelope,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    (elapsed, envelope)
}

/// The best of a few runs: what the operation costs, not the noise.
fn best(options: &Options, arguments: &[&str]) -> Duration {
    (0..3).map(|_| run(options, arguments).0).min().unwrap()
}

fn size(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        std::fs::read_dir(path)
            .map(|entries| entries.flatten().map(|entry| size(&entry.path())).sum())
            .unwrap_or(0)
    } else {
        meta.len()
    }
}

/// The space a tree occupies on disk: its files' allocated blocks.
fn disk_usage(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_dir() {
        return std::fs::read_dir(path)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| disk_usage(&entry.path()))
                    .sum()
            })
            .unwrap_or(0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        meta.blocks() * 512
    }
    #[cfg(not(unix))]
    {
        meta.len()
    }
}

struct Monitor(Child);

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn monitor(options: &Options) -> Monitor {
    let child = Command::new(&options.binary)
        .arg("--db")
        .arg(&options.dir)
        .arg("serve")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the monitor starts");
    let handle = options.dir.join(".db/serve.json");
    let started = Instant::now();
    loop {
        let published = std::fs::read(&handle)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .is_some_and(|handle| handle["pid"] == child.id());
        if published {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the monitor did not start"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Monitor(child)
}

struct Report {
    failures: usize,
}

impl Report {
    fn check(&mut self, what: &str, took: Duration, target: Duration) {
        let pass = took <= target;
        if !pass {
            self.failures += 1;
        }
        println!(
            "{:<44} {:>9.3}s  target {:>8.3}s  {}",
            what,
            took.as_secs_f64(),
            target.as_secs_f64(),
            if pass { "ok" } else { "MISSED" }
        );
    }
}

fn main() {
    let options = match options() {
        Ok(options) => options,
        Err(message) => {
            eprintln!(
                "{message}\nusage: cargo xtask scale [--rows N] [--binary PATH] [--dir PATH]"
            );
            std::process::exit(2);
        }
    };
    let users = options.rows.div_ceil(3);
    println!("generating {} rows in {}", users * 3, options.dir.display());
    generate(&options.dir, users).expect("the dataset is written");
    let data = size(&options.dir.join("users")) + size(&options.dir.join("posts"));
    let rows = (users * 3) as f64;
    // Targets for 100k rows, scaled to the dataset where the work is linear.
    let per_100k = |seconds: f64| Duration::from_secs_f64(seconds * (rows / 100_000.0).max(1.0));
    let mut report = Report { failures: 0 };

    let (adopted, _) = run(&options, &["status"]);
    println!(
        "{:<44} {:>9.3}s",
        "first use: adopt the folder",
        adopted.as_secs_f64()
    );
    report.check(
        "warm status, unmonitored (scans every file)",
        best(&options, &["status"]),
        per_100k(1.0),
    );

    let _monitor = monitor(&options);
    run(&options, &["status"]);
    report.check(
        "warm status, monitored",
        best(&options, &["status"]),
        Duration::from_millis(100),
    );
    report.check(
        "SELECT count(*), monitored",
        best(&options, &["sql", "SELECT count(*) AS n FROM posts"]),
        Duration::from_millis(100),
    );
    report.check(
        "indexed SELECT by key, monitored",
        best(
            &options,
            &["sql", "SELECT title FROM posts WHERE user_id = 'u7'"],
        ),
        Duration::from_millis(100),
    );
    report.check(
        "one-row update, monitored",
        best(
            &options,
            &["sql", "UPDATE users SET age = age + 1 WHERE id = 'u5'"],
        ),
        Duration::from_millis(150),
    );
    let bulk = (10_000).min(users);
    let started = Instant::now();
    run(
        &options,
        &[
            "sql",
            &format!(
                "UPDATE users SET age = age + 1 WHERE CAST(substr(id, 2) AS INTEGER) < {bulk}"
            ),
        ],
    );
    report.check(
        &format!("{bulk}-row update"),
        started.elapsed(),
        Duration::from_secs(3),
    );
    report.check(
        "snapshot create",
        run(&options, &["snapshot", "create", "scale"]).0,
        Duration::from_millis(500),
    );
    report.check(
        "check, monitored",
        best(&options, &["check"]),
        per_100k(1.5),
    );

    // What the folder occupies is its files' blocks, not the sum of their
    // lengths: the target holds `.db/` to the folder's own footprint.
    let footprint = disk_usage(&options.dir.join("users")) + disk_usage(&options.dir.join("posts"));
    let metadata = disk_usage(&options.dir.join(".db"));
    let overhead = metadata as f64 / footprint as f64;
    println!(
        "{:<44} {:>9.2}x  target {:>8.2}x  {}   ({:.1} MB of .db; the rows' bytes alone: {:.1} MB)",
        ".db on disk / rows on disk",
        overhead,
        1.5,
        if overhead <= 1.5 { "ok" } else { "MISSED" },
        metadata as f64 / 1e6,
        data as f64 / 1e6,
    );
    if overhead > 1.5 {
        report.failures += 1;
    }
    if report.failures > 0 {
        eprintln!("{} target(s) missed", report.failures);
        std::process::exit(1);
    }
}
