//! Entry points for fuzzing: each takes arbitrary bytes, exercises one of the
//! places untrusted input enters, and panics only when an invariant breaks.
//!
//! The fuzz targets in `fuzz/` call these under libFuzzer; the test suite
//! calls them with generated input on every run, so a regression is caught
//! without a fuzzing toolchain.

use crate::{
    canonical,
    locate::Spans,
    schema::{RefPath, Schema},
};
use serde_json::{Value, json};

/// Parsing never panics, and every value a parsed document holds is located
/// within the input.
pub fn json_and_locator(data: &[u8]) {
    let spans = Spans::of(data);
    let Ok(value) = crate::json::with_depth_limit(64, || crate::json::parse(data)) else {
        return;
    };
    let lines = data.split(|byte| *byte == b'\n').count();
    let mut pointers = vec![];
    collect_pointers(&value, String::new(), &mut pointers);
    for pointer in pointers {
        let location = spans
            .value_location(&pointer)
            .unwrap_or_else(|| panic!("{pointer:?} is in the document but was not located"));
        assert!(
            location.line >= 1 && location.line <= lines,
            "{pointer:?} located outside the input"
        );
        assert!(location.column >= 1, "columns count from 1");
    }
}

fn collect_pointers(value: &Value, at: String, out: &mut Vec<String>) {
    match value {
        Value::Object(members) => {
            for (key, child) in members {
                collect_pointers(
                    child,
                    format!("{at}/{}", crate::schema::path::escape_pointer(key)),
                    out,
                );
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                collect_pointers(child, format!("{at}/{index}"), out);
            }
        }
        _ => {}
    }
    out.push(at);
}

/// Decoding a schema reports faults and never panics; a schema that decodes
/// reads back from its own rendering with the same identity.
pub fn schema_decode(data: &[u8]) {
    let Ok(schema) = crate::json::with_depth_limit(64, || Schema::from_bytes(data)) else {
        return;
    };
    let again =
        Schema::from_bytes(&schema.bytes(2)).expect("a schema reads back from its own rendering");
    assert_eq!(
        schema.identity(),
        again.identity(),
        "rendering a schema does not change it"
    );
}

/// A reference path that parses prints back to text that parses to itself.
pub fn reference_path(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(path) = RefPath::parse(text) else {
        return;
    };
    let printed = path.to_string();
    let reparsed = RefPath::parse(&printed)
        .unwrap_or_else(|error| panic!("{printed:?} does not parse: {error}"));
    assert_eq!(reparsed, path, "{text:?} printed as {printed:?}");
}

thread_local! {
    static SQL: (tempfile::TempDir, crate::catalog::Catalog) = sql_database();
}

fn sql_database() -> (tempfile::TempDir, crate::catalog::Catalog) {
    let directory = tempfile::tempdir().expect("a scratch directory");
    let root = directory.path();
    std::fs::create_dir_all(root.join(".db/schema")).expect("scratch layout");
    std::fs::create_dir_all(root.join("t")).expect("scratch layout");
    let document = json!({
        "$schema": crate::schema::meta::DIALECT_URI, "type": "object",
        "properties": {"id": {"type": "string"}, "n": {"type": "integer", "x-reldir-type": "int"}, "tags": {"type": "array"}},
        "required": ["id"], "additionalProperties": false,
        "x-reldir": {"table": "t", "primaryKey": ["id"]}
    });
    std::fs::write(root.join(".db/schema/t.json"), document.to_string()).expect("scratch schema");
    for (id, n) in [("a", 1), ("b", 2), ("c", 3)] {
        std::fs::write(
            root.join(format!("t/{id}.json")),
            json!({"id": id, "n": n, "tags": [id]}).to_string(),
        )
        .expect("scratch row");
    }
    let catalog = crate::catalog::Catalog::observe(
        root,
        &crate::config::Config::default(),
        &crate::fs::Disk,
        std::rc::Rc::new(crate::mirror::Mirror::open_memory().expect("an in-memory mirror")),
        false,
    )
    .expect("the scratch database observes");
    (directory, catalog)
}

