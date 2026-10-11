//! Three-way merge of JSON documents, member by member: what git's merge
//! driver for row files does.
//!
//! A member one side changed and the other left as the common ancestor had
//! it takes the change; a member both sides changed alike takes it once;
//! objects both sides changed are merged member by member. Anything else --
//! a scalar or an array both sides changed differently, a member one side
//! removed and the other changed -- is a conflict.
//!
//! A clean merge is written as edits to "ours": every byte the merge leaves
//! alone keeps its spelling and the file keeps its style. A conflicted merge
//! is "ours" with each conflicting member replaced by git's diff3 markers,
//! scoped to that member alone; each side's section is written so that
//! keeping any one of them, and deleting the markers, leaves valid JSON.

use crate::json_edit::{Children, Document, EditError, edit_document};
use crate::schema::path::escape_pointer;
use serde_json::{Map, Value};

/// A member, element or whole document both sides changed differently.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    /// Where, as an RFC 6901 pointer.
    pub pointer: String,
    pub base: Option<Value>,
    pub ours: Option<Value>,
    pub theirs: Option<Value>,
}

/// The labels of the three sides, as git passes them.
#[derive(Debug, Clone)]
pub struct Labels {
    pub base: String,
    pub ours: String,
    pub theirs: String,
    /// The length of the marker runs: git's `conflict-marker-size`.
    pub marker_size: usize,
}

impl Default for Labels {
    fn default() -> Self {
        Self {
            base: "base".into(),
            ours: "ours".into(),
            theirs: "theirs".into(),
            marker_size: 7,
        }
    }
}

/// The merged file.
#[derive(Debug, Clone, PartialEq)]
pub enum Merged {
    /// Merged without conflict.
    Clean(Vec<u8>),
    /// "Ours", with markers around every conflict.
    Conflicted {
        bytes: Vec<u8>,
        conflicts: Vec<Conflict>,
    },
}

/// Why the documents could not be merged member by member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeError {
    /// One side is not a JSON document.
    Invalid { side: &'static str, reason: String },
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MergeError::Invalid { side, reason } => {
                write!(f, "{side} is not a JSON document: {reason}")
            }
        }
    }
}

fn read<'a>(raw: &'a [u8], side: &'static str) -> Result<Document<'a>, MergeError> {
    Document::parse(raw).map_err(|error| MergeError::Invalid {
        side,
        reason: error.to_string(),
    })
}

/// Merge three values at `pointer`: the merged value -- `None` for absent --
/// with conflicts resolved as "ours", and every conflict found.
fn merge_value(
    pointer: &str,
    base: Option<&Value>,
    ours: Option<&Value>,
    theirs: Option<&Value>,
    conflicts: &mut Vec<Conflict>,
) -> Option<Value> {
    if ours == theirs {
        return ours.cloned();
    }
    if base == ours {
        return theirs.cloned();
    }
    if base == theirs {
        return ours.cloned();
    }
    // Objects on both sides merge member by member; an ancestor that was
    // not an object contributed no members.
    if let (Some(Value::Object(ours)), Some(Value::Object(theirs))) = (ours, theirs) {
        let empty = Map::new();
        let base = match base {
            Some(Value::Object(base)) => base,
            _ => &empty,
        };
        return Some(Value::Object(merge_members(
            pointer, base, ours, theirs, conflicts,
        )));
    }
    conflicts.push(Conflict {
        pointer: pointer.to_string(),
        base: base.cloned(),
        ours: ours.cloned(),
        theirs: theirs.cloned(),
    });
    ours.cloned()
}

/// Merge two objects that came from one, member by member. Members keep
/// "ours" order; members only "theirs" added follow, in their order.
fn merge_members(
    pointer: &str,
    base: &Map<String, Value>,
    ours: &Map<String, Value>,
    theirs: &Map<String, Value>,
    conflicts: &mut Vec<Conflict>,
) -> Map<String, Value> {
    let mut names: Vec<&String> = ours.keys().collect();
    names.extend(theirs.keys().filter(|name| !ours.contains_key(*name)));
    names.extend(
        base.keys()
            .filter(|name| !ours.contains_key(*name) && !theirs.contains_key(*name)),
    );
    let mut merged = Map::new();
    for name in names {
        let at = format!("{pointer}/{}", escape_pointer(name));
        if let Some(value) = merge_value(
            &at,
            base.get(name),
            ours.get(name),
            theirs.get(name),
            conflicts,
        ) {
            merged.insert(name.clone(), value);
        }
    }
    merged
}

