//! The documentation is executed.
//!
//! Every ```` ```console ```` block in `docs/*.md` and `README.md` is a transcript:
//! each `$ ` line is run, and what follows it, up to the next `$ ` line, is the
//! output it must produce (stderr, then stdout). A page's transcripts run in
//! order in one copy of a fixture database, so a page reads as a session.
//!
//! In expected output, a line that is exactly `[..]` matches any number of
//! lines, and `[..]` inside a line matches any text -- for timings, hashes and
//! the tails of long messages.
//!
//! The fixture is `tests/docs/<page>/` when it exists, used as it stands (for
//! pages that begin from a bare folder, or from an empty one: a fixture of a
//! `.gitkeep` alone, which is not copied), and otherwise `tests/docs/default/`,
//! established before the page runs.
//!
//! Beside `reldir`, a transcript may use `ls`, `mkdir`, `rm`, `cat` and
//! `echo TEXT > FILE`. `reldir` runs with `--format table` unless the command
//! chooses a format, because the documentation shows what a terminal shows.
//!
//! The meta-tests below keep the documentation honest in the other direction:
//! every code the binary can emit is documented, no documented code is stale,
//! every cross-link resolves, and nothing cites the retired specification.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_reldir"))
}

fn pages() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(repo("docs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    out.push(repo("README.md"));
    out.sort();
    out
}

/// Split a command line into words, honouring single and double quotes.
fn words(line: &str) -> Vec<String> {
    let mut out = vec![];
    let mut current = String::new();
    let mut started = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                started = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    current.push(c);
                }
            }
            '"' => {
                started = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' => {
                            if let Some(next) = chars.next() {
                                current.push(next);
                            }
                        }
                        other => current.push(other),
                    }
                }
            }
            '\\' => {
                started = true;
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            c if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            other => {
                started = true;
                current.push(other);
            }
        }
    }
    if started {
        out.push(current);
    }
    out
}

struct Step {
    command: String,
    expected: Vec<String>,
    line: usize,
}

fn transcripts(text: &str) -> Vec<Step> {
    let mut steps: Vec<Step> = vec![];
    let mut in_console = false;
    for (index, raw) in text.lines().enumerate() {
        let trimmed = raw.trim_end();
        if !in_console {
            if trimmed.trim_start() == "```console" {
                in_console = true;
            }
            continue;
        }
        if trimmed.trim_start() == "```" {
            in_console = false;
            continue;
        }
        if let Some(command) = trimmed.strip_prefix("$ ") {
            steps.push(Step {
                command: command.to_string(),
                expected: vec![],
                line: index + 1,
            });
        } else if let Some(step) = steps.last_mut() {
            step.expected.push(trimmed.to_string());
        }
    }
    steps
}

/// Whether `line` matches `pattern`, where `[..]` matches any text.
fn line_matches(pattern: &str, line: &str) -> bool {
    let parts: Vec<&str> = pattern.split("[..]").collect();
    if parts.len() == 1 {
        return pattern == line;
    }
    let mut rest = line;
    for (index, part) in parts.iter().enumerate() {
        if index == 0 {
            match rest.strip_prefix(part) {
                Some(after) => rest = after,
                None => return false,
            }
        } else if index == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
    }
    true
}

/// Whether `lines` match `patterns`, where a `[..]` line matches any run.
fn lines_match(patterns: &[String], lines: &[String]) -> bool {
    match patterns.split_first() {
        None => lines.is_empty(),
        Some((first, rest)) if first == "[..]" => {
            (0..=lines.len()).any(|skip| lines_match(rest, &lines[skip..]))
        }
        Some((first, rest)) => {
            !lines.is_empty() && line_matches(first, &lines[0]) && lines_match(rest, &lines[1..])
        }
    }
}

