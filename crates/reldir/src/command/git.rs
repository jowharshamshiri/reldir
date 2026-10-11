//! `reldir git`: row files that merge member by member and diff as JSON.
//!
//! `setup` declares, in the database's `.gitattributes`, that each table's
//! row files merge with `reldir git merge-file` and diff through
//! `reldir git textconv`, and configures both drivers in the repository. Two
//! branches that change different members of one row then merge cleanly,
//! and a conflict is marked around the member both changed, not the file.

use super::{Context, Intent};
use crate::output::{Finish, Sink};
use reldir_core::{
    code::Code,
    diagnostic::{DbError, Diagnostic, Result},
    merge::{Labels, MergeError, Merged},
};
use serde_json::{Map, Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const BEGIN: &str = "# reldir: begin -- row files merge member by member and diff as JSON pointers";
const END: &str = "# reldir: end";
const HOOK_MARK: &str = "# reldir: installed by `reldir git setup`";

/// Run git in `root`; its standard output, or the failure it reported.
pub fn git(root: &Path, arguments: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .map_err(|error| {
            DbError::usage(format!("git could not be run: {error}"))
                .with_help("install git, or put it on PATH")
        })?;
    if !output.status.success() {
        return Err(DbError::usage(format!(
            "`git {}` failed: {}",
            arguments.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(output.stdout)
}

fn git_text(root: &Path, arguments: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&git(root, arguments)?)
        .trim_end()
        .to_string())
}

/// The block of `.gitattributes` that routes the tables' row files.
fn attributes(tables: &[String]) -> String {
    let mut block = format!("{BEGIN}\n");
    for table in tables {
        block.push_str(&format!("/{table}/*.json merge=reldir diff=reldir\n"));
    }
    block.push_str(END);
    block.push('\n');
    block
}

/// `existing` with its reldir block replaced by `block`, or `block` added.
fn with_block(existing: &str, block: &str) -> String {
    match (existing.find(BEGIN), existing.find(END)) {
        (Some(start), Some(end)) if end > start => {
            let after = existing[end + END.len()..].trim_start_matches('\n');
            format!("{}{block}{after}", &existing[..start])
        }
        _ if existing.is_empty() => block.to_string(),
        _ => {
            let separator = if existing.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            format!("{existing}{separator}{block}")
        }
    }
}

/// Configure git for the database: attributes, drivers and, with `hooks`, a
/// check after every merge and checkout.
pub fn setup(context: &Context, sink: &mut dyn Sink, hooks: bool) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    let root = database.root.clone();
    let top = PathBuf::from(git_text(&root, &["rev-parse", "--show-toplevel"]).map_err(
        |error| {
            error.with_help("`reldir git setup` configures the git repository the database is in")
        },
    )?);
    let tables: Vec<String> = database.catalog.schemas.keys().cloned().collect();
    let attributes_path = root.join(".gitattributes");
    let existing = match std::fs::read_to_string(&attributes_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(DbError::io(&attributes_path, error)),
    };
    let updated = with_block(&existing, &attributes(&tables));
    if updated != existing {
        std::fs::write(&attributes_path, &updated)
            .map_err(|error| DbError::io(&attributes_path, error))?;
    }
    let settings = [
        (
            "merge.reldir.name",
            "reldir: merge row files member by member",
        ),
        (
            "merge.reldir.driver",
            "reldir --quiet --format table git merge-file %O %A %B --path %P --labels %S %X %Y --marker-size %L",
        ),
        ("diff.reldir.textconv", "reldir --format table git textconv"),
    ];
    for (key, value) in settings {
        git(&root, &["config", "--local", key, value])?;
    }
    let mut installed = vec![];
    if hooks {
        let relative = pathdiff(&root, &top);
        for name in ["post-merge", "post-checkout"] {
            let hook = PathBuf::from(git_text(
                &root,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    &format!("hooks/{name}"),
                ],
            )?);
            match std::fs::read_to_string(&hook) {
                Ok(text) if !text.contains(HOOK_MARK) => {
                    return Err(DbError::usage(format!(
                        "{} exists and is not reldir's; nothing was changed in it",
                        hook.display()
                    ))
                    .with_help(format!(
                        "add `reldir --db {} --readonly --format table check` to it yourself",
                        relative.display()
                    )));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(DbError::io(&hook, error)),
            }
            let script = format!(
                "#!/bin/sh\n{HOOK_MARK}\n# After a {what}, report what no single file shows: rows from two\n# branches that together break a key, a unique value or a reference.\ncd \"$(git rev-parse --show-toplevel)\" || exit 0\nreldir --db '{db}' --readonly --format table check || true\n",
                what = if name == "post-merge" {
                    "merge"
                } else {
                    "checkout"
                },
                db = relative.display(),
            );
            if let Some(parent) = hook.parent() {
                std::fs::create_dir_all(parent).map_err(|error| DbError::io(parent, error))?;
            }
            std::fs::write(&hook, script).map_err(|error| DbError::io(&hook, error))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
                    .map_err(|error| DbError::io(&hook, error))?;
            }
            installed.push(hook.to_string_lossy().into_owned());
        }
    }
    let mut record = Map::new();
    record.insert("kind".into(), json!("git_setup"));
    record.insert("attributes".into(), json!(attributes_path));
    record.insert("tables".into(), json!(tables));
    record.insert(
        "config".into(),
        Value::Object(
            settings
                .iter()
                .map(|(key, value)| (key.to_string(), json!(value)))
                .collect(),
        ),
    );
    record.insert("hooks".into(), json!(installed));
    sink.record(record)?;
    Ok(Finish::ok(format!(
        "git merges and diffs {} table(s) as JSON",
        tables.len()
    )))
}

/// `path` relative to `base`, both absolute; `path` itself when it is not
/// under `base`.
fn pathdiff(path: &Path, base: &Path) -> PathBuf {
    match path.strip_prefix(base) {
        Ok(relative) if relative.as_os_str().is_empty() => PathBuf::from("."),
        Ok(relative) => relative.to_path_buf(),
        Err(_) => path.to_path_buf(),
    }
}

pub struct MergeFile<'a> {
    pub base: &'a Path,
    pub ours: &'a Path,
    pub theirs: &'a Path,
    pub path: Option<&'a str>,
    pub labels: &'a [String],
    pub marker_size: usize,
}

fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).map_err(|error| DbError::io(path, error))
}

/// git's merge driver: merge the three versions of a row file into `ours`.
/// A conflict leaves markers in it and fails, as git expects.
pub fn merge_file(sink: &mut dyn Sink, files: MergeFile<'_>) -> Result<Finish> {
    let name = files.path.unwrap_or("the file");
    let labels = match files.labels {
        // A git before 2.44 passes the placeholders `git setup` writes as
        // they are: there are no names to give the sides.
        [base, ours, theirs] if [base, ours, theirs] == ["%S", "%X", "%Y"] => Labels {
            marker_size: files.marker_size,
            ..Labels::default()
        },
        [base, ours, theirs] => Labels {
            base: base.clone(),
            ours: ours.clone(),
            theirs: theirs.clone(),
            marker_size: files.marker_size,
        },
        [] => Labels {
            marker_size: files.marker_size,
            ..Labels::default()
        },
        other => {
            return Err(DbError::usage(format!(
                "--labels names the base, ours and theirs: three labels, not {}",
                other.len()
            )));
        }
    };
    let (base, ours, theirs) = (read(files.base)?, read(files.ours)?, read(files.theirs)?);
    match reldir_core::merge::merge_documents(&base, &ours, &theirs, &labels) {
        Ok(Merged::Clean(bytes)) => {
            std::fs::write(files.ours, bytes).map_err(|error| DbError::io(files.ours, error))?;
            Ok(Finish::ok(format!("{name} merged member by member")))
        }
        Ok(Merged::Conflicted { bytes, conflicts }) => {
            std::fs::write(files.ours, bytes).map_err(|error| DbError::io(files.ours, error))?;
            let related: Vec<Diagnostic> = conflicts
                .iter()
                .map(|conflict| {
                    Diagnostic::error(
                        Code::MERGE_CONFLICT,
                        format!("both sides changed {} differently", conflict.pointer),
                    )
                    .pointer(conflict.pointer.clone())
                })
                .collect();
            let pointers: Vec<&str> = conflicts
                .iter()
                .map(|conflict| conflict.pointer.as_str())
                .collect();
            Err(DbError::from_diag(
                Diagnostic::error(
                    Code::MERGE_CONFLICT,
                    format!(
                        "{name}: both sides changed {} differently; each is marked in the file",
                        pointers.join(", ")
                    ),
                )
                .help("keep one side of each conflict and delete its markers"),
            )
            .with_related(related))
        }
        Err(MergeError::Invalid { side, reason }) => {
            // Not JSON documents: git's own line merge does what it can.
            sink.note(&format!(
                "{name}: {side} is not a JSON document ({reason}); merging it as text"
            ));
            text_merge(&files, &labels)
        }
    }
}

