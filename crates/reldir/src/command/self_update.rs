//! `reldir self update`: replace this binary with the one a GitHub release
//! built for this platform.
//!
//! The release workflow attaches, for every target, `reldir-v<version>-<target>.tar.gz`
//! and `reldir-v<version>-<target>.sha256`, which lists the checksum of each
//! archive for that target (`sha256sum` lines; on Windows the `.zip` is listed
//! too). An update reads the release, downloads the
//! archive for the target this binary was built for, checks it against its
//! checksum, runs the binary it holds to see that it is reldir at the version
//! the release names, and only then puts it in place of the running one.

use crate::{
    command::Context,
    output::{Finish, Sink},
};
use reldir_core::{
    code::Code,
    diagnostic::{DbError, Result},
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// The version this binary is.
const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// The target triple it was built for; release archives are named by it.
const TARGET: &str = env!("RELDIR_TARGET");
/// The largest archive accepted, 512 MiB: a release binary is tens of megabytes.
const ARCHIVE_LIMIT: u64 = 536_870_912;
/// The largest checksum listing accepted, 64 KiB: it holds a line or two.
const CHECKSUMS_LIMIT: u64 = 65_536;
/// The largest release description accepted, 16 MiB.
const RELEASE_LIMIT: u64 = 16_777_216;
/// Where the releases are read from, unless `RELDIR_RELEASES_URL` says otherwise.
fn releases_url() -> String {
    if let Ok(url) = std::env::var("RELDIR_RELEASES_URL") {
        return url.trim_end_matches('/').to_string();
    }
    let repository = env!("CARGO_PKG_REPOSITORY").trim_end_matches('/');
    let path = repository
        .strip_prefix("https://github.com/")
        .unwrap_or("jowharshamshiri/reldir");
    format!("https://api.github.com/repos/{path}/releases")
}

const BINARY: &str = if cfg!(windows) {
    "reldir.exe"
} else {
    "reldir"
};

/// Update to the latest release, or to `version` when one is named.
pub fn update(context: &Context, sink: &mut dyn Sink, version: Option<&str>) -> Result<Finish> {
    let agent = agent()?;
    let release = match version {
        Some(version) => {
            let tag = format!("v{}", version.trim_start_matches('v'));
            fetch_release(&agent, &format!("{}/tags/{tag}", releases_url()))
                .map_err(|error| not_found_as(error, format!("there is no release {tag}")))?
        }
        None => fetch_release(&agent, &format!("{}/latest", releases_url()))
            .map_err(|error| not_found_as(error, "the repository has no release yet".into()))?,
    };
    let tag = release["tag_name"]
        .as_str()
        .ok_or_else(|| unreachable_error("the release names no tag".into()))?
        .to_string();
    let target_version = tag.trim_start_matches('v').to_string();

    let mut record = Map::new();
    record.insert("kind".into(), json!("self_update"));
    record.insert("current".into(), json!(CURRENT));
    record.insert("release".into(), json!(target_version));
    record.insert("target".into(), json!(TARGET));

    // Without a named version, only a newer release is an update.
    if version.is_none() && !newer(&target_version, CURRENT) {
        record.insert("installed".into(), json!(false));
        sink.record(record)?;
        return Ok(Finish::ok(format!(
            "reldir {CURRENT} is up to date (latest release {target_version})"
        ))
        .with("updated", false));
    }

    let archive_name = format!("reldir-{tag}-{TARGET}.tar.gz");
    let archive_url = asset_url(&release, &archive_name).ok_or_else(|| {
        DbError::new(
            Code::RELEASE_NOT_FOUND,
            format!("release {tag} holds no build for {TARGET} ({archive_name})"),
        )
        .with_help(
            "build from source: `cargo install --locked --git https://github.com/jowharshamshiri/reldir reldir`",
        )
    })?;
    let checksums_name = format!("reldir-{tag}-{TARGET}.sha256");
    let checksum_url = asset_url(&release, &checksums_name).ok_or_else(|| {
        DbError::new(
            Code::RELEASE_UNVERIFIED,
            format!("release {tag} publishes no checksum for {archive_name} ({checksums_name})"),
        )
    })?;
    let destination = std::env::current_exe().map_err(|error| {
        DbError::new(
            Code::IO_ERROR,
            format!("cannot tell where this binary is: {error}"),
        )
    })?;
    record.insert("archive".into(), json!(archive_url));
    record.insert("path".into(), json!(destination));

    if context.dry_run {
        record.insert("installed".into(), json!(false));
        sink.record(record)?;
        return Ok(Finish::ok(format!(
            "dry run: would replace reldir {CURRENT} with {target_version} at {}; nothing was written",
            destination.display()
        ))
        .with("updated", false)
        .with("dry_run", true));
    }

    sink.note(&format!("downloading {archive_name}"));
    let checksums = download(&agent, &checksum_url, CHECKSUMS_LIMIT)?;
    let published = published_checksum(&String::from_utf8_lossy(&checksums), &archive_name)
        .ok_or_else(|| {
            DbError::new(
                Code::RELEASE_UNVERIFIED,
                format!("{checksums_name} lists no checksum for {archive_name}"),
            )
        })?;
    let archive = download(&agent, &archive_url, ARCHIVE_LIMIT)?;
    let actual = hex::encode(Sha256::digest(&archive));
    if published != actual {
        return Err(DbError::new(
            Code::RELEASE_UNVERIFIED,
            format!(
                "{archive_name} does not match its checksum: published {published}, downloaded {actual}"
            ),
        ));
    }
    record.insert("sha256".into(), json!(actual));

    // Unpacked beside the binary it replaces, so the replacement is a rename
    // on one filesystem.
    let directory = destination.parent().unwrap_or(Path::new("."));
    let staging = tempfile::Builder::new()
        .prefix(".reldir-update-")
        .tempdir_in(directory)
        .or_else(|_| tempfile::tempdir())
        .map_err(|error| {
            DbError::new(
                Code::IO_ERROR,
                format!(
                    "cannot stage the update in {}: {error}",
                    directory.display()
                ),
            )
        })?;
    let unpacked = unpack(&archive, staging.path(), &archive_name)?;
    verify_runs(&unpacked, &target_version)?;
    self_replace::self_replace(&unpacked).map_err(|error| {
        DbError::new(
            Code::IO_ERROR,
            format!("cannot replace {}: {error}", destination.display()),
        )
        .with_help("run it again with permission to write there, or reinstall")
    })?;

    record.insert("installed".into(), json!(true));
    sink.record(record)?;
    Ok(Finish::ok(format!(
        "updated reldir {CURRENT} -> {target_version} at {}",
        destination.display()
    ))
    .with("updated", true))
}

fn agent() -> Result<ureq::Agent> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .unversioned_rustls_crypto_provider(provider)
        .build();
    let config = ureq::Agent::config_builder()
        .tls_config(tls)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_body(Some(Duration::from_secs(300)))
        .user_agent(format!("reldir/{CURRENT}"))
        .build();
    Ok(ureq::Agent::new_with_config(config))
}

