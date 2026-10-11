//! Entry points for fuzzing: each takes arbitrary bytes, exercises one of the
//! places untrusted input enters, and panics only when an invariant breaks.
//!
//! The fuzz targets in `fuzz/` call these under libFuzzer; the test suite
//! calls them with generated input on every run, so a regression is caught
//! without a fuzzing toolchain.

use crate::code::Code;
use crate::{
    locate::Spans,
    naming::{self, KeyText, NamingRule},
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
    let rendered = schema.render(None).expect("a schema that decoded renders");
    let again = Schema::from_bytes(&rendered).expect("a schema reads back from its own rendering");
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
        "x-reldir": {"table": "t", "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}}}
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
        &crate::engine::fs::Disk,
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
            cancel: None,
            max_rows: 1000,
            max_memory: 64 << 20,
        };
        let known = [
            Code::QUERY_UNSUPPORTED,
            Code::QUERY_TYPE_ERROR,
            Code::UNKNOWN_TABLE,
            Code::UNKNOWN_COLUMN,
            Code::RESOURCE_LIMIT,
            Code::INTERNAL_METADATA_CORRUPT,
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
                known.contains(&error.diagnostic.code),
                "{text:?} failed as {}, which a statement should not produce",
                error.diagnostic.code
            );
        }
        let rows = catalog.mirror.count("t").expect("the mirror still answers");
        assert_eq!(rows, 3, "{text:?} changed the mirror");
    });
}

/// Any key under reldir's naming rule either has a file name -- safe on
/// every filesystem, within the length every filesystem allows, and parsing
/// back to the key -- or is refused with the reason; never a panic.
pub fn file_name(data: &[u8]) {
    let Ok(key) = std::str::from_utf8(data) else {
        return;
    };
    let text = KeyText::of(&json!(key)).expect("a string is a key value");
    let Ok(stem) = naming::render(&NamingRule::Reldir, std::slice::from_ref(&text)) else {
        return;
    };
    let name = format!("{stem}{}", naming::EXTENSION);
    assert!(!name.starts_with('.'), "{key:?} became a hidden file");
    assert!(
        !name.contains('/') && !name.contains('\\') && !name.contains('\0'),
        "{key:?} escaped its directory"
    );
    assert!(
        naming::fits(&name, crate::canonical::MAX_FILENAME_BYTES),
        "{key:?} was given a name longer than every filesystem allows"
    );
    assert_eq!(
        naming::parse(&NamingRule::Reldir, &stem).ok(),
        Some(vec![key.to_string()]),
        "{key:?} does not parse back from {stem:?}"
    );
}

/// Three documents, as the merge driver gets them: any bytes, split at the
/// first two `0xFF` bytes (which UTF-8 never holds). A merge never panics;
/// a clean one is a JSON document, and a conflicted one holds markers, and
/// keeping "ours" throughout leaves a JSON document.
pub fn merge(data: &[u8]) {
    let mut parts = data.splitn(3, |byte| *byte == 0xFF);
    let (Some(base), Some(ours), Some(theirs)) = (parts.next(), parts.next(), parts.next()) else {
        return;
    };
    let labels = crate::merge::Labels::default();
    match crate::merge::merge_documents(base, ours, theirs, &labels) {
        Ok(crate::merge::Merged::Clean(bytes)) => {
            crate::json::with_depth_limit(64, || crate::json::parse(&bytes))
                .expect("a clean merge is a JSON document");
        }
        Ok(crate::merge::Merged::Conflicted { bytes, conflicts }) => {
            assert!(!conflicts.is_empty());
            assert!(
                crate::merge::has_conflict_markers(&bytes),
                "conflicts are marked"
            );
            let text = String::from_utf8(bytes).expect("a merge of UTF-8 is UTF-8");
            let mut kept = String::new();
            let mut section = None;
            for line in text.split_inclusive('\n') {
                let bare = line.trim_end_matches('\n');
                if bare.starts_with("<<<<<<<") {
                    section = Some(0);
                } else if bare.starts_with("|||||||") {
                    section = Some(1);
                } else if bare.starts_with("=======") {
                    section = Some(2);
                } else if bare.starts_with(">>>>>>>") {
                    section = None;
                } else if section.is_none_or(|at| at == 0) {
                    kept.push_str(bare);
                }
            }
            crate::json::with_depth_limit(64, || crate::json::parse(kept.as_bytes()))
                .expect("keeping ours leaves a JSON document");
        }
        Err(_) => {}
    }
}

/// A configuration file: accepted and valid, or refused -- never a panic.
pub fn config(data: &[u8]) {
    if let Ok(config) = crate::json::parse_as::<crate::config::Config>(data) {
        let _ = config.validate();
        let _ = config.ignore_set();
    }
}

/// A migration file: read, or refused -- never a panic.
pub fn migration(data: &[u8]) {
    let _ = crate::json::parse_as::<crate::migrate::Migration>(data);
}

/// History's binary formats -- a pack index, a pack entry, a tree node, a
/// revision, a snapshot ref -- read from damaged bytes: each is refused as
/// corrupt, never a panic, and what decodes encodes back to itself.
pub fn history_formats(data: &[u8]) {
    let _ = crate::store::pack::Index::parse(data);
    let id = crate::store::Id::of(data);
    let _ = crate::store::pack::decode_entry(data, id, data.len() as u32);
    if let Ok(node) = crate::store::node::Node::decode(data) {
        assert_eq!(
            crate::store::node::Node::decode(&node.encode())
                .ok()
                .map(|again| again.encode()),
            Some(node.encode()),
            "a node decodes from its own encoding"
        );
    }
    let _ = crate::store::revision::Revision::decode(data);
    let _ = crate::store::refs::SnapshotRef::decode(data);
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

        #[test]
        fn test7110_arbitrary_merges_never_break_the_driver(
            base in json_like(), ours in json_like(), theirs in json_like(), noise in prop::collection::vec(any::<u8>(), 0..40)
        ) {
            let mut data = serde_json::to_vec(&base).unwrap();
            data.push(0xFF);
            data.extend(serde_json::to_vec_pretty(&ours).unwrap());
            data.push(0xFF);
            data.extend(serde_json::to_vec(&theirs).unwrap());
            merge(&data);
            merge(&noise);
        }

        #[test]
        fn test7111_arbitrary_settings_and_migrations_are_read_or_refused(data in prop::collection::vec(any::<u8>(), 0..200), value in json_like()) {
            config(&data);
            migration(&data);
            config(value.to_string().as_bytes());
            migration(value.to_string().as_bytes());
        }

        #[test]
        fn test7112_damaged_history_bytes_are_refused_never_followed(data in prop::collection::vec(any::<u8>(), 0..300)) {
            history_formats(&data);
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