/// Merge the values of three documents.
pub fn merge_values(base: Option<&Value>, ours: &Value, theirs: &Value) -> (Value, Vec<Conflict>) {
    let mut conflicts = vec![];
    let merged = merge_value("", base, Some(ours), Some(theirs), &mut conflicts)
        .expect("both sides hold a document, so the merge holds one");
    (merged, conflicts)
}

/// Merge three row files: `base` empty when both sides added the file.
pub fn merge_documents(
    base: &[u8],
    ours: &[u8],
    theirs: &[u8],
    labels: &Labels,
) -> Result<Merged, MergeError> {
    let base_value = if base.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        Some(read(base, "the common ancestor")?.value().clone())
    };
    let ours_document = read(ours, "ours")?;
    let theirs_value = read(theirs, "theirs")?.value().clone();
    let (merged, conflicts) =
        merge_values(base_value.as_ref(), ours_document.value(), &theirs_value);
    let bytes = edit_document(ours, &merged).map_err(|error| MergeError::Invalid {
        side: "the merge",
        reason: error.to_string(),
    })?;
    if conflicts.is_empty() {
        return Ok(Merged::Clean(bytes));
    }
    // The merge as it would be had every conflict gone "their" way.
    let mut theirs_resolved = merged.clone();
    for conflict in &conflicts {
        set(
            &mut theirs_resolved,
            &conflict.pointer,
            conflict.theirs.clone(),
        );
    }
    let marked = mark(
        &bytes,
        &conflicts,
        &theirs_resolved,
        base_value.as_ref(),
        labels,
    )
    .map_err(|error| MergeError::Invalid {
        side: "the merge",
        reason: error.to_string(),
    })?;
    Ok(Merged::Conflicted {
        bytes: marked,
        conflicts,
    })
}

fn compact(value: &Value) -> String {
    crate::canonical::compact(value)
}

/// A conflict block: each side's text on its own lines between git's markers.
fn block(sides: [&str; 3], labels: &Labels) -> String {
    let run = |c: char| c.to_string().repeat(labels.marker_size);
    let [ours, base, theirs] = sides;
    let section = |text: &str| {
        if text.is_empty() {
            String::new()
        } else {
            format!("{text}\n")
        }
    };
    format!(
        "\n{} {}\n{}{} {}\n{}{}\n{}{} {}\n",
        run('<'),
        labels.ours,
        section(ours),
        run('|'),
        labels.base,
        section(base),
        run('='),
        section(theirs),
        run('>'),
        labels.theirs,
    )
}