fn run_step(root: &Path, command: &str) -> Vec<String> {
    let mut words = words(command);
    if let Some(at) = words.iter().position(|word| word == ">") {
        assert_eq!(
            words[0], "echo",
            "only `echo ... > FILE` redirects: {command}"
        );
        let text = words[1..at].join(" ");
        fs::write(root.join(&words[at + 1]), format!("{text}\n")).unwrap();
        return vec![];
    }
    match words[0].as_str() {
        "ls" => {
            let directory = root.join(words.get(1).map(String::as_str).unwrap_or("."));
            let mut names: Vec<String> = fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|name| !name.starts_with('.'))
                .collect();
            names.sort();
            names
        }
        "mkdir" => {
            for path in &words[1..] {
                fs::create_dir(root.join(path))
                    .unwrap_or_else(|error| panic!("mkdir {path}: {error}"));
            }
            vec![]
        }
        "rm" => {
            for path in &words[1..] {
                fs::remove_file(root.join(path))
                    .unwrap_or_else(|error| panic!("rm {path}: {error}"));
            }
            vec![]
        }
        "cat" => fs::read_to_string(root.join(&words[1]))
            .unwrap()
            .lines()
            .map(String::from)
            .collect(),
        "reldir" => {
            let mut arguments: Vec<String> = words.split_off(1);
            if !arguments.iter().any(|a| a == "--format" || a == "--json") {
                arguments.insert(0, "table".into());
                arguments.insert(0, "--format".into());
            }
            let output = Command::new(binary())
                .args(&arguments)
                .current_dir(root)
                .env_remove("RELDIR_DB")
                .env("NO_COLOR", "1")
                .output()
                .unwrap();
            let mut lines: Vec<String> = String::from_utf8_lossy(&output.stderr)
                .lines()
                .map(|l| l.trim_end().to_string())
                .collect();
            lines.extend(
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(|l| l.trim_end().to_string()),
            );
            lines
        }
        other => panic!("a transcript may not run {other:?}: {command}"),
    }
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().is_some_and(|name| name == ".gitkeep") {
            continue;
        }
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_tree(&path, &target);
        } else {
            fs::copy(&path, &target).unwrap();
        }
    }
}

