//! Properties that must hold for every input, checked on generated ones.

use proptest::prelude::*;
use reldir::{
    canonical,
    catalog::Catalog,
    config::Config,
    fs::Disk,
    integrity,
    mirror::Mirror,
    schema::{RefPath, Schema, path::Step},
};
use serde_json::{Map, Value, json};
use std::{collections::BTreeSet, fs, path::Path, rc::Rc};

const DIALECT: &str = "https://reldir.dev/schema/reldir-2";

/// Arbitrary JSON, a few levels deep.
fn json_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(|n| json!(n)),
        "[a-zé\u{301} ]{0,6}".prop_map(Value::String),
    ];
    leaf.prop_recursive(3, 24, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map("[a-d]{1,3}", inner, 0..4)
                .prop_map(|members| Value::Object(members.into_iter().collect())),
        ]
    })
}

/// The same value with every object's members in reverse order.
fn reversed(value: &Value) -> Value {
    match value {
        Value::Object(members) => Value::Object(
            members
                .iter()
                .rev()
                .map(|(k, v)| (k.clone(), reversed(v)))
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(reversed).collect()),
        other => other.clone(),
    }
}

fn table(extra: Value) -> Value {
    let mut document = json!({
        "$schema": DIALECT, "type": "object",
        "properties": {"id": {"type": "string"}, "n": {"type": "integer", "x-reldir-type": "int"}},
        "required": ["id"], "additionalProperties": false,
        "x-reldir": {"table": "t", "primaryKey": ["id"]}
    });
    if let Value::Object(members) = extra {
        for (key, value) in members {
            document["properties"]["n"][key] = value;
        }
    }
    document
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, .. ProptestConfig::default() })]

    /// A row's hash is what it says, not how it is written: member order
    /// never matters, and canonicalizing is idempotent.
    #[test]
    fn test7001_row_identity_ignores_member_order(value in json_value()) {
        let row = json!({"id": "x", "v": value});
        prop_assert_eq!(canonical::row_hash(&row), canonical::row_hash(&reversed(&row)));
        let schema = Schema::from_document(json!({
            "$schema": DIALECT, "type": "object",
            "properties": {"id": {"type": "string"}, "v": {}},
            "required": ["id"], "additionalProperties": false,
            "x-reldir": {"table": "t", "primaryKey": ["id"]}
        }), None).unwrap();
        let once = canonical::canonical_row(row.as_object().unwrap(), &schema);
        let twice = canonical::canonical_row(once.as_object().unwrap(), &schema);
        prop_assert_eq!(once, twice);
    }

    /// A schema's identity ignores formatting, member order and annotations,
    /// and changes with any rule.
    #[test]
    fn test7002_schema_identity_is_its_rules(description in "[a-z ]{0,20}", bound in 1i64..100, spacing in 0usize..5) {
        let plain = Schema::from_document(table(json!({})), None).unwrap();
        let annotated = Schema::from_document(table(json!({"description": description, "$comment": "x"})), None).unwrap();
        prop_assert_eq!(plain.identity(), annotated.identity());
        let reformatted = Schema::from_bytes(&canonical::pretty_with_indent(&reversed(plain.document()), spacing + 1)).unwrap();
        prop_assert_eq!(plain.identity(), reformatted.identity());
        let bounded = Schema::from_document(table(json!({"minimum": bound})), None).unwrap();
        prop_assert_ne!(plain.identity(), bounded.identity());
    }

    /// Every key has one file name, no two keys share one, and the name decodes
    /// back to the key.
    #[test]
    fn test7003_file_names_are_injective_and_decode(a in "\\PC{1,12}", b in "\\PC{1,12}") {
        let schema = Schema::from_document(json!({
            "$schema": DIALECT, "type": "object", "properties": {"id": {"type": "string"}},
            "required": ["id"], "additionalProperties": false, "x-reldir": {"table": "t", "primaryKey": ["id"]}
        }), None).unwrap();
        let name = |key: &str| canonical::filename(&schema, json!({"id": key}).as_object().unwrap()).unwrap();
        let (first, second) = (name(&a), name(&b));
        prop_assert_eq!(a == b, first == second);
        prop_assert!(!first.starts_with('.') && !first.contains('/') && !first.contains('\\'));
        let stem = first.strip_suffix(".json").unwrap();
        prop_assert_eq!(canonical::percent_decode(stem).unwrap(), a);
    }

    /// A reference path prints in the grammar and parses back to itself.
    #[test]
    fn test7004_reference_paths_round_trip(
        names in prop::collection::vec("[a-z_][a-z0-9_]{0,5}|\"[^\"\\\\]{1,4}\"", 1..4),
        shapes in prop::collection::vec(0u8..3, 1..4),
        literal in "[a-z' ]{0,5}",
    ) {
        let mut steps = vec![];
        for (index, name) in names.iter().enumerate() {
            let name = name.trim_matches('"').to_string();
            steps.push(Step::Member(name));
            match shapes.get(index).copied().unwrap_or(0) {
                1 => steps.push(Step::Each),
                2 => steps.push(Step::Where { member: "kind".into(), equals: Value::String(literal.clone()) }),
                _ => {}
            }
        }
        let path = RefPath::from_steps(steps).unwrap();
        let text = path.to_string();
        prop_assert_eq!(RefPath::parse(&text).unwrap(), path, "{}", text);
    }
}

