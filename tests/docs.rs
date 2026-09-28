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
//! pages that begin from a bare folder), and otherwise `tests/docs/default/`,
//! established before the page runs.
//!
//! Beside `reldir`, a transcript may use `ls`, `rm`, `cat` and
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
    let mut out: Vec<PathBuf> = fs::read_dir("docs")
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    out.push(PathBuf::from("README.md"));
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

/// Every string used as a diagnostic code in the source.
fn emitted_codes() -> std::collections::BTreeSet<String> {
    let pattern = regex::Regex::new(
        r#"(?:Diagnostic::(?:error|warning|suggestion|info)|DbError::new|column_error|fault|refused|fail|incomplete)\(\s*"([A-Z][A-Z0-9_]+)""#,
    )
    .unwrap();
    let mut out = std::collections::BTreeSet::new();
    let mut stack = vec![PathBuf::from("src")];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = fs::read_to_string(&path).unwrap();
                let body = text.split("#[cfg(test)]").next().unwrap_or(&text);
                for capture in pattern.captures_iter(body) {
                    out.insert(capture[1].to_string());
                }
            }
        }
    }
    out
}

fn documented(page: &str) -> String {
    fs::read_to_string(page).unwrap()
}

#[test]
fn test4002_every_code_the_binary_emits_is_documented() {
    let errors = documented("docs/errors.md");
    let validation = documented("docs/validation.md");
    let missing: Vec<String> = emitted_codes()
        .into_iter()
        .filter(|code| {
            let spelled = format!("`{code}`");
            !errors.contains(&spelled) && !validation.contains(&spelled)
        })
        .collect();
    assert!(
        missing.is_empty(),
        "codes the binary emits that the documentation does not: {missing:?}"
    );
    assert!(emitted_codes().len() > 60, "the scan finds the codes");
}

#[test]
fn test4003_no_documented_code_is_stale() {
    let emitted = emitted_codes();
    let source: String = {
        let mut all = String::new();
        let mut stack = vec![PathBuf::from("src")];
        while let Some(directory) = stack.pop() {
            for entry in fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    all.push_str(&fs::read_to_string(&path).unwrap());
                }
            }
        }
        all
    };
    let table_code = regex::Regex::new(r"^\| `([A-Z][A-Z0-9_]+)`").unwrap();
    let mut stale = vec![];
    for line in documented("docs/errors.md").lines() {
        if let Some(capture) = table_code.captures(line) {
            let code = &capture[1];
            if !emitted.contains(code) && !source.contains(&format!("\"{code}\"")) {
                stale.push(code.to_string());
            }
        }
    }
    assert!(
        stale.is_empty(),
        "documented codes nothing emits: {stale:?}"
    );
}

#[test]
fn test4004_cross_links_resolve() {
    let link = regex::Regex::new(r"\{\{ site\.baseurl \}\}/([a-z-]+)(?:#([a-z0-9-]+))?").unwrap();
    let heading_anchor = |page: &str, anchor: &str| {
        fs::read_to_string(format!("docs/{page}.md"))
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
            if !Path::new(&format!("docs/{target}.md")).exists() {
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
    let mut stack = vec![
        PathBuf::from("src"),
        PathBuf::from("tests"),
        PathBuf::from("docs"),
    ];
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
    let mut stack = vec![PathBuf::from("src"), PathBuf::from("tests")];
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