/// A release, as the GitHub API describes it.
fn fetch_release(agent: &ureq::Agent, url: &str) -> std::result::Result<Value, FetchError> {
    let mut request = agent
        .get(url)
        .header("Accept", "application/vnd.github+json");
    // A token raises GitHub's limit on unauthenticated reads; it is sent to
    // the API only, never to wherever an asset is served from.
    if url.starts_with("https://api.github.com/")
        && let Some(token) = ["GITHUB_TOKEN", "GH_TOKEN"]
            .iter()
            .find_map(|name| usable_token(std::env::var(name).ok()))
    {
        request = request.header("Authorization", format!("Bearer {token}"));
    }
    let mut response = request.call().map_err(|error| match error {
        ureq::Error::StatusCode(404) => FetchError::NotFound,
        error => FetchError::Other(format!("cannot read {url}: {error}")),
    })?;
    let body = response
        .body_mut()
        .with_config()
        .limit(RELEASE_LIMIT)
        .read_to_vec()
        .map_err(|error| FetchError::Other(format!("cannot read {url}: {error}")))?;
    serde_json::from_slice(&body)
        .map_err(|error| FetchError::Other(format!("{url} is not a release: {error}")))
}

/// A token worth sending: set, and not blank.
fn usable_token(token: Option<String>) -> Option<String> {
    token.filter(|token| !token.trim().is_empty())
}

enum FetchError {
    NotFound,
    Other(String),
}

fn not_found_as(error: FetchError, message: String) -> DbError {
    match error {
        FetchError::NotFound => DbError::new(Code::RELEASE_NOT_FOUND, message),
        FetchError::Other(message) => unreachable_error(message),
    }
}

fn unreachable_error(message: String) -> DbError {
    DbError::new(Code::RELEASE_UNREACHABLE, message).with_help(
        "check the network; a proxy is read from HTTPS_PROXY, and GITHUB_TOKEN raises GitHub's rate limit",
    )
}

fn asset_url(release: &Value, name: &str) -> Option<String> {
    release["assets"]
        .as_array()?
        .iter()
        .find(|asset| asset["name"] == name)?["browser_download_url"]
        .as_str()
        .map(str::to_string)
}

