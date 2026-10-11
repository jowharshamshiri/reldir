//! `reldir self update` against a release served from the loopback: a copy of
//! the binary is updated, never the one the tests run.
#![cfg(all(unix, feature = "self-update"))]

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::{Path, PathBuf},
    process::Command,
};

const TARGET: &str = env!("RELDIR_TARGET");

/// Serve, until the process ends, the files `make` lays out for the base URL
/// the server listens on; returns that URL.
fn serve(make: impl FnOnce(&str) -> HashMap<String, Vec<u8>>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let files = make(&base);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap() == 0 || header == "\r\n" {
                    break;
                }
            }
            let (status, body) = match files.get(&path) {
                Some(body) => ("200 OK", body.clone()),
                None => ("404 Not Found", b"{\"message\":\"Not Found\"}".to_vec()),
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    base
}

/// A gzipped tar holding a `reldir` that answers `--version` as `version`.
fn archive(version: &str) -> Vec<u8> {
    let script = format!("#!/bin/sh\necho 'reldir {version}'\n");
    let mut header = tar::Header::new_gnu();
    header.set_size(script.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
        vec![],
        flate2::Compression::default(),
    ));
    builder
        .append_data(&mut header, "reldir", script.as_bytes())
        .unwrap();
    builder.into_inner().unwrap().finish().unwrap()
}

/// A release as the workflow publishes one: its archive for `target`, and
/// the archive's checksum beside it.
struct Release {
    version: &'static str,
    target: &'static str,
    archive: Vec<u8>,
    /// The listing published as `reldir-v<version>-<target>.sha256`.
    checksums: Option<String>,
}

impl Release {
    fn of(version: &'static str) -> Self {
        let archive = archive(version);
        let checksums = listing(&archive, &format!("reldir-v{version}-{TARGET}.tar.gz"));
        Release {
            version,
            target: TARGET,
            checksums: Some(checksums),
            archive,
        }
    }

    /// Served as the latest release, and by its tag.
    fn serve(self) -> String {
        serve(|base| {
            let tag = format!("v{}", self.version);
            let name = format!("reldir-{tag}-{}.tar.gz", self.target);
            let sums = format!("reldir-{tag}-{}.sha256", self.target);
            let mut assets = vec![
                json!({"name": name, "browser_download_url": format!("{base}/download/{name}")}),
            ];
            if self.checksums.is_some() {
                assets.push(json!({"name": sums, "browser_download_url": format!("{base}/download/{sums}")}));
            }
            let release = serde_json::to_vec(&json!({"tag_name": tag, "assets": assets})).unwrap();
            let mut files = HashMap::new();
            files.insert("/releases/latest".to_string(), release.clone());
            files.insert(format!("/releases/tags/{tag}"), release);
            if let Some(checksums) = self.checksums {
                files.insert(format!("/download/{sums}"), checksums.into_bytes());
            }
            files.insert(format!("/download/{name}"), self.archive);
            files
        })
    }
}

/// A `sha256sum` line for `bytes` under `name`.
fn listing(bytes: &[u8], name: &str) -> String {
    format!("{}  {name}\n", hex::encode(Sha256::digest(bytes)))
}

/// Run a binary just written. Another test's child, forked while the file
/// was open for writing, can hold it a moment longer (`ETXTBSY`).
fn output(command: &mut Command) -> std::process::Output {
    for _ in 0..100 {
        match command.output() {
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            result => return result.unwrap(),
        }
    }
    panic!("{command:?} stayed busy")
}

/// A copy of the binary under test, in a directory of its own.
fn copy_of_reldir() -> (tempfile::TempDir, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let copy = directory.path().join("reldir");
    fs::copy(assert_cmd::cargo::cargo_bin("reldir"), &copy).unwrap();
    (directory, copy)
}

fn update(binary: &Path, base: &str, args: &[&str]) -> (Value, i32) {
    let output = output(
        Command::new(binary)
            .args(["--json", "self", "update"])
            .args(args)
            .env("RELDIR_RELEASES_URL", format!("{base}/releases")),
    );
    let envelope = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (envelope, output.status.code().unwrap())
}

