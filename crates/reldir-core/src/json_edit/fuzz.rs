//! The fuzzing entry point: any bytes in, and a panic only when an edit
//! disturbs bytes it does not own or writes something other than intended.

use super::{
    BOM, EditError, apply, edit_document, first_difference, render,
    scan::{Children, Document, Kind, NodeId},
    style::Style,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// How many edits of each kind one input is put through, so a large input
/// costs a bounded number of re-parses.
const EDITS_PER_KIND: usize = 24;

/// For any input that parses:
///
/// - the scanner agrees with the strict parser on validity and with
///   `serde_json` on the value;
/// - no edits, or a diff to the value it already holds, leave the bytes
///   unchanged;
/// - replacing a scalar changes exactly that scalar's bytes;
/// - removing a member or element changes nothing outside its container,
///   keeps every sibling's text, and reads back without it;
/// - inserting a member into a non-empty object is a pure insertion, and every
///   insertion reads back with the member last.
pub fn fuzz_roundtrip(data: &[u8]) {
    crate::json::with_depth_limit(64, || roundtrip(data));
}

fn roundtrip(data: &[u8]) {
    // Detection tolerates anything, parsed or not.
    let _ = Style::detect(data);
    let body = data.strip_prefix(BOM).unwrap_or(data);
    let verdict = crate::json::parse(body);
    let document = match Document::parse(data) {
        Ok(document) => document,
        Err(EditError::Invalid(error)) => {
            assert!(
                verdict.is_err() || error.to_string().contains("not finite"),
                "the scanner refused what the parser accepts: {error}"
            );
            return;
        }
        Err(other) => panic!("reading {data:?} failed unexpectedly: {other}"),
    };
    assert!(
        verdict.is_ok(),
        "the scanner accepted what the parser refuses"
    );
    let mut reader = serde_json::Deserializer::from_slice(body);
    reader.disable_recursion_limit();
    let reference = Value::deserialize(&mut reader).expect("serde_json reads an accepted document");
    assert_eq!(
        first_difference(document.value(), &reference),
        None,
        "the scanner's value differs from serde_json's"
    );

    assert_eq!(apply(data, &[]).expect("no edits apply"), data);
    assert_eq!(
        edit_document(data, document.value()).expect("an unchanged value applies"),
        data
    );

    let pointer_of = |id: NodeId| document.pointer(id);
    let mut scalars = 0;
    let mut removals = 0;
    let mut insertions = 0;
    for (id, node) in document.nodes().iter().enumerate() {
        match &node.children {
            Children::Scalar if scalars < EDITS_PER_KIND => {
                scalars += 1;
                replace_scalar(&document, data, id, &pointer_of(id));
            }
            Children::Scalar => {}
            Children::Members(_) | Children::Elements(_) => {
                for index in 0..node.len() {
                    if removals < EDITS_PER_KIND {
                        removals += 1;
                        remove_child(&document, data, id, index);
                    }
                }
                if node.kind == Kind::Object && insertions < EDITS_PER_KIND {
                    insertions += 1;
                    insert_member(&document, data, id);
                }
            }
        }
    }
}

fn replace_scalar(document: &Document<'_>, data: &[u8], id: NodeId, pointer: &str) {
    let node = document.node(id);
    let replacement = match node.kind {
        Kind::Null => json!(true),
        Kind::Bool => Value::Null,
        Kind::Number => json!("é\n\"x\""),
        Kind::String => "1.50".parse().map(Value::Number).expect("a number"),
        Kind::Object | Kind::Array => unreachable!("a scalar"),
    };
    let edited = apply(
        data,
        &[super::Edit::Replace {
            pointer: pointer.to_string(),
            value: replacement.clone(),
        }],
    )
    .unwrap_or_else(|error| panic!("replacing {pointer:?}: {error}"));
    let mut expected = data[..node.span.start].to_vec();
    expected.extend_from_slice(&render(&replacement, &Style::default_for_new_table(), 0));
    expected.extend_from_slice(&data[node.span.end..]);
    assert_eq!(
        edited, expected,
        "replacing {pointer:?} touched other bytes"
    );
}

fn remove_child(document: &Document<'_>, data: &[u8], container: NodeId, index: usize) {
    let node = document.node(container);
    let child = node.child_value(index);
    let pointer = document.pointer(child);
    let edited = apply(
        data,
        &[super::Edit::Remove {
            pointer: pointer.clone(),
        }],
    )
    .unwrap_or_else(|error| panic!("removing {pointer:?}: {error}"));
    // Outside the container, nothing moved.
    assert!(
        edited.starts_with(&data[..node.span.start])
            && edited.ends_with(&data[node.span.end..])
            && edited.len() <= data.len(),
        "removing {pointer:?} touched bytes outside its container"
    );
    // The value without the child, computed by serde_json's own pointer.
    let mut expected = document.value().clone();
    let parent = expected
        .pointer_mut(&document.pointer(container))
        .expect("the container is in the value");
    match parent {
        Value::Object(members) => {
            let name = members
                .keys()
                .nth(index)
                .cloned()
                .expect("the member exists");
            members.shift_remove(&name);
        }
        Value::Array(elements) => {
            elements.remove(index);
        }
        _ => unreachable!("a container"),
    }
    let reread = Document::parse(&edited).expect("the edit reads back");
    assert_eq!(
        first_difference(reread.value(), &expected),
        None,
        "removing {pointer:?}"
    );
    // Every sibling keeps its text.
    let new_container = reread
        .lookup(&document.pointer(container))
        .expect("the container remains");
    let kept: Vec<NodeId> = (0..node.len())
        .filter(|sibling| *sibling != index)
        .map(|sibling| node.child_value(sibling))
        .collect();
    for (position, old) in kept.into_iter().enumerate() {
        let new = reread.node(new_container).child_value(position);
        assert_eq!(
            &data[document.node(old).span.clone()],
            &edited[reread.node(new).span.clone()],
            "removing {pointer:?} rewrote a sibling"
        );
    }
}

fn insert_member(document: &Document<'_>, data: &[u8], container: NodeId) {
    let node = document.node(container);
    let Children::Members(members) = &node.children else {
        unreachable!("an object");
    };
    let mut name = "new~/\u{e9}".to_string();
    while members.iter().any(|member| member.name == name) {
        name.push('+');
    }
    let value: Value =
        serde_json::from_str(r#"{"n": [1.50, -0, 1e2, []], "s": "é"}"#).expect("a literal value");
    let pointer = document.pointer(container);
    let edited = apply(
        data,
        &[super::Edit::Insert {
            pointer: pointer.clone(),
            name: name.clone(),
            value: value.clone(),
            after: None,
        }],
    )
    .unwrap_or_else(|error| panic!("inserting into {pointer:?}: {error}"));
    let mut expected = document.value().clone();
    expected
        .pointer_mut(&pointer)
        .and_then(Value::as_object_mut)
        .expect("the object is in the value")
        .insert(name, value);
    let reread = Document::parse(&edited).expect("the edit reads back");
    assert_eq!(
        first_difference(reread.value(), &expected),
        None,
        "inserting into {pointer:?}"
    );
    if node.is_empty() {
        assert!(
            edited.starts_with(&data[..node.span.start])
                && edited.ends_with(&data[node.span.end..]),
            "inserting into empty {pointer:?} touched bytes outside it"
        );
    } else {
        // Every original byte is still there, in one piece either side of the
        // insertion: what the two share at the front and at the back covers
        // the whole original.
        let prefix = data
            .iter()
            .zip(&edited)
            .take_while(|(left, right)| left == right)
            .count();
        let suffix = data
            .iter()
            .rev()
            .zip(edited.iter().rev())
            .take_while(|(left, right)| left == right)
            .count();
        assert!(
            edited.len() > data.len() && prefix + suffix >= data.len(),
            "inserting into {pointer:?} rewrote existing bytes"
        );
    }
}