fn text_merge(files: &MergeFile<'_>, labels: &Labels) -> Result<Finish> {
    let marker = format!("--marker-size={}", labels.marker_size);
    let output = Command::new("git")
        .args([
            "merge-file",
            "-p",
            "--diff3",
            &marker,
            "-L",
            &labels.ours,
            "-L",
            &labels.base,
            "-L",
            &labels.theirs,
        ])
        .arg(files.ours)
        .arg(files.base)
        .arg(files.theirs)
        .output()
        .map_err(|error| DbError::usage(format!("git could not be run: {error}")))?;
    std::fs::write(files.ours, &output.stdout).map_err(|error| DbError::io(files.ours, error))?;
    match output.status.code() {
        Some(0) => Ok(Finish::ok("merged as text")),
        Some(count) if count > 0 => Err(DbError::new(
            Code::MERGE_CONFLICT,
            format!("{count} conflict(s) merging the file as text; each is marked in it"),
        )),
        _ => Err(DbError::usage(format!(
            "git merge-file failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

/// Every leaf of a document as a line, `pointer = value`, in document order.
fn leaves(pointer: &str, value: &Value, out: &mut String) {
    match value {
        Value::Object(members) if !members.is_empty() => {
            for (name, member) in members {
                leaves(
                    &format!(
                        "{pointer}/{}",
                        reldir_core::schema::path::escape_pointer(name)
                    ),
                    member,
                    out,
                );
            }
        }
        Value::Array(items) if !items.is_empty() => {
            for (index, item) in items.iter().enumerate() {
                leaves(&format!("{pointer}/{index}"), item, out);
            }
        }
        other => {
            let at = if pointer.is_empty() { "/" } else { pointer };
            out.push_str(&format!(
                "{at} = {}\n",
                reldir_core::canonical::compact(other)
            ));
        }
    }
}

/// git's diff text conversion: a row file as one line per value, so a diff
/// names the members that changed. A file that is not JSON is shown as it is.
pub fn textconv(sink: &mut dyn Sink, file: &Path) -> Result<Finish> {
    let bytes = read(file)?;
    let text = match reldir_core::json::parse(&bytes) {
        Ok(value) => {
            let mut out = String::new();
            leaves("", &value, &mut out);
            out
        }
        Err(_) => String::from_utf8_lossy(&bytes).into_owned(),
    };
    sink.text("textconv", &file.to_string_lossy(), &text)?;
    Ok(Finish::ok(String::new()))
}

/// The files a git revision holds under the database root, by path relative
/// to it: each table's rows and each schema.
pub fn files_at(root: &Path, revision: &str, tables: &[String]) -> Result<Vec<(String, Vec<u8>)>> {
    let listed = git(
        root,
        &["ls-tree", "-r", "-z", "--name-only", revision, "--", "."],
    )
    .map_err(|error| error.with_help("name a revision git knows: a commit, a branch or a tag"))?;
    let prefix = git_text(root, &["rev-parse", "--show-prefix"])?;
    let wanted = |path: &str| {
        let Some(name) = path.strip_suffix(".json") else {
            return false;
        };
        let parts: Vec<&str> = name.split('/').collect();
        match parts.as_slice() {
            [table, _] => tables.iter().any(|known| known == table) || *table == "schema",
            [".db", "schema", _] => true,
            _ => false,
        }
    };
    let paths: Vec<String> = listed
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .filter(|path| wanted(path))
        .collect();
    if paths.is_empty() {
        return Ok(vec![]);
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| DbError::usage(format!("git could not be run: {error}")))?;
    let mut stdin = child.stdin.take().expect("piped");
    let requests: String = paths
        .iter()
        .map(|path| format!("{revision}:{prefix}{path}\n"))
        .collect();
    let writer = std::thread::spawn(move || stdin.write_all(requests.as_bytes()));
    let mut reader = BufReader::new(child.stdout.take().expect("piped"));
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let mut header = String::new();
        reader
            .read_line(&mut header)
            .map_err(|error| DbError::usage(format!("git cat-file failed: {error}")))?;
        let size: usize = header
            .split_whitespace()
            .nth(2)
            .and_then(|size| size.parse().ok())
            .ok_or_else(|| {
                DbError::usage(format!(
                    "git cat-file could not read {path}: {}",
                    header.trim()
                ))
            })?;
        let mut bytes = vec![0; size + 1];
        reader
            .read_exact(&mut bytes)
            .map_err(|error| DbError::usage(format!("git cat-file failed: {error}")))?;
        bytes.truncate(size);
        out.push((path, bytes));
    }
    writer
        .join()
        .map_err(|_| DbError::usage("writing to git cat-file failed"))?
        .map_err(|error| DbError::usage(format!("writing to git cat-file failed: {error}")))?;
    child
        .wait()
        .map_err(|error| DbError::usage(format!("git cat-file failed: {error}")))?;
    Ok(out)
}