#[test]
fn test4001_every_console_transcript_in_the_documentation_is_true() {
    let mut failures = vec![];
    let mut ran = 0;
    for page in pages() {
        let steps = transcripts(&fs::read_to_string(&page).unwrap());
        if steps.is_empty() {
            continue;
        }
        let stem = page.file_stem().unwrap().to_string_lossy().to_string();
        let own = Path::new("tests/docs").join(&stem);
        let sandbox = tempfile::tempdir().unwrap();
        let root = sandbox.path().join("db");
        if own.is_dir() {
            copy_tree(&own, &root);
        } else {
            copy_tree(Path::new("tests/docs/default"), &root);
            let established = Command::new(binary())
                .args(["--quiet", "status"])
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                established.status.success(),
                "the default fixture is a valid database"
            );
        }
        for step in steps {
            ran += 1;
            let actual = run_step(&root, &step.command);
            if !lines_match(&step.expected, &actual) {
                failures.push(format!(
                    "{}:{}: $ {}\n--- expected\n{}\n--- actual\n{}\n",
                    page.display(),
                    step.line,
                    step.command,
                    step.expected.join("\n"),
                    actual.join("\n")
                ));
            }
        }
    }
    assert!(ran > 20, "the documentation shows its commands working");
    assert!(
        failures.is_empty(),
        "{} transcript(s) are false:\n\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// The generated sections of the error reference, rendered from the code
/// registry: the table of exit statuses and, per group, every code with its
/// exit status and meaning.
fn error_reference() -> Vec<(&'static str, String)> {
    use reldir_core::code::{Code, Exit};
    let mut exits = String::from("| Status | Meaning |\n|---|---|\n");
    for exit in Exit::ALL {
        exits.push_str(&format!("| `{}` | {} |\n", exit.code(), exit.meaning()));
    }
    let mut codes = String::new();
    // Each group once, where its first code is.
    let mut groups = vec![];
    for code in Code::ALL {
        if !groups.contains(&code.group()) {
            groups.push(code.group());
        }
    }
    for group in groups {
        codes.push_str(&format!(
            "### {}\n\n| Code | Exit | Meaning |\n|---|---|---|\n",
            group.title()
        ));
        for code in Code::ALL.iter().filter(|code| code.group() == group) {
            // The anchor a SARIF rule's helpUri names.
            codes.push_str(&format!(
                "| <a id=\"{}\"></a>`{code}` | `{}` | {} |\n",
                code.as_str().to_lowercase(),
                code.exit().code(),
                code.summary()
            ));
        }
        codes.push('\n');
    }
    vec![
        ("exits", exits),
        ("codes", codes.trim_end().to_string() + "\n"),
    ]
}

/// The error reference is the registry, verbatim: every code and status is
/// documented, nothing documented is missing from the registry, and an exit
/// status can never be described differently in two places. Run with
/// `RELDIR_BLESS=1` to regenerate the sections after changing the registry.
#[test]
fn test4002_the_error_reference_is_the_code_registry() {
    let path = repo("docs/errors.md");
    let mut text = fs::read_to_string(&path).unwrap();
    let mut stale = vec![];
    for (name, expected) in error_reference() {
        let begin = format!("<!-- generated:{name} -->\n");
        let end = format!("<!-- end:{name} -->");
        let start = text
            .find(&begin)
            .unwrap_or_else(|| panic!("docs/errors.md has no {begin:?} marker"))
            + begin.len();
        let stop = start
            + text[start..]
                .find(&end)
                .unwrap_or_else(|| panic!("docs/errors.md has no {end:?} marker"));
        if text[start..stop] != expected {
            stale.push(name);
            text.replace_range(start..stop, &expected);
        }
    }
    if std::env::var_os("RELDIR_BLESS").is_some() {
        fs::write(&path, text).unwrap();
        return;
    }
    assert!(
        stale.is_empty(),
        "docs/errors.md differs from the code registry in {stale:?}; run the docs tests with \
         RELDIR_BLESS=1 to regenerate it"
    );
}

/// Every registered code is raised by some code path, so the reference never
/// documents a failure that cannot happen.
#[test]
fn test4003_every_registered_code_is_raised() {
    let mut source = String::new();
    let mut stack = vec![repo("crates")];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if !path.ends_with("tests") {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") && !path.ends_with("code.rs")
            {
                let text = fs::read_to_string(&path).unwrap();
                source.push_str(text.split("#[cfg(test)]").next().unwrap_or(&text));
            }
        }
    }
    let unraised: Vec<&str> = reldir_core::code::Code::ALL
        .iter()
        .map(|code| code.as_str())
        .filter(|code| {
            let used = regex::Regex::new(&format!(r"Code::{code}\b")).unwrap();
            !used.is_match(&source)
        })
        .collect();
    assert!(
        unraised.is_empty(),
        "registered codes nothing raises: {unraised:?}"
    );
}

#[test]
fn test4004_cross_links_resolve() {
    let link = regex::Regex::new(r"\{\{ site\.baseurl \}\}/([a-z-]+)(?:#([a-z0-9-]+))?").unwrap();
    let heading_anchor = |page: &str, anchor: &str| {
        fs::read_to_string(repo(&format!("docs/{page}.md")))
            .unwrap()
            .lines()
            .any(|line| {
                let heading = line.trim_start_matches('#');
                line.starts_with('#')
                    && heading
                        .trim()
                        .to_lowercase()
                        .chars()
                        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
                        .collect::<String>()
                        .replace(' ', "-")
                        == anchor
            })
    };
    let mut broken = vec![];
    for page in pages() {
        let text = fs::read_to_string(&page).unwrap();
        for capture in link.captures_iter(&text) {
            let target = &capture[1];
            if !repo(&format!("docs/{target}.md")).exists() {
                broken.push(format!("{}: {target}", page.display()));
            } else if let Some(anchor) = capture.get(2)
                && !heading_anchor(target, anchor.as_str())
            {
                broken.push(format!("{}: {target}#{}", page.display(), anchor.as_str()));
            }
        }
    }
    assert!(broken.is_empty(), "links to nothing: {broken:?}");
}