fn version_of(binary: &Path) -> String {
    let output = output(Command::new(binary).arg("--version"));
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

fn current() -> String {
    format!("reldir {}", env!("CARGO_PKG_VERSION"))
}

#[test]
fn test2601_a_newer_release_replaces_the_binary_that_ran() {
    let (_directory, binary) = copy_of_reldir();
    let base = Release::of("999.0.0").serve();
    let (envelope, exit) = update(&binary, &base, &[]);
    assert_eq!(exit, 0, "{envelope:#}");
    let record = &envelope["records"][0];
    assert_eq!(record["type"], "self_update", "{envelope:#}");
    assert_eq!(record["installed"], true);
    assert_eq!(record["release"], "999.0.0");
    assert_eq!(version_of(&binary), "reldir 999.0.0");
}

#[test]
fn test2602_no_newer_release_leaves_the_binary_alone() {
    let (_directory, binary) = copy_of_reldir();
    let base = Release::of("0.0.1").serve();
    let (envelope, exit) = update(&binary, &base, &[]);
    assert_eq!(exit, 0, "{envelope:#}");
    assert_eq!(envelope["records"][0]["installed"], false);
    assert!(
        envelope["summary"].as_str().unwrap().contains("up to date"),
        "{envelope:#}"
    );
    assert_eq!(version_of(&binary), current());
}

#[test]
fn test2603_a_named_version_is_installed_even_when_older() {
    let (_directory, binary) = copy_of_reldir();
    let base = Release::of("0.0.1").serve();
    let (envelope, exit) = update(&binary, &base, &["0.0.1"]);
    assert_eq!(exit, 0, "{envelope:#}");
    assert_eq!(version_of(&binary), "reldir 0.0.1");
}

#[test]
fn test2604_an_archive_that_fails_its_checksum_is_never_installed() {
    let (_directory, binary) = copy_of_reldir();
    let mut release = Release::of("999.0.0");
    release.checksums = Some(format!(
        "{}  reldir-v999.0.0-{TARGET}.tar.gz\n",
        "0".repeat(64)
    ));
    let (envelope, exit) = update(&binary, &release.serve(), &[]);
    assert_eq!(exit, 6, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "RELEASE_UNVERIFIED");
    assert_eq!(version_of(&binary), current());
}

#[test]
fn test2605_a_release_without_a_checksum_is_never_installed() {
    let (_directory, binary) = copy_of_reldir();
    let mut release = Release::of("999.0.0");
    release.checksums = None;
    let (envelope, exit) = update(&binary, &release.serve(), &[]);
    assert_eq!(exit, 6, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "RELEASE_UNVERIFIED");
    assert_eq!(version_of(&binary), current());
}

#[test]
fn test2606_a_binary_that_is_not_the_release_it_claims_is_refused() {
    let (_directory, binary) = copy_of_reldir();
    let mut release = Release::of("999.0.0");
    release.archive = archive("998.0.0");
    release.checksums = Some(listing(
        &release.archive,
        &format!("reldir-v999.0.0-{TARGET}.tar.gz"),
    ));
    let (envelope, exit) = update(&binary, &release.serve(), &[]);
    assert_eq!(exit, 6, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "RELEASE_UNVERIFIED");
    assert_eq!(version_of(&binary), current());
}

#[test]
fn test2607_a_release_with_no_build_for_this_platform_is_reported() {
    let (_directory, binary) = copy_of_reldir();
    let mut release = Release::of("999.0.0");
    release.target = "sparc-sun-solaris";
    let (envelope, exit) = update(&binary, &release.serve(), &[]);
    assert_eq!(exit, 4, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "RELEASE_NOT_FOUND");
    assert_eq!(version_of(&binary), current());
}

#[test]
fn test2608_an_unknown_version_is_reported() {
    let (_directory, binary) = copy_of_reldir();
    let base = Release::of("999.0.0").serve();
    let (envelope, exit) = update(&binary, &base, &["1.2.3"]);
    assert_eq!(exit, 4, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "RELEASE_NOT_FOUND");
}

#[test]
fn test2609_a_dry_run_names_the_update_and_writes_nothing() {
    let (directory, binary) = copy_of_reldir();
    let base = Release::of("999.0.0").serve();
    let (envelope, exit) = update(&binary, &base, &["--dry-run"]);
    assert_eq!(exit, 0, "{envelope:#}");
    assert_eq!(envelope["records"][0]["installed"], false);
    assert_eq!(version_of(&binary), current());
    let left: Vec<_> = fs::read_dir(directory.path()).unwrap().collect();
    assert_eq!(left.len(), 1, "nothing staged beside the binary");
}

#[test]
fn test2610_an_unreachable_release_server_is_reported() {
    let (_directory, binary) = copy_of_reldir();
    // A port that was free a moment ago: nothing answers there.
    let base = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let (envelope, exit) = update(&binary, &base, &[]);
    assert_eq!(exit, 6, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "RELEASE_UNREACHABLE");
}

#[test]
fn test2612_a_checksum_listing_without_the_archive_is_never_trusted() {
    let (_directory, binary) = copy_of_reldir();
    let mut release = Release::of("999.0.0");
    // Windows' listing names the .zip as well; one naming only another
    // archive vouches for nothing downloaded here.
    release.checksums = Some(listing(&release.archive, "reldir-v999.0.0-other.zip"));
    let (envelope, exit) = update(&binary, &release.serve(), &[]);
    assert_eq!(exit, 6, "{envelope:#}");
    assert_eq!(envelope["error"]["code"], "RELEASE_UNVERIFIED");
    assert_eq!(version_of(&binary), current());
}
