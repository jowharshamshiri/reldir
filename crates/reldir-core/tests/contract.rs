//! The observed contract, under random use.
//!
//! A folder of JSON files that other programs read has an implicit schema:
//! the shapes its files hold. Adopting it changes no file; and after any
//! sequence of changes made through reldir -- SQL, row operations, imports,
//! with their referential actions -- every file still fits that schema, every
//! byte a change did not change is as it was, and a change reldir refuses
//! leaves the folder exactly as it found it.
//!
//! The oracles share no code with reldir: the files are written here, in
//! styles and spellings chosen here; the shape they establish is read here;
//! and the schema reldir adopted is checked by an independent validator.

use proptest::prelude::*;
use reldir_core::{Connection, Options};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

/// What a member of the latent shape holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Integers, written without a fraction.
    Int,
    /// Numbers always written with a fraction.
    Float,
    /// Integers beyond 64 bits.
    Big,
    /// Strings, some in decomposed Unicode.
    Text,
    /// Timestamps written with offsets.
    Instant,
    Bool,
    /// An object with an integer and, sometimes, a string.
    Nested,
    /// An array of strings.
    Tags,
    /// Always null.
    Nothing,
}

#[derive(Debug, Clone)]
struct Member {
    name: String,
    kind: Kind,
    /// Some rows leave it out.
    optional: bool,
    /// Some rows hold null in it.
    nullable: bool,
}

/// How a file is written.
#[derive(Debug, Clone, Copy)]
struct Style {
    indent: Option<&'static str>,
    crlf: bool,
    bom: bool,
    final_newline: bool,
}

#[derive(Debug, Clone)]
struct Folder {
    /// The key is the file name, not a member.
    keyed_by_name: bool,
    members: Vec<Member>,
    rows: Vec<(Map<String, Value>, Style)>,
}

fn number(lexeme: &str) -> Value {
    serde_json::from_str(lexeme).expect("a number lexeme")
}

fn value_of(kind: Kind, seed: u64) -> Value {
    match kind {
        Kind::Int => json!((seed % 50) as i64 - 10),
        Kind::Float => number(&format!("{}.{}", seed % 20, seed % 7)),
        Kind::Big => number(&format!("1844674407370955161{}", seed % 10)),
        Kind::Text => {
            let words = ["café", "cafe\u{301}", "naïve", "plain", "日本"];
            Value::String(words[(seed % words.len() as u64) as usize].to_string())
        }
        Kind::Instant => Value::String(format!(
            "2024-0{}-1{}T1{}:00:00{}",
            1 + seed % 9,
            seed % 10,
            seed % 10,
            ["+02:00", "Z", "-05:30"][(seed % 3) as usize]
        )),
        Kind::Bool => Value::Bool(seed.is_multiple_of(2)),
        Kind::Nested => {
            let mut nested = Map::new();
            nested.insert("n".into(), json!(seed % 9));
            if !seed.is_multiple_of(3) {
                nested.insert("label".into(), json!(format!("l{}", seed % 4)));
            }
            Value::Object(nested)
        }
        Kind::Tags => json!((0..seed % 3).map(|i| format!("t{i}")).collect::<Vec<_>>()),
        Kind::Nothing => Value::Null,
    }
}

fn member() -> impl Strategy<Value = (Kind, bool, bool)> {
    (
        prop_oneof![
            Just(Kind::Int),
            Just(Kind::Float),
            Just(Kind::Big),
            Just(Kind::Text),
            Just(Kind::Instant),
            Just(Kind::Bool),
            Just(Kind::Nested),
            Just(Kind::Tags),
            Just(Kind::Nothing),
        ],
        any::<bool>(),
        any::<bool>(),
    )
}

fn style() -> impl Strategy<Value = Style> {
    (
        prop_oneof![
            Just(None),
            Just(Some("  ")),
            Just(Some("    ")),
            Just(Some("\t"))
        ],
        any::<bool>(),
        prop::bool::weighted(0.15),
        prop::bool::weighted(0.8),
    )
        .prop_map(|(indent, crlf, bom, final_newline)| Style {
            indent,
            crlf,
            bom,
            final_newline,
        })
}