#[test]
fn test4005_nothing_cites_the_retired_specification() {
    let citation =
        regex::Regex::new(r"Section \d+|spec\.md|dev_exp_improvements|gaps\.md|\.scrap").unwrap();
    let mut found = vec![];
    let mut stack = vec![repo("crates"), repo("docs")];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|ext| ext == "rs" || ext == "md")
                && !path.ends_with("docs.rs")
            {
                let text = fs::read_to_string(&path).unwrap();
                for (number, line) in text.lines().enumerate() {
                    if citation.is_match(line) {
                        found.push(format!(
                            "{}:{}: {}",
                            path.display(),
                            number + 1,
                            line.trim()
                        ));
                    }
                }
            }
        }
    }
    assert!(
        found.is_empty(),
        "the documentation is complete on its own; these cite something retired:\n{}",
        found.join("\n")
    );
}

#[test]
fn test4006_every_test_ordinal_is_unique() {
    let ordinal = regex::Regex::new(r"fn (test\d{4})_").unwrap();
    let mut seen: std::collections::BTreeMap<String, String> = Default::default();
    let mut duplicates = vec![];
    let mut stack = vec![repo("crates")];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                for capture in ordinal.captures_iter(&fs::read_to_string(&path).unwrap()) {
                    if let Some(previous) =
                        seen.insert(capture[1].to_string(), path.display().to_string())
                    {
                        duplicates.push(format!(
                            "{} in {} and {}",
                            &capture[1],
                            previous,
                            path.display()
                        ));
                    }
                }
            }
        }
    }
    assert!(duplicates.is_empty(), "{duplicates:?}");
}

/// A path relative to the repository root, wherever the test runs from.
fn repo(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

/// Every code's SARIF rule links to its entry in the error reference, which
/// holds an anchor for each, once.
#[test]
fn test4007_every_help_link_lands_on_its_code() {
    let reference = fs::read_to_string(repo("docs/errors.md")).unwrap();
    for code in reldir_core::code::Code::ALL {
        let anchor = format!("id=\"{}\"", code.as_str().to_lowercase());
        assert_eq!(
            reference.matches(&anchor).count(),
            1,
            "docs/errors.md holds {anchor} not exactly once"
        );
    }
    let headings: Vec<&str> = reference
        .lines()
        .filter(|line| line.starts_with("### "))
        .collect();
    let mut unique = headings.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        headings.len(),
        "a group appears twice: {headings:?}"
    );
}

/// The protocol page shows the protocol's JSON Schema exactly as `reldir
/// protocol schema` prints it. Run with `RELDIR_BLESS=1` to regenerate it
/// after changing the protocol.
#[test]
fn test4008_the_protocol_page_is_the_protocol_schema() {
    let output = Command::new(binary())
        .args(["--format", "json", "protocol", "schema"])
        .env_remove("RELDIR_DB")
        .output()
        .unwrap();
    let envelope: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let schema = &envelope["records"][0]["document"];
    let expected = format!(
        "```json\n{}\n```\n",
        serde_json::to_string_pretty(schema).unwrap()
    );
    let path = repo("docs/protocol.md");
    let mut text = fs::read_to_string(&path).unwrap();
    let begin = "<!-- generated:schema -->\n";
    let end = "<!-- end:schema -->";
    let start = text
        .find(begin)
        .unwrap_or_else(|| panic!("docs/protocol.md has no {begin:?} marker"))
        + begin.len();
    let stop = start
        + text[start..]
            .find(end)
            .unwrap_or_else(|| panic!("docs/protocol.md has no {end:?} marker"));
    if text[start..stop] == expected {
        return;
    }
    text.replace_range(start..stop, &expected);
    if std::env::var_os("RELDIR_BLESS").is_some() {
        fs::write(&path, text).unwrap();
        return;
    }
    panic!(
        "docs/protocol.md differs from `reldir protocol schema`; run the docs tests with \
         RELDIR_BLESS=1 to regenerate it"
    );
}