/// One edit to a small database made outside reldir.
#[derive(Debug, Clone)]
enum Edit {
    Write { id: u8, parent: Option<u8>, n: i64 },
    Garbage { id: u8 },
    Remove { id: u8 },
    Rename { id: u8, to: u8 },
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        4 => (0u8..6, proptest::option::of(0u8..6), -3i64..10).prop_map(|(id, parent, n)| Edit::Write { id, parent, n }),
        1 => (0u8..6).prop_map(|id| Edit::Garbage { id }),
        2 => (0u8..6).prop_map(|id| Edit::Remove { id }),
        1 => (0u8..6, 0u8..6).prop_map(|(id, to)| Edit::Rename { id, to }),
    ]
}

/// Apply an edit, giving what it writes a modification time of its own well
/// outside the racy window, as an edit made a while ago would have: the
/// incremental mirror then trusts every unchanged file's stat and re-reads
/// only what an edit touched, which is the path this property exercises.
fn apply(root: &Path, edit: &Edit, step: u64) {
    let path = |id: u8| root.join(format!("nodes/n{id}.json"));
    let stamp = std::time::SystemTime::now() - std::time::Duration::from_secs(10_000 - step);
    let settle = |target: std::path::PathBuf| {
        if let Ok(file) = fs::File::options().write(true).open(&target) {
            file.set_modified(stamp).unwrap();
        }
    };
    match edit {
        Edit::Write { id, parent, n } => {
            let mut row = Map::new();
            row.insert("id".into(), json!(format!("n{id}")));
            if let Some(parent) = parent {
                row.insert("parent".into(), json!(format!("n{parent}")));
            }
            row.insert("n".into(), json!(n));
            fs::write(path(*id), serde_json::to_vec(&row).unwrap()).unwrap();
            settle(path(*id));
        }
        Edit::Garbage { id } => {
            fs::write(path(*id), b"{\"id\": ").unwrap();
            settle(path(*id));
        }
        Edit::Remove { id } => {
            let _ = fs::remove_file(path(*id));
        }
        Edit::Rename { id, to } => {
            let _ = fs::rename(path(*id), path(*to));
        }
    }
}

/// The verdict on a directory, and how many files were read to reach it.
fn verdict(root: &Path, mirror: Rc<Mirror>) -> (BTreeSet<String>, usize) {
    let catalog = Catalog::observe(root, &Config::default(), &Disk, mirror, false).unwrap();
    let verdict = integrity::validate(&catalog).unwrap();
    let found = verdict
        .errors
        .iter()
        .chain(&verdict.warnings)
        .map(|d| format!("{} {:?} {:?} {}", d.code, d.path, d.pointer, d.message))
        .collect();
    (found, catalog.reread.len())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, .. ProptestConfig::default() })]

    /// Judging a directory incrementally -- re-reading only what changed since
    /// the last observation -- always reaches the verdict a judgment from
    /// scratch reaches.
    #[test]
    fn test7005_incremental_validation_equals_full_validation(edits in prop::collection::vec(edit(), 1..14)) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("nodes")).unwrap();
        fs::create_dir_all(root.join(".db/schema")).unwrap();
        fs::write(root.join(".db/schema/nodes.json"), serde_json::to_vec(&json!({
            "$schema": DIALECT, "type": "object",
            "properties": {"id": {"type": "string"}, "parent": {"type": "string"}, "n": {"type": "integer", "minimum": 0}},
            "required": ["id", "n"], "additionalProperties": false,
            "x-reldir": {"table": "nodes", "primaryKey": ["id"], "unique": [["n"]],
                         "foreignKeys": [{"from": ["parent"], "to": {"table": "nodes"}}],
                         "acyclic": [{"name": "tree", "edges": ["parent"]}]}
        })).unwrap()).unwrap();
        let persistent = Rc::new(Mirror::open_memory().unwrap());
        for (step, edit) in (1u64..).zip(&edits) {
            apply(root, edit, step);
            let (incremental, reread) = verdict(root, Rc::clone(&persistent));
            let (full, _) = verdict(root, Rc::new(Mirror::open_memory().unwrap()));
            prop_assert_eq!(incremental, full, "after {:?}", edit);
            prop_assert!(step == 1 || reread <= 2, "only what the edit touched is read again, not {} files", reread);
        }
    }
}