fn folder() -> impl Strategy<Value = Folder> {
    (
        any::<bool>(),
        prop::collection::vec(member(), 1..6),
        prop::collection::vec((any::<u64>(), style(), any::<u64>()), 3..9),
    )
        .prop_map(|(keyed_by_name, kinds, rows)| {
            let members: Vec<Member> = kinds
                .into_iter()
                .enumerate()
                .map(|(index, (kind, optional, nullable))| Member {
                    name: format!("m{index}"),
                    kind,
                    optional,
                    nullable: nullable && kind != Kind::Nothing,
                })
                .collect();
            let rows = rows
                .into_iter()
                .enumerate()
                .map(|(index, (seed, style, order))| {
                    let mut row = Map::new();
                    if !keyed_by_name {
                        row.insert("id".into(), json!(format!("r{index}")));
                    }
                    for (offset, member) in members.iter().enumerate() {
                        let seed = seed.wrapping_add(offset as u64 * 7919);
                        // The first row has every member, so each is seen.
                        if member.optional && index > 0 && seed % 3 == 0 {
                            continue;
                        }
                        let value = if member.nullable && index > 0 && seed % 4 == 1 {
                            Value::Null
                        } else {
                            value_of(member.kind, seed)
                        };
                        row.insert(member.name.clone(), value);
                    }
                    // Members in an order of the file's own.
                    let mut entries: Vec<(String, Value)> = row.into_iter().collect();
                    let rotate = (order % entries.len().max(1) as u64) as usize;
                    entries.rotate_left(rotate);
                    (entries.into_iter().collect(), style)
                })
                .collect();
            Folder {
                keyed_by_name,
                members,
                rows,
            }
        })
}

/// A value written in a style, here rather than by reldir.
fn render(value: &Value, style: &Style) -> Vec<u8> {
    fn write(value: &Value, indent: Option<&str>, depth: usize, out: &mut String) {
        let pad = |level: usize| indent.map(|unit| unit.repeat(level)).unwrap_or_default();
        let (open_gap, colon) = match indent {
            Some(_) => ("\n", ": "),
            None => ("", ":"),
        };
        match value {
            Value::Object(members) if !members.is_empty() => {
                out.push('{');
                for (index, (name, member)) in members.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(open_gap);
                    out.push_str(&pad(depth + 1));
                    out.push_str(&serde_json::to_string(name).unwrap());
                    out.push_str(colon);
                    write(member, indent, depth + 1, out);
                }
                out.push_str(open_gap);
                out.push_str(&pad(depth));
                out.push('}');
            }
            Value::Array(items) if !items.is_empty() => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(open_gap);
                    out.push_str(&pad(depth + 1));
                    write(item, indent, depth + 1, out);
                }
                out.push_str(open_gap);
                out.push_str(&pad(depth));
                out.push(']');
            }
            other => out.push_str(&serde_json::to_string(other).unwrap()),
        }
    }
    let mut text = String::new();
    write(value, style.indent, 0, &mut text);
    if style.final_newline {
        text.push('\n');
    }
    if style.crlf {
        text = text.replace('\n', "\r\n");
    }
    let mut bytes = if style.bom {
        b"\xEF\xBB\xBF".to_vec()
    } else {
        vec![]
    };
    bytes.extend(text.into_bytes());
    bytes
}

/// The JSON a file holds, BOM aside.
fn parse(bytes: &[u8]) -> Value {
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    serde_json::from_slice(body)
        .unwrap_or_else(|error| panic!("not JSON ({error}): {}", String::from_utf8_lossy(bytes)))
}