fn download(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>> {
    let mut response = agent
        .get(url)
        .call()
        .map_err(|error| unreachable_error(format!("cannot download {url}: {error}")))?;
    response
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()
        .map_err(|error| unreachable_error(format!("cannot download {url}: {error}")))
}

/// Unpack the archive's reldir binary into `directory`, returning its path.
fn unpack(archive: &[u8], directory: &Path, name: &str) -> Result<PathBuf> {
    let unverified = |message: String| DbError::new(Code::RELEASE_UNVERIFIED, message);
    let mut entries = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in entries
        .entries()
        .map_err(|error| unverified(format!("{name} is not a gzipped tar archive: {error}")))?
    {
        let mut entry =
            entry.map_err(|error| unverified(format!("{name} cannot be read: {error}")))?;
        let path = entry
            .path()
            .map_err(|error| unverified(format!("{name} holds an unreadable name: {error}")))?;
        // The release puts the binary at the archive's root; a file of the
        // same name deeper in is something else.
        if !entry.header().entry_type().is_file() || path != Path::new(BINARY) {
            continue;
        }
        let mut bytes = vec![];
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| unverified(format!("{name} cannot be read: {error}")))?;
        let unpacked = directory.join(BINARY);
        std::fs::write(&unpacked, bytes).map_err(|error| {
            DbError::new(
                Code::IO_ERROR,
                format!("cannot write {}: {error}", unpacked.display()),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&unpacked, std::fs::Permissions::from_mode(0o755)).map_err(
                |error| {
                    DbError::new(
                        Code::IO_ERROR,
                        format!("cannot make {} executable: {error}", unpacked.display()),
                    )
                },
            )?;
        }
        return Ok(unpacked);
    }
    Err(unverified(format!("{name} holds no {BINARY}")))
}

/// The new binary runs here, and says it is the version the release names.
fn verify_runs(binary: &Path, version: &str) -> Result<()> {
    let output = std::process::Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|error| {
            DbError::new(
                Code::RELEASE_UNVERIFIED,
                format!("the downloaded reldir does not run here: {error}"),
            )
        })?;
    let said = String::from_utf8_lossy(&output.stdout);
    if !output.status.success() || said.trim() != format!("reldir {version}") {
        return Err(DbError::new(
            Code::RELEASE_UNVERIFIED,
            format!(
                "the downloaded binary says {:?}, not reldir {version}",
                said.trim()
            ),
        ));
    }
    Ok(())
}

/// The checksum a `sha256sum` listing gives `name`: lines of the hash, a
/// space, and the name, marked `*` when hashed in binary mode.
fn published_checksum(listing: &str, name: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let (hash, listed) = line.trim_end().split_once(char::is_whitespace)?;
        (listed.trim_start().trim_start_matches('*') == name).then(|| hash.to_ascii_lowercase())
    })
}

/// Whether `candidate` is a later version than `current`: dotted numbers
/// compared as numbers, so 0.103.1678 follows 0.99.2000.
fn newer(candidate: &str, current: &str) -> bool {
    fn parts(version: &str) -> Vec<u64> {
        version
            .split(['-', '+', '_'])
            .next()
            .unwrap_or_default()
            .split('.')
            .map(|part| part.parse().unwrap_or(0))
            .collect()
    }
    parts(candidate) > parts(current)
}

#[cfg(test)]
mod tests {
    use super::{newer, published_checksum, usable_token};

    #[test]
    fn test2613_a_blank_token_is_not_sent() {
        assert_eq!(usable_token(None), None);
        assert_eq!(usable_token(Some(String::new())), None);
        assert_eq!(usable_token(Some("  ".into())), None);
        assert_eq!(usable_token(Some("ghp_x".into())).as_deref(), Some("ghp_x"));
    }

    #[test]
    fn test2600_a_release_is_newer_by_number_not_by_text() {
        assert!(newer("0.103.1678", "0.99.2000"));
        assert!(newer("0.103.1679", "0.103.1678"));
        assert!(newer("1.0.0", "0.999.999"));
        assert!(!newer("0.103.1678", "0.103.1678"));
        assert!(!newer("0.9.0", "0.10.0"));
        assert!(!newer("0.103.1678", "0.103.1678-dev"));
    }

    #[test]
    fn test2611_a_checksum_is_read_by_the_name_it_lists() {
        let unix = "abc123  reldir-v1.2.3-x86_64-unknown-linux-gnu.tar.gz\n";
        assert_eq!(
            published_checksum(unix, "reldir-v1.2.3-x86_64-unknown-linux-gnu.tar.gz").as_deref(),
            Some("abc123")
        );
        let windows = "AAA *reldir-v1.2.3-x86_64-pc-windows-msvc.tar.gz\r\nBBB *reldir-v1.2.3-x86_64-pc-windows-msvc.zip\r\n";
        assert_eq!(
            published_checksum(windows, "reldir-v1.2.3-x86_64-pc-windows-msvc.tar.gz").as_deref(),
            Some("aaa")
        );
        assert_eq!(
            published_checksum(windows, "reldir-v1.2.3-x86_64-pc-windows-msvc.zip").as_deref(),
            Some("bbb")
        );
        assert_eq!(published_checksum(unix, "another.tar.gz"), None);
    }
}
