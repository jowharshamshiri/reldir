//! reldir's row validator against the official JSON-Schema-Test-Suite.
//!
//! A table schema is a JSON Schema 2020-12 document and each row is judged by
//! it, so the validator must agree with the standard's own test suite -- the
//! required tests and the optional ones, `format` assertion included, since
//! reldir asserts formats. The suite is vendored under `tests/conformance/`
//! at the commit named in `SOURCE`.
//!
//! Two kinds of case are deliberately judged differently, and each is checked
//! for that behaviour rather than skipped:
//!
//! - a schema that `$ref`s a document outside itself (other than the standard
//!   meta-schemas) is refused with `SCHEMA_REF_EXTERNAL`, because a table's
//!   validity must never depend on a file the database does not govern;
//! - a case listed in `DIVERGENCES` is one where the validator's answer is
//!   known to differ, each with its reason. The list is checked in both
//!   directions: a listed case that starts agreeing fails the test, so the
//!   list cannot rot.

use indexmap::IndexMap;
use reldir::schema::row::RowValidator;
use serde_json::Value;
use std::{fs, path::Path};

/// Cases whose answer is known to differ from the suite's, by
/// `file / case description / test description`.
const DIVERGENCES: &[(&str, &str)] = &[
    (
        "optional/format/duration.json / validation of duration strings / weeks cannot be combined with a time component",
        "the validator library's `duration` check accepts `P1WT1H`",
    ),
    (
        "optional/format/ecmascript-regex.json / Python-specific regular expression syntax is not valid ECMA 262 / Python named group (?P<name>...) is not ECMA 262",
        "the `regex` format is checked with a non-ECMA-262 regex dialect",
    ),
    (
        "optional/format/ecmascript-regex.json / global inline flag groups are not valid ECMA 262 / a single global inline flag (?i)",
        "the `regex` format is checked with a non-ECMA-262 regex dialect",
    ),
    (
        "optional/format/ecmascript-regex.json / global inline flag groups are not valid ECMA 262 / multiple global inline flags (?ims)",
        "the `regex` format is checked with a non-ECMA-262 regex dialect",
    ),
    (
        "optional/format/ecmascript-regex.json / ECMA 262 named groups and backreferences are valid / an ECMA 262 named backreference \\k<name>",
        "the `regex` format is checked with a dialect without backreferences",
    ),
    (
        "optional/format/ecmascript-regex.json / ECMA 262 character classes and escapes / an empty character class is valid ECMA 262",
        "the `regex` format is checked with a dialect that refuses `[]`",
    ),
    (
        "optional/format/ecmascript-regex.json / ECMA 262 character classes and escapes / a negated empty character class is valid ECMA 262",
        "the `regex` format is checked with a dialect that refuses `[^]`",
    ),
    (
        "optional/format/email.json / validation of e-mail addresses / an empty quoted string in the local part is valid",
        "the validator library's `email` check refuses `\"\"@example.com`",
    ),
    (
        "optional/format/email.json / validation of e-mail addresses / a non-ASCII character in the local part is not valid",
        "the validator library's `email` check accepts non-ASCII local parts (that is `idn-email`)",
    ),
    (
        "optional/format/email.json / validation of e-mail addresses / a lowercase IPv6 tag in an address literal is valid",
        "the validator library's `email` check refuses `[ipv6:...]`",
    ),
    (
        "optional/format/uri-template.json / format: uri-template / an apostrophe in a literal is valid",
        "the validator library's `uri-template` check refuses `'` in a literal",
    ),
    (
        "vocabulary.json / schema that uses custom metaschema with with no validation vocabulary / no validation: invalid number, but it still validates",
        "a remote meta-schema that switches vocabularies off is never fetched; a reldir table's `$schema` is always the reldir dialect",
    ),
];

fn files(directory: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "json") {
            out.push(path);
        }
    }
}

/// Whether a case depends on a document served by the suite's remote server,
/// which reldir must refuse to reach.
fn needs_a_remote_document(schema: &Value) -> bool {
    schema.to_string().contains("localhost:1234")
}

/// Whether a test expects `format` to be only an annotation. reldir enables the
/// format-assertion vocabulary, so the answer to these is the opposite.
fn expects_format_as_annotation(test: &str) -> bool {
    test.contains("only an annotation by default")
}

#[test]
fn test6001_rows_are_judged_exactly_as_json_schema_2020_12_judges_them() {
    let mut paths = vec![];
    files(Path::new("tests/conformance/draft2020-12"), &mut paths);
    paths.sort();
    let mut disagreements = vec![];
    let mut resolved_divergences = vec![];
    let mut agreed = 0;
    let mut refused_remote = 0;
    for path in &paths {
        let name = path
            .strip_prefix("tests/conformance/draft2020-12")
            .unwrap()
            .display()
            .to_string();
        let cases: Vec<Value> = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        for case in cases {
            let description = case["description"].as_str().unwrap_or_default();
            let schema = &case["schema"];
            // A remote `$ref` is refused; a remote `$schema` names a
            // meta-schema, which judges the schema rather than the rows, so
            // the rows are judged as usual.
            let validator = match RowValidator::compile(schema, &IndexMap::new()) {
                Ok(validator) => validator,
                Err(diagnostic)
                    if diagnostic.code == "SCHEMA_REF_EXTERNAL"
                        && needs_a_remote_document(schema) =>
                {
                    refused_remote += 1;
                    continue;
                }
                Err(diagnostic) => {
                    disagreements.push(format!(
                        "{name} / {description}: refused as {}: {}",
                        diagnostic.code, diagnostic.message
                    ));
                    continue;
                }
            };
            for test in case["tests"].as_array().unwrap() {
                let label = format!(
                    "{name} / {description} / {}",
                    test["description"].as_str().unwrap_or_default()
                );
                let mut expected = test["valid"].as_bool().unwrap();
                if expects_format_as_annotation(test["description"].as_str().unwrap_or_default()) {
                    expected = !expected;
                }
                let actual = validator.is_valid(&test["data"]);
                let listed = DIVERGENCES.iter().any(|(case, _)| *case == label);
                match (actual == expected, listed) {
                    (true, false) => agreed += 1,
                    (true, true) => resolved_divergences.push(label),
                    (false, true) => {}
                    (false, false) => disagreements
                        .push(format!("{label}: expected valid={expected}, got {actual}")),
                }
            }
        }
    }
    assert!(agreed > 1500, "the suite ran: {agreed} tests agreed");
    assert!(refused_remote > 0, "remote references were exercised");
    assert!(
        resolved_divergences.is_empty(),
        "listed divergences that now agree; remove them: {resolved_divergences:#?}"
    );
    assert!(
        disagreements.is_empty(),
        "{} disagreement(s):\n{}",
        disagreements.len(),
        disagreements.join("\n")
    );
}