/// The merged bytes -- "ours" wherever the sides conflict -- with markers
/// around each conflict.
///
/// Within an object, a conflicted member's sections carry one comma beside
/// it, so that keeping any side of any conflict leaves valid JSON: members
/// before the object's first unconflicted member -- present whatever is
/// chosen -- carry the comma after them, members after it the comma before.
/// An object whose every member conflicts is marked whole.
fn mark(
    bytes: &[u8],
    conflicts: &[Conflict],
    theirs_resolved: &Value,
    base: Option<&Value>,
    labels: &Labels,
) -> Result<Vec<u8>, EditError> {
    let document = Document::parse(bytes)?;
    let texts = |sides: [Option<&Value>; 3], lead: &str, trail: &str, name: Option<&str>| {
        sides.map(|side| {
            side.map(|value| match name {
                Some(name) => format!(
                    "{lead}{}: {}{trail}",
                    compact(&Value::String(name.to_string())),
                    compact(value)
                ),
                None => format!("{lead}{}{trail}", compact(value)),
            })
            .unwrap_or_default()
        })
    };
    // Conflicts grouped by the object holding them. An object whose every
    // member conflicts has no member to carry the commas, so it becomes one
    // conflict of the member holding it, a level up -- where a side without
    // it simply leaves the member out -- until every group has an anchor.
    let mut pending: Vec<Conflict> = conflicts.to_vec();
    let parents: Vec<(String, Vec<Conflict>)> = loop {
        let mut parents: Vec<(String, Vec<Conflict>)> = vec![];
        for conflict in &pending {
            let (parent, _) = split(&conflict.pointer);
            match parents.iter_mut().find(|(pointer, _)| *pointer == parent) {
                Some((_, group)) => group.push(conflict.clone()),
                None => parents.push((parent, vec![conflict.clone()])),
            }
        }
        let unanchored = parents.iter().find(|(parent, group)| {
            !parent.is_empty()
                && !group.iter().any(|conflict| conflict.pointer.is_empty())
                && matches!(
                    document.lookup(parent).map(|id| &document.node(id).children),
                    Ok(Children::Members(members))
                        if members.iter().all(|member| group.iter().any(|conflict| split(&conflict.pointer).1 == member.name))
                )
        });
        let Some((parent, _)) = unanchored else {
            break parents;
        };
        let parent = parent.clone();
        pending.retain(|conflict| split(&conflict.pointer).0 != parent);
        pending.push(Conflict {
            pointer: parent.clone(),
            base: base.and_then(|base| base.pointer(&parent)).cloned(),
            ours: document.value().pointer(&parent).cloned(),
            theirs: theirs_resolved.pointer(&parent).cloned(),
        });
    };
    // Each replacement: the byte range it covers, and the text that goes there.
    let mut replacements: Vec<(std::ops::Range<usize>, String)> = vec![];
    let whole = |pointer: &str,
                 replacements: &mut Vec<(std::ops::Range<usize>, String)>|
     -> Result<(), EditError> {
        let node = document.lookup(pointer)?;
        let span = document.node(node).span.clone();
        let sides = [
            document.value().pointer(pointer),
            base.and_then(|base| base.pointer(pointer)),
            theirs_resolved.pointer(pointer),
        ];
        let [ours, base, theirs] = texts(sides, "", "", None);
        replacements.push((span, block([&ours, &base, &theirs], labels)));
        Ok(())
    };
    for (parent, group) in parents {
        if group.iter().any(|conflict| conflict.pointer.is_empty()) {
            whole("", &mut replacements)?;
            continue;
        }
        let parent_node = document.node(document.lookup(&parent)?);
        let Children::Members(members) = &parent_node.children else {
            // Arrays are merged whole, so a conflict is never inside one.
            return Err(EditError::internal(format!(
                "a conflict under {parent:?} is not an object's member"
            )));
        };
        let conflicted: Vec<String> = group
            .iter()
            .map(|conflict| split(&conflict.pointer).1)
            .collect();
        let Some(anchor) = members
            .iter()
            .position(|member| !conflicted.contains(&member.name))
        else {
            whole(&parent, &mut replacements)?;
            continue;
        };
        for conflict in group {
            let name = split(&conflict.pointer).1;
            let sides = [
                conflict.ours.as_ref(),
                conflict.base.as_ref(),
                conflict.theirs.as_ref(),
            ];
            let (range, lead, trail) = match members.iter().position(|member| member.name == name) {
                Some(index) if index < anchor => (
                    members[index].name_span.start..members[index + 1].name_span.start,
                    "",
                    ", ",
                ),
                Some(index) => (
                    document.node(members[index - 1].value).span.end
                        ..document.node(members[index].value).span.end,
                    ", ",
                    "",
                ),
                // "Ours" removed it: the sides that have it follow the
                // object's last member.
                None => {
                    let end = document
                        .node(members.last().expect("the anchor is a member").value)
                        .span
                        .end;
                    (end..end, ", ", "")
                }
            };
            let [ours, base, theirs] = texts(sides, lead, trail, Some(&name));
            replacements.push((range, block([&ours, &base, &theirs], labels)));
        }
    }
    replacements.sort_by(|left, right| {
        right
            .0
            .start
            .cmp(&left.0.start)
            .then(right.0.end.cmp(&left.0.end))
    });
    let mut out = bytes.to_vec();
    for (range, text) in replacements {
        out.splice(range, text.into_bytes());
    }
    Ok(out)
}

/// Put `value` at `pointer` -- or take the member away, for `None`.
fn set(root: &mut Value, pointer: &str, value: Option<Value>) {
    if pointer.is_empty() {
        if let Some(value) = value {
            *root = value;
        }
        return;
    }
    let (parent, name) = split(pointer);
    if let Some(Value::Object(members)) = root.pointer_mut(&parent) {
        match value {
            Some(value) => {
                members.insert(name, value);
            }
            None => {
                members.shift_remove(&name);
            }
        }
    }
}

/// A pointer's parent and last token, decoded.
fn split(pointer: &str) -> (String, String) {
    let (parent, token) = pointer.rsplit_once('/').unwrap_or(("", pointer));
    (
        parent.to_string(),
        token.replace("~1", "/").replace("~0", "~"),
    )
}