/// What kind of JSON a value is, telling an integer's spelling from a
/// float's.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(number) => {
            if number.to_string().contains(['.', 'e', 'E']) {
                "float"
            } else {
                "int"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The shape a set of documents holds: for each path, the kinds of value
/// seen there and how often it was present where its parent was.
#[derive(Debug, Default)]
struct Shape {
    kinds: BTreeMap<String, BTreeSet<&'static str>>,
    present: BTreeMap<String, usize>,
    parents: BTreeMap<String, usize>,
}

impl Shape {
    fn observe(&mut self, path: &str, value: &Value) {
        self.kinds
            .entry(path.to_string())
            .or_default()
            .insert(kind_of(value));
        match value {
            Value::Object(members) => {
                for (name, member) in members {
                    let at = format!("{path}/{name}");
                    *self.present.entry(at.clone()).or_default() += 1;
                    self.observe(&at, member);
                }
                *self.parents.entry(path.to_string()).or_default() += 1;
            }
            Value::Array(items) => {
                for item in items {
                    self.observe(&format!("{path}/[]"), item);
                }
            }
            _ => {}
        }
    }

    /// Whether a document fits: every value of a kind seen at its path, every
    /// member one seen, every member always present still present.
    fn admits(&self, path: &str, value: &Value) -> Result<(), String> {
        let kinds = self
            .kinds
            .get(path)
            .ok_or_else(|| format!("{path} was never seen"))?;
        if !kinds.contains(kind_of(value)) {
            return Err(format!(
                "{path} holds {}, and only {kinds:?} were seen there",
                kind_of(value)
            ));
        }
        match value {
            Value::Object(members) => {
                for (name, member) in members {
                    self.admits(&format!("{path}/{name}"), member)?;
                }
                let parents = self.parents.get(path).copied().unwrap_or(0);
                for (at, count) in &self.present {
                    let Some(name) = at.strip_prefix(&format!("{path}/")) else {
                        continue;
                    };
                    if !name.contains('/') && *count == parents && !members.contains_key(name) {
                        return Err(format!("{at} was always present, and is gone"));
                    }
                }
                Ok(())
            }
            Value::Array(items) => items
                .iter()
                .try_for_each(|item| self.admits(&format!("{path}/[]"), item)),
            _ => Ok(()),
        }
    }
}

/// Every file of the folder's table, by name, with its bytes.
fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(root.join("items")).unwrap() {
        let path = entry.unwrap().path();
        out.insert(
            path.file_name().unwrap().to_string_lossy().into_owned(),
            fs::read(&path).unwrap(),
        );
    }
    out
}

/// How a file is written, read back: what a change must keep.
fn written_style(bytes: &[u8]) -> (bool, bool, bool, Option<String>) {
    let bom = bytes.starts_with(b"\xEF\xBB\xBF");
    let text = String::from_utf8_lossy(bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes));
    let crlf = text.contains("\r\n");
    let final_newline = text.ends_with('\n');
    let indent = text.lines().nth(1).map(|line| {
        line.chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect::<String>()
    });
    (bom, crlf, final_newline, indent)
}

#[derive(Debug, Clone)]
enum Operation {
    /// Set a member of a row to another value of its kind.
    Set {
        row: usize,
        member: usize,
        seed: u64,
    },
    /// Set a member of a row to null.
    Null {
        row: usize,
        member: usize,
    },
    /// Add one to an integer member, in SQL.
    Increment {
        member: usize,
    },
    /// Insert a row with every member.
    Insert {
        seed: u64,
    },
    Delete {
        row: usize,
    },
    /// A row command: patch a member.
    Patch {
        row: usize,
        member: usize,
        seed: u64,
    },
}