/// SQL is classified, authorized and run or refused -- never a panic -- and a
/// mutation never changes the mirror it was computed on.
pub fn sql_front_end(data: &[u8]) {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    SQL.with(|(_, catalog)| {
        let limits = crate::sql::QueryLimits {
            timeout: Some(std::time::Duration::from_millis(200)),
            max_rows: 1000,
            max_memory: 64 << 20,
        };
        let known = [
            "QUERY_UNSUPPORTED",
            "QUERY_TYPE_ERROR",
            "UNKNOWN_TABLE",
            "UNKNOWN_COLUMN",
            "RESOURCE_LIMIT",
            "INTERNAL_METADATA_CORRUPT",
        ];
        let judged = match crate::sql::classify(text) {
            Ok(crate::sql::StatementKind::Read) => {
                crate::sql::query(&catalog.mirror, &catalog.schemas, text, &[], limits, |_| {
                    Ok(())
                })
                .map(|_| ())
            }
            Ok(crate::sql::StatementKind::Mutation) => {
                crate::sql::mutate(catalog, text, &[], limits).map(|_| ())
            }
            Err(error) => Err(error),
        };
        if let Err(error) = judged {
            assert!(
                known.contains(&error.diagnostic.code.as_str()),
                "{text:?} failed as {}, which a statement should not produce",
                error.diagnostic.code
            );
        }
        let rows = catalog.mirror.count("t").expect("the mirror still answers");
        assert_eq!(rows, 3, "{text:?} changed the mirror");
    });
}

/// Any key has a file name that is safe on every filesystem and decodes back
/// to the key.
pub fn file_name(data: &[u8]) {
    let Ok(key) = std::str::from_utf8(data) else {
        return;
    };
    if key.is_empty() {
        return;
    }
    let schema = Schema::from_document(
        json!({
            "$schema": crate::schema::meta::DIALECT_URI, "type": "object",
            "properties": {"id": {"type": "string"}}, "required": ["id"],
            "additionalProperties": false, "x-reldir": {"table": "t", "primaryKey": ["id"]}
        }),
        None,
    )
    .expect("the probe schema is valid");
    let name = canonical::filename(&schema, json!({"id": key}).as_object().expect("an object"))
        .expect("a present key has a file name");
    assert!(!name.starts_with('.'), "{key:?} became a hidden file");
    assert!(
        !name.contains('/') && !name.contains('\\') && !name.contains('\0'),
        "{key:?} escaped its directory"
    );
    let stem = name
        .strip_suffix(".json")
        .expect("a row file ends in .json");
    assert_eq!(
        canonical::percent_decode(stem).as_deref(),
        Some(key),
        "{key:?} does not decode back"
    );
    assert_eq!(
        canonical::filename_fits(&name),
        name.len() <= canonical::MAX_FILENAME_BYTES
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, .. ProptestConfig::default() })]

        #[test]
        fn test7101_arbitrary_bytes_never_break_the_json_front_end(data in prop::collection::vec(any::<u8>(), 0..200)) {
            json_and_locator(&data);
        }

        #[test]
        fn test7102_arbitrary_documents_are_located_everywhere(value in json_like()) {
            json_and_locator(serde_json::to_string_pretty(&value).unwrap().as_bytes());
            json_and_locator(value.to_string().as_bytes());
        }

        #[test]
        fn test7103_arbitrary_schema_files_never_panic(data in prop::collection::vec(any::<u8>(), 0..200)) {
            schema_decode(&data);
        }

        #[test]
        fn test7104_arbitrary_paths_round_trip(text in "[a-z_.\\[\\]?='\"0-9 ]{0,24}") {
            reference_path(text.as_bytes());
        }

        #[test]
        fn test7105_arbitrary_sql_is_refused_or_run(text in "(SELECT|INSERT|UPDATE|DELETE|WITH|PRAGMA|ATTACH|DROP)?[ a-z0-9_*(),.=<>'\"]{0,40}") {
            sql_front_end(text.as_bytes());
        }

        #[test]
        fn test7106_arbitrary_keys_have_safe_file_names(key in "\\PC{1,40}") {
            file_name(key.as_bytes());
        }
    }

    fn json_like() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<i32>().prop_map(|n| json!(n)),
            "[a-z/~\"é]{0,5}".prop_map(Value::String)
        ];
        leaf.prop_recursive(3, 16, 3, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..3).prop_map(Value::Array),
                prop::collection::btree_map("[a-c/~]{1,2}", inner, 0..3)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
            ]
        })
    }

    #[test]
    fn test7107_the_sql_entry_point_admits_and_refuses_as_the_binary_does() {
        sql_front_end(b"SELECT count(*) FROM t");
        sql_front_end(b"DELETE FROM t");
        sql_front_end(b"PRAGMA table_info(t)");
        sql_front_end(b"SELECT * FROM _reldir_files");
    }
}