/// Whether a file holds git's conflict markers: a line of seven `<`, then
/// one of seven `=`, then one of seven `>`.
pub fn has_conflict_markers(raw: &[u8]) -> bool {
    let text = String::from_utf8_lossy(raw);
    let mut stage = 0;
    for line in text.lines() {
        let expected = ["<<<<<<<", "=======", ">>>>>>>"];
        if stage < 3 && line.starts_with(expected[stage]) {
            stage += 1;
        }
    }
    stage == 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn clean(base: &str, ours: &str, theirs: &str) -> String {
        match merge_documents(
            base.as_bytes(),
            ours.as_bytes(),
            theirs.as_bytes(),
            &Labels::default(),
        )
        .unwrap()
        {
            Merged::Clean(bytes) => String::from_utf8(bytes).unwrap(),
            Merged::Conflicted { bytes, .. } => {
                panic!("conflicted:\n{}", String::from_utf8_lossy(&bytes))
            }
        }
    }

    #[test]
    fn test2500_edits_to_different_members_merge_and_keep_ours_style() {
        let base = "{\n    \"id\": \"ada\",\n    \"name\": \"Ada\",\n    \"age\": 36\n}\n";
        let ours = "{\n    \"id\": \"ada\",\n    \"name\": \"Ada L\",\n    \"age\": 36\n}\n";
        let theirs = "{\"id\":\"ada\",\"name\":\"Ada\",\"age\":37,\"tags\":[\"math\"]}";
        assert_eq!(
            clean(base, ours, theirs),
            "{\n    \"id\": \"ada\",\n    \"name\": \"Ada L\",\n    \"age\": 37,\n    \"tags\": [\n        \"math\"\n    ]\n}\n",
            "a new value is written in the file's own style"
        );
        // Removed on one side, untouched on the other: removed.
        assert_eq!(
            clean(base, "{\"id\":\"ada\",\"name\":\"Ada\"}", base),
            "{\"id\":\"ada\",\"name\":\"Ada\"}"
        );
        // Both sides made the same change.
        assert_eq!(clean(base, ours, ours), ours);
        // Nested objects merge member by member.
        assert_eq!(
            clean(
                r#"{"a":{"x":1,"y":1}}"#,
                r#"{"a":{"x":2,"y":1}}"#,
                r#"{"a":{"x":1,"y":3}}"#
            ),
            r#"{"a":{"x":2,"y":3}}"#
        );
        // Added on both sides with no ancestor: merged when compatible.
        assert_eq!(
            clean("", r#"{"a":1,"b":2}"#, r#"{"a":1,"c":3}"#),
            r#"{"a":1,"b":2,"c":3}"#
        );
        // Numbers keep their spelling: 1.50 is not 1.5.
        assert_eq!(
            clean(
                r#"{"p":1.5,"q":1}"#,
                r#"{"p":1.50,"q":1}"#,
                r#"{"p":1.5,"q":2}"#
            ),
            r#"{"p":1.50,"q":2}"#
        );
    }

    fn conflicted(base: &str, ours: &str, theirs: &str) -> (String, Vec<Conflict>) {
        match merge_documents(
            base.as_bytes(),
            ours.as_bytes(),
            theirs.as_bytes(),
            &Labels::default(),
        )
        .unwrap()
        {
            Merged::Conflicted { bytes, conflicts } => {
                (String::from_utf8(bytes).unwrap(), conflicts)
            }
            Merged::Clean(bytes) => panic!("clean: {}", String::from_utf8_lossy(&bytes)),
        }
    }

    /// Keep one side of every conflict, as a person resolving it would.
    fn resolve(text: &str, keep: usize) -> Value {
        let mut out = String::new();
        let mut section: Option<usize> = None;
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
            } else if section.is_none() || section == Some(keep) {
                out.push_str(bare);
            }
        }
        crate::json::parse_str(&out).unwrap_or_else(|error| panic!("{out:?}: {error}"))
    }

    #[test]
    fn test2501_a_conflict_is_marked_around_the_member_alone() {
        let base = r#"{"id":"ada","name":"Ada","age":36}"#;
        let (text, conflicts) = conflicted(
            base,
            r#"{"id":"ada","name":"Ada L","age":37}"#,
            r#"{"id":"ada","name":"Ada Lovelace","age":36}"#,
        );
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].pointer, "/name");
        assert!(has_conflict_markers(text.as_bytes()));
        assert_eq!(text.matches("<<<<<<<").count(), 1, "{text}");
        assert_eq!(
            resolve(&text, 0),
            json!({"id": "ada", "name": "Ada L", "age": 37})
        );
        assert_eq!(
            resolve(&text, 2),
            json!({"id": "ada", "name": "Ada Lovelace", "age": 37})
        );
    }

    #[test]
    fn test2502_every_side_of_every_conflict_resolves_to_json() {
        let cases = [
            // First member, removed on one side and changed on the other.
            (r#"{"a":1,"b":2}"#, r#"{"b":2}"#, r#"{"a":5,"b":2}"#),
            // Last member.
            (r#"{"a":1,"b":2}"#, r#"{"a":1,"b":3}"#, r#"{"a":1}"#),
            // The only member.
            (r#"{"a":1}"#, r#"{"a":2}"#, r#"{"a":3}"#),
            // Added differently on both sides.
            (r#"{"a":1}"#, r#"{"a":1,"n":"x"}"#, r#"{"a":1,"n":"y"}"#),
            // Arrays changed differently: the whole array conflicts.
            (
                r#"{"t":["a"],"z":0}"#,
                r#"{"t":["a","b"],"z":0}"#,
                r#"{"t":["c"],"z":0}"#,
            ),
            // Every member of an object conflicts: the object is marked whole.
            (
                r#"{"a":1,"b":1,"c":1}"#,
                r#"{"a":2,"b":2,"c":2}"#,
                r#"{"a":3,"b":3,"c":3}"#,
            ),
            // Conflicts on both sides of the first member that agrees.
            (
                r#"{"a":1,"b":1,"k":0,"c":1,"d":1}"#,
                r#"{"a":2,"b":2,"k":0,"c":2,"d":2}"#,
                r#"{"b":3,"k":0,"c":3,"e":5}"#,
            ),
            // Nested: a conflict inside an object member.
            (
                r#"{"o":{"x":1,"y":1},"z":1}"#,
                r#"{"o":{"x":2,"y":1},"z":1}"#,
                r#"{"o":{"x":3,"y":2},"z":1}"#,
            ),
            // The whole document replaced by something not an object.
            (r#"{"a":1}"#, "[1]", "[2]"),
        ];
        for (base, ours, theirs) in cases {
            let (text, conflicts) = conflicted(base, ours, theirs);
            assert!(!conflicts.is_empty());
            for keep in [0, 1, 2] {
                resolve(&text, keep);
            }
        }
    }

    fn object() -> impl proptest::strategy::Strategy<Value = Value> {
        use proptest::prelude::*;
        let leaf = prop_oneof![
            (0i64..3).prop_map(|n| json!(n)),
            "[ab]".prop_map(Value::String),
            prop::collection::vec((0i64..2).prop_map(|n| json!(n)), 0..2).prop_map(Value::Array),
        ];
        let member = leaf.prop_recursive(2, 8, 3, |inner| {
            prop::collection::btree_map("[a-d]", inner, 0..3)
                .prop_map(|members| Value::Object(members.into_iter().collect()))
        });
        prop::collection::btree_map("[a-e]", member, 0..5)
            .prop_map(|members| Value::Object(members.into_iter().collect()))
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig { cases: 512, ..Default::default() })]

        #[test]
        fn test2504_any_merge_is_clean_and_right_or_resolves_to_json(
            base in object(), ours in object(), theirs in object(), pretty in proptest::prelude::any::<bool>()
        ) {
            let render = |value: &Value| if pretty {
                serde_json::to_string_pretty(value).unwrap()
            } else {
                value.to_string()
            };
            let (base_text, ours_text, theirs_text) = (render(&base), render(&ours), render(&theirs));
            let merged = merge_documents(base_text.as_bytes(), ours_text.as_bytes(), theirs_text.as_bytes(), &Labels::default()).unwrap();
            match merged {
                Merged::Clean(bytes) => {
                    let value = crate::json::parse(&bytes).unwrap();
                    let (expected, conflicts) = merge_values(Some(&base), &ours, &theirs);
                    proptest::prop_assert!(conflicts.is_empty());
                    proptest::prop_assert_eq!(value, expected);
                    if ours == base {
                        proptest::prop_assert_eq!(crate::json::parse(&bytes).unwrap(), theirs.clone());
                    }
                }
                Merged::Conflicted { bytes, conflicts } => {
                    proptest::prop_assert!(!conflicts.is_empty());
                    let text = String::from_utf8(bytes).unwrap();
                    for keep in [0, 1, 2] {
                        resolve(&text, keep);
                    }
                    // Kept "ours" throughout, the file is ours with theirs'
                    // clean changes: never less than either side agreed on.
                    let (expected, _) = merge_values(Some(&base), &ours, &theirs);
                    proptest::prop_assert_eq!(resolve(&text, 0), expected);
                }
            }
        }
    }

    #[test]
    fn test2503_a_side_that_is_not_json_is_named() {
        let error = merge_documents(b"{}", b"{", b"{}", &Labels::default()).unwrap_err();
        assert!(
            error.to_string().starts_with("ours is not a JSON document"),
            "{error}"
        );
        assert!(!has_conflict_markers(b"{\"a\": \"<<<<<<< not a marker\"}"));
    }
}