fn operation() -> impl Strategy<Value = Operation> {
    prop_oneof![
        3 => (0usize..16, 0usize..8, any::<u64>()).prop_map(|(row, member, seed)| Operation::Set { row, member, seed }),
        1 => (0usize..16, 0usize..8).prop_map(|(row, member)| Operation::Null { row, member }),
        1 => (0usize..8).prop_map(|member| Operation::Increment { member }),
        2 => any::<u64>().prop_map(|seed| Operation::Insert { seed }),
        1 => (0usize..16).prop_map(|row| Operation::Delete { row }),
        2 => (0usize..16, 0usize..8, any::<u64>()).prop_map(|(row, member, seed)| Operation::Patch { row, member, seed }),
    ]
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

/// Run one operation; the member it targets, when it targets one.
fn perform(
    db: &Connection,
    folder: &Folder,
    keys: &[String],
    operation: &Operation,
    serial: usize,
) -> (Result<(), reldir_core::Error>, Option<String>) {
    // A table keyed by its file names exposes the name as `id` too.
    let key_column = "id";
    let pick = |row: usize| keys.get(row % keys.len().max(1)).cloned();
    let member = |index: usize| &folder.members[index % folder.members.len()];
    match operation {
        Operation::Set {
            row,
            member: index,
            seed,
        } => {
            let (Some(key), member) = (pick(*row), member(*index)) else {
                return (Ok(()), None);
            };
            let value = value_of(member.kind, *seed);
            let sql = format!(
                "UPDATE items SET {} = ? WHERE {key_column} = ?",
                quote(&member.name)
            );
            (
                db.execute(&sql, &[value, json!(key)]).map(|_| ()),
                Some(member.name.clone()),
            )
        }
        Operation::Null { row, member: index } => {
            let (Some(key), member) = (pick(*row), member(*index)) else {
                return (Ok(()), None);
            };
            let sql = format!(
                "UPDATE items SET {} = NULL WHERE {key_column} = ?",
                quote(&member.name)
            );
            (
                db.execute(&sql, &[json!(key)]).map(|_| ()),
                Some(member.name.clone()),
            )
        }
        Operation::Increment { member: index } => {
            let member = member(*index);
            if member.kind != Kind::Int {
                return (Ok(()), None);
            }
            let sql = format!("UPDATE items SET {0} = {0} + 1", quote(&member.name));
            (db.execute(&sql, &[]).map(|_| ()), Some(member.name.clone()))
        }
        Operation::Insert { seed } => {
            let mut row = Map::new();
            row.insert("id".into(), json!(format!("new{serial}")));
            for (offset, member) in folder.members.iter().enumerate() {
                row.insert(
                    member.name.clone(),
                    value_of(member.kind, seed.wrapping_add(offset as u64)),
                );
            }
            (db.insert("items", vec![row]).map(|_| ()), None)
        }
        Operation::Delete { row } => {
            let Some(key) = pick(*row) else {
                return (Ok(()), None);
            };
            (db.delete("items", &[json!(key)]).map(|_| ()), None)
        }
        Operation::Patch {
            row,
            member: index,
            seed,
        } => {
            let (Some(key), member) = (pick(*row), member(*index)) else {
                return (Ok(()), None);
            };
            let mut patch = Map::new();
            patch.insert(member.name.clone(), value_of(member.kind, *seed));
            (
                db.update("items", &[json!(key)], &patch).map(|_| ()),
                Some(member.name.clone()),
            )
        }
    }
}

/// The adopted contract, for an independent validator: reldir's dialect
/// marker replaced by the standard's, its extensions left to be ignored.
fn validator(root: &Path) -> (boon::Schemas, boon::SchemaIndex) {
    let mut document: Value =
        serde_json::from_slice(&fs::read(root.join(".db/schema/items.json")).unwrap()).unwrap();
    document["$schema"] = json!("https://json-schema.org/draft/2020-12/schema");
    let mut schemas = boon::Schemas::new();
    let mut compiler = boon::Compiler::new();
    compiler.add_resource("urn:reldir:items", document).unwrap();
    let index = compiler.compile("urn:reldir:items", &mut schemas).unwrap();
    (schemas, index)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    #[test]
    fn test7201_changes_keep_every_file_within_the_observed_contract(
        folder in folder(),
        operations in prop::collection::vec(operation(), 1..10),
    ) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir_all(root.join("items")).unwrap();
        let mut original = Shape::default();
        let mut keys = vec![];
        for (index, (row, style)) in folder.rows.iter().enumerate() {
            let key = format!("r{index}");
            let mut body = row.clone();
            if folder.keyed_by_name {
                body.remove("id");
            }
            fs::write(root.join("items").join(format!("{key}.json")), render(&Value::Object(body.clone()), style)).unwrap();
            original.observe("", &Value::Object(body));
            keys.push(key);
        }
        let before = files(root);

        // Adopting the folder writes no file of it.
        let db = Connection::open(root, Options::default()).unwrap();
        prop_assert_eq!(files(root), before.clone(), "adoption changed a file");
        let (schemas, index) = validator(root);
        for (name, bytes) in &before {
            prop_assert!(schemas.validate(&parse(bytes), index).is_ok(), "{} does not fit its own contract", name);
        }

        for (serial, operation) in operations.iter().enumerate() {
            let before = files(root);
            let (outcome, targeted) = perform(&db, &folder, &keys, operation, serial);
            let after = files(root);
            match outcome {
                Err(_) => prop_assert_eq!(&after, &before, "a refused {:?} changed the folder", operation),
                Ok(()) => {
                    for (name, bytes) in &after {
                        let value = parse(bytes);
                        let mut body = value.clone();
                        if folder.keyed_by_name {
                            prop_assert!(body.get("id").is_none(), "{} gained the key as a member", name);
                            body.as_object_mut().unwrap().remove("id");
                        }
                        if let Err(why) = original.admits("", &body) {
                            prop_assert!(false, "after {:?}, {} leaves the observed shape: {}", operation, name, why);
                        }
                        prop_assert!(
                            schemas.validate(&value, index).is_ok(),
                            "after {:?}, {} does not fit the adopted contract", operation, name
                        );
                        let Some(previous) = before.get(name) else { continue };
                        if previous == bytes {
                            continue;
                        }
                        // A file the change rewrote keeps its style, and every
                        // member the change did not target keeps its exact
                        // spelling.
                        let (was, now) = (written_style(previous), written_style(bytes));
                        prop_assert_eq!((was.0, was.1, was.2), (now.0, now.1, now.2), "{} changed its mark or line endings", name);
                        // An object left with no members has no line to show
                        // its indentation on.
                        if let (Some(was), Some(now)) = (&was.3, &now.3) {
                            prop_assert_eq!(was, now, "{} changed its indentation", name);
                        }
                        let (old, new) = (parse(previous), value);
                        for (member, old_value) in old.as_object().unwrap() {
                            if Some(member) == targeted.as_ref() {
                                continue;
                            }
                            prop_assert_eq!(Some(old_value), new.get(member), "{}.{} changed though the change was elsewhere", name, member);
                        }
                    }
                }
            }
            keys = after.keys().map(|name| name.trim_end_matches(".json").to_string()).collect();
        }
    }
}