#[test]
fn test7006_the_generators_reach_the_interesting_cases() {
    // A property is only as good as the inputs it sees: check the reversal
    // actually reorders something, so test7001 is not vacuous.
    let value = json!({"a": 1, "b": {"c": 2, "d": 3}});
    assert_ne!(
        serde_json::to_string(&value).unwrap(),
        serde_json::to_string(&reversed(&value)).unwrap()
    );
}

/// One node of a generated forest: its parent, and the nodes it links to.
type Node = (Option<usize>, Vec<usize>);

/// A random forest: each node may have a parent among the nodes before it,
/// and links to any nodes. Paired with the node to delete.
fn forest() -> impl Strategy<Value = (Vec<Node>, usize)> {
    (2usize..9).prop_flat_map(|count| {
        let nodes = (0..count)
            .map(|index| {
                let parent = if index == 0 {
                    Just(None).boxed()
                } else {
                    proptest::option::of(0..index).boxed()
                };
                (parent, prop::collection::vec(0..count, 0..3))
            })
            .collect::<Vec<_>>();
        (nodes, 0..count)
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]

    /// Deleting a row carries out exactly what the foreign keys declare -- a
    /// cascade removes every descendant, `remove` takes every link to a
    /// removed row out of its array -- and nothing else changes.
    #[test]
    fn test7007_referential_actions_match_an_oracle((nodes, victim) in forest()) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("schema")).unwrap();
        fs::write(root.join("schema/nodes.json"), serde_json::to_vec(&json!({
            "$schema": DIALECT, "type": "object",
            "properties": {"id": {"type": "string"}, "parent": {"type": ["string", "null"]},
                           "links": {"type": "array", "items": {"type": "string"}}},
            "required": ["id", "links"], "additionalProperties": false,
            "x-reldir": {"table": "nodes", "primaryKey": ["id"], "foreignKeys": [
                {"from": ["parent"], "to": {"table": "nodes"}, "onDelete": "cascade"},
                {"from": ["links[]"], "to": {"table": "nodes"}, "onDelete": "remove"}
            ]}
        })).unwrap()).unwrap();
        fs::create_dir_all(root.join("nodes")).unwrap();
        for (index, (parent, links)) in nodes.iter().enumerate() {
            let row = json!({
                "id": format!("n{index}"),
                "parent": parent.map(|p| format!("n{p}")),
                "links": links.iter().map(|l| format!("n{l}")).collect::<Vec<_>>(),
            });
            fs::write(root.join(format!("nodes/n{index}.json")), serde_json::to_vec(&row).unwrap()).unwrap();
        }

        // The oracle: the victim and every descendant go; links to them go.
        let mut gone = BTreeSet::from([victim]);
        loop {
            let before = gone.len();
            for (index, (parent, _)) in nodes.iter().enumerate() {
                if parent.is_some_and(|p| gone.contains(&p)) {
                    gone.insert(index);
                }
            }
            if gone.len() == before { break; }
        }

        let opening = reldir::state::Opening {
            access: reldir::db::Access::Write,
            establish: true,
            rebuild_metadata: false,
            dry_run: false,
        };
        let reldir::state::Opened::Database { database, .. } = reldir::state::open(root, opening, &Default::default()).unwrap() else {
            panic!("a folder with rows is a database")
        };
        let mut database = *database;
        let row = database.catalog.row_at(Path::new(&format!("nodes/n{victim}.json"))).unwrap().unwrap();
        database.apply(vec![reldir::plan::RowChange::delete(row)], reldir::db::Request::internal(false)).unwrap();

        for (index, (parent, links)) in nodes.iter().enumerate() {
            let path = root.join(format!("nodes/n{index}.json"));
            prop_assert_eq!(path.exists(), !gone.contains(&index), "n{}", index);
            if gone.contains(&index) { continue; }
            let row: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let expected: Vec<String> = links.iter().filter(|l| !gone.contains(l)).map(|l| format!("n{l}")).collect();
            prop_assert_eq!(&row["links"], &json!(expected), "n{} links", index);
            prop_assert_eq!(&row["parent"], &json!(parent.map(|p| format!("n{p}"))), "n{} parent is untouched", index);
        }
        prop_assert!(database.is_valid());
    }
}