#[test]
fn test7202_the_operations_the_property_runs_are_made_not_merely_refused() {
    let style = Style {
        indent: Some("    "),
        crlf: true,
        bom: true,
        final_newline: false,
    };
    let members = vec![
        Member {
            name: "m0".into(),
            kind: Kind::Int,
            optional: false,
            nullable: false,
        },
        Member {
            name: "m1".into(),
            kind: Kind::Big,
            optional: false,
            nullable: false,
        },
        Member {
            name: "m2".into(),
            kind: Kind::Instant,
            optional: false,
            nullable: false,
        },
        Member {
            name: "m3".into(),
            kind: Kind::Text,
            optional: false,
            nullable: true,
        },
    ];
    let rows = (0..3u64)
        .map(|seed| {
            let mut row = Map::new();
            row.insert("id".into(), json!(format!("r{seed}")));
            for member in &members {
                row.insert(member.name.clone(), value_of(member.kind, seed + 1));
            }
            // One row holds null where null is admitted, so it is observed.
            if seed == 2 {
                row.insert("m3".into(), Value::Null);
            }
            (row, style)
        })
        .collect();
    let folder = Folder {
        keyed_by_name: false,
        members,
        rows,
    };
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    fs::create_dir_all(root.join("items")).unwrap();
    for (row, style) in &folder.rows {
        let name = row["id"].as_str().unwrap();
        fs::write(
            root.join(format!("items/{name}.json")),
            render(&Value::Object(row.clone()), style),
        )
        .unwrap();
    }
    let db = Connection::open(root, Options::default()).unwrap();
    let keys: Vec<String> = (0..3).map(|n| format!("r{n}")).collect();
    let before = fs::read(root.join("items/r0.json")).unwrap();
    for operation in [
        Operation::Set {
            row: 0,
            member: 0,
            seed: 3,
        },
        Operation::Increment { member: 0 },
        Operation::Patch {
            row: 0,
            member: 3,
            seed: 1,
        },
        Operation::Null { row: 1, member: 3 },
        Operation::Insert { seed: 9 },
        Operation::Delete { row: 1 },
    ] {
        let (outcome, _) = perform(&db, &folder, &keys, &operation, 7);
        assert!(outcome.is_ok(), "{operation:?}: {outcome:?}");
    }
    let after = fs::read(root.join("items/r0.json")).unwrap();
    assert_eq!(written_style(&before), written_style(&after));
    let (old, new) = (parse(&before), parse(&after));
    assert_eq!(old["m1"], new["m1"], "the big integer keeps its digits");
    assert_eq!(old["m2"], new["m2"], "the timestamp keeps its offset");
}
