use super::*;
use proptest::prelude::*;
use serde_json::{Map, json};

/// A number spelled exactly as given.
fn number(lexeme: &str) -> Value {
    let read = value(lexeme);
    assert!(read.is_number(), "{lexeme} is a number");
    read
}

fn value(text: &str) -> Value {
    serde_json::from_str(text).expect("a JSON literal")
}

fn replace(pointer: &str, value: Value) -> Edit {
    Edit::Replace {
        pointer: pointer.into(),
        value,
    }
}

fn remove(pointer: &str) -> Edit {
    Edit::Remove {
        pointer: pointer.into(),
    }
}

fn insert(pointer: &str, name: &str, value: Value, after: Option<&str>) -> Edit {
    Edit::Insert {
        pointer: pointer.into(),
        name: name.into(),
        value,
        after: after.map(str::to_string),
    }
}

fn array_insert(pointer: &str, index: usize, value: Value) -> Edit {
    Edit::ArrayInsert {
        pointer: pointer.into(),
        index,
        value,
    }
}

/// The document after `edits`, as text.
fn edited(raw: &str, edits: &[Edit]) -> String {
    let bytes = apply(raw.as_bytes(), edits)
        .unwrap_or_else(|error| panic!("{edits:?} on {raw:?} failed: {error}"));
    String::from_utf8(bytes).expect("UTF-8 in, UTF-8 out")
}

fn refused(raw: &str, edits: &[Edit]) -> EditError {
    apply(raw.as_bytes(), edits).expect_err("the edits must be refused")
}

/// The scanner records, for every member and element, the spans an edit
/// needs: the name, the colon, the value and the entry from just after the
/// preceding bracket or comma. With a byte order mark, spans are offsets into
/// the file as it is, mark included.
#[test]
fn test8400_the_scanner_records_every_span() {
    let raw = "\u{feff}{ \"a\" : [1, {\"b\":true}],\n  \"c\": \"x\" }";
    let document = Document::parse(raw.as_bytes()).unwrap();
    assert!(document.bom());
    let text = |span: std::ops::Range<usize>| &raw[span];
    let root = document.node(document.root());
    assert_eq!(text(root.span.clone()), &raw[3..]);
    let Children::Members(members) = &root.children else {
        panic!("the root is an object");
    };
    assert_eq!(members[0].name, "a");
    assert_eq!(text(members[0].name_span.clone()), "\"a\"");
    assert_eq!(&raw[members[0].colon..members[0].colon + 1], ":");
    assert_eq!(text(members[0].entry.clone()), " \"a\" : [1, {\"b\":true}]");
    assert_eq!(text(members[1].entry.clone()), "\n  \"c\": \"x\"");
    let array = document.node(members[0].value);
    assert_eq!(text(array.span.clone()), "[1, {\"b\":true}]");
    let Children::Elements(elements) = &array.children else {
        panic!("an array");
    };
    assert_eq!(text(elements[0].entry.clone()), "1");
    assert_eq!(text(elements[1].entry.clone()), " {\"b\":true}");
    let inner = document.lookup("/a/1/b").unwrap();
    assert_eq!(text(document.node(inner).span.clone()), "true");
    assert_eq!(document.pointer(inner), "/a/1/b");
    assert_eq!(document.node(inner).parent, Some((elements[1].value, 0)));
}

/// The scanner accepts exactly what the strict parser accepts, plus a byte
/// order mark: comments, duplicate names, trailing content, numbers with no
/// finite value and nesting past the configured limit are all refused.
#[test]
fn test8401_the_scanner_accepts_exactly_what_the_strict_parser_accepts() {
    for bad in [
        "{\"a\": 1 // note\n}",
        "{/* note */\"a\": 1}",
        "{\"a\": 1, \"a\": 2}",
        "{\"a\": 1} {}",
        "{\"a\": 1,}",
        "[1e999]",
        "[-1e999]",
        "",
        "\u{feff}",
        "\u{feff}\u{feff}{}",
        "{\"a\": NaN}",
    ] {
        let error = Document::parse(bad.as_bytes()).expect_err(bad);
        assert!(matches!(error, EditError::Invalid(_)), "{bad:?}: {error}");
    }
    for good in ["{}", "\u{feff}{}", " [1, 2] \n", "\"text\"", "1e-999", "-0"] {
        Document::parse(good.as_bytes()).unwrap_or_else(|error| panic!("{good:?}: {error}"));
    }
    let deep = format!("{}{}", "[".repeat(40), "]".repeat(40));
    crate::json::with_depth_limit(16, || {
        let EditError::Invalid(error) = Document::parse(deep.as_bytes()).unwrap_err() else {
            panic!("a depth refusal is an invalid document");
        };
        assert!(crate::json::is_depth_limit(&error), "{error}");
    });
    crate::json::with_depth_limit(64, || {
        Document::parse(deep.as_bytes()).expect("40 levels are within 64");
    });
}

/// The value read keeps what the file wrote: number lexemes, string content
/// (escapes decoded, normalization form untouched) and member order. It is
/// the value serde_json itself reads.
#[test]
fn test8402_the_value_read_keeps_spelling_and_order() {
    let raw = "{\"z\": 1.50, \"big\": 12345678901234567890123, \"neg\": -0, \"esc\": \"caf\\u00e9\", \"nfd\": \"cafe\u{301}\", \"a\": [1, 1.0]}";
    let document = Document::parse(raw.as_bytes()).unwrap();
    let read = document.value();
    assert_eq!(read["z"].to_string(), "1.50");
    assert_eq!(read["big"].to_string(), "12345678901234567890123");
    assert_eq!(read["neg"].to_string(), "-0");
    assert_eq!(read["esc"], json!("café"));
    assert_eq!(read["nfd"], Value::String("cafe\u{301}".into()));
    assert_ne!(read["nfd"], json!("café"), "NFD is not NFC");
    assert_ne!(
        read["a"][0], read["a"][1],
        "1 and 1.0 are spelled differently"
    );
    assert_eq!(
        read.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["z", "big", "neg", "esc", "nfd", "a"]
    );
    let reference: Value = serde_json::from_str(raw).unwrap();
    assert_eq!(first_difference(read, &reference), None);
    assert_eq!(
        first_difference(&json!({"a": 1, "b": 2}), &json!({"b": 2, "a": 1})),
        Some(String::new())
    );
    assert_eq!(
        first_difference(&json!({"a": [1, {"b": 2}]}), &json!({"a": [1, {"b": 3}]})),
        Some("/a/1/b".into())
    );
}

/// Pointers are read by RFC 6901 and nothing looser, and a pointer to
/// nothing is an error naming it.
#[test]
fn test8403_pointers_are_strict() {
    let raw = r#"{"a/b": {"c~d": [10, 20]}, "": 1}"#;
    let document = Document::parse(raw.as_bytes()).unwrap();
    let at = |pointer: &str| {
        let id = document.lookup(pointer).unwrap();
        &raw[document.node(id).span.clone()]
    };
    assert_eq!(at("/a~1b/c~0d/1"), "20");
    assert_eq!(at("/"), "1");
    assert_eq!(at(""), raw);
    for (pointer, reason) in [
        ("a", "begins with '/'"),
        ("/a~2b", "'~' is followed"),
        ("/a~", "'~' is followed"),
        ("/a~1b/c~0d/01", "without leading zeros"),
        ("/a~1b/c~0d/-", "without leading zeros"),
        ("/a~1b/c~0d/+1", "without leading zeros"),
    ] {
        match document.lookup(pointer) {
            Err(EditError::Pointer { reason: given, .. }) => {
                assert!(given.contains(reason), "{pointer}: {given}")
            }
            other => panic!("{pointer}: {other:?}"),
        }
    }
    assert!(matches!(
        document.lookup("/a~1b/c~0d/2"),
        Err(EditError::Index {
            index: 2,
            len: 2,
            ..
        })
    ));
    assert!(matches!(
        document.lookup("/missing"),
        Err(EditError::Missing { pointer }) if pointer == "/missing"
    ));
    assert!(matches!(
        document.lookup("/a~1b/c~0d/0/deeper"),
        Err(EditError::Missing { .. })
    ));
}

/// Style detection measures each property from the file, by majority.
#[test]
fn test8404_style_is_measured_from_the_file() {
    let style = |indent: &str,
                 colon: Colon,
                 comma: Comma,
                 newline: Newline,
                 final_newline: bool,
                 bom: bool,
                 empty: Empty,
                 padding: bool,
                 inline_scalar_arrays: bool| Style {
        indent: Indent::new(indent).unwrap(),
        colon,
        comma,
        newline,
        final_newline,
        bom,
        empty,
        padding,
        inline_scalar_arrays,
    };
    use {Colon as C, Comma as K, Empty as E, Newline as N};
    let cases = [
        (
            "{\n  \"a\": 1,\n  \"b\": [\n    1\n  ]\n}\n",
            style(
                "  ",
                C::Spaced,
                K::Spaced,
                N::Lf,
                true,
                false,
                E::Tight,
                false,
                false,
            ),
        ),
        (
            "{\n    \"a\": {\n        \"b\": 1\n    }\n}",
            style(
                "    ",
                C::Spaced,
                K::Spaced,
                N::Lf,
                false,
                false,
                E::Tight,
                false,
                false,
            ),
        ),
        (
            "{\n\t\"a\": [1, 2],\n\t\"b\": {}\n}\n",
            style(
                "\t",
                C::Spaced,
                K::Spaced,
                N::Lf,
                true,
                false,
                E::Tight,
                false,
                true,
            ),
        ),
        (
            "{\"a\":1,\"b\":[1,2],\"c\":{}}",
            style(
                "",
                C::Tight,
                K::Tight,
                N::Lf,
                false,
                false,
                E::Tight,
                false,
                false,
            ),
        ),
        (
            "{\"a\": 1, \"b\": [1, 2]}\n",
            style(
                "",
                C::Spaced,
                K::Spaced,
                N::Lf,
                true,
                false,
                E::Tight,
                false,
                false,
            ),
        ),
        (
            "\u{feff}{\r\n  \"a\" : { },\r\n  \"b\" : 2\r\n}\r\n",
            style(
                "  ",
                C::Surrounded,
                K::Spaced,
                N::CrLf,
                true,
                true,
                E::Spaced,
                false,
                false,
            ),
        ),
        (
            "{ \"a\": [ 1, 2 ], \"b\": { \"c\": 1 } }",
            style(
                "",
                C::Spaced,
                K::Spaced,
                N::Lf,
                false,
                false,
                E::Tight,
                true,
                false,
            ),
        ),
        (
            // Three two-space steps outvote one four-space step.
            "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": [\n      3\n  ]\n}",
            style(
                "  ",
                C::Spaced,
                K::Spaced,
                N::Lf,
                false,
                false,
                E::Tight,
                false,
                false,
            ),
        ),
    ];
    for (raw, expected) in cases {
        assert_eq!(Style::detect(raw.as_bytes()), expected, "{raw:?}");
    }
    // Nothing observed: a one-line document with fallbacks, and the evidence
    // says so.
    let detection = Style::detect_detailed(b"[]");
    assert!(detection.style.is_compact());
    assert_eq!(detection.evidence.indent, 0);
    assert_eq!(detection.evidence.colon, 0);
    assert_eq!(detection.evidence.empty, 1);
    // Any bytes at all are surveyed without panicking.
    for junk in [
        &b"{\"a\": [1, 2"[..],
        b"\xff\xfe}}]]",
        b"\"unterminated",
        b"\xEF\xBB",
        b":,:,",
    ] {
        let _ = Style::detect(junk);
    }
}

/// A table records its style in its schema; the recorded shape is fixed, and
/// a style that could not be written back is refused rather than read.
#[test]
fn test8405_a_style_serializes_to_its_documented_shape() {
    let default = Style::default_for_new_table();
    assert_eq!(
        serde_json::to_value(&default).unwrap(),
        json!({
            "indent": "  ", "colon": ": ", "comma": ", ", "newline": "\n",
            "final_newline": true, "bom": false, "empty": "{}", "padding": false,
            "inline_scalar_arrays": false
        })
    );
    let tabbed = Style {
        indent: Indent::tabs(1).unwrap(),
        colon: Colon::Surrounded,
        newline: Newline::CrLf,
        empty: Empty::Spaced,
        ..default.clone()
    };
    let text = serde_json::to_string(&tabbed).unwrap();
    assert_eq!(serde_json::from_str::<Style>(&text).unwrap(), tabbed);
    let mut record = serde_json::to_value(&default).unwrap();
    for (field, bad) in [
        ("indent", json!("  \t")),
        ("indent", json!("x")),
        ("indent", json!(" ".repeat(17))),
        ("colon", json!("=")),
        ("newline", json!("\r")),
        ("empty", json!("{\n}")),
    ] {
        let mut broken = record.clone();
        broken[field] = bad.clone();
        assert!(
            serde_json::from_value::<Style>(broken).is_err(),
            "{field} = {bad} must be refused"
        );
    }
    record["unknown"] = json!(true);
    assert!(serde_json::from_value::<Style>(record).is_err());
    assert!(Indent::spaces(16).is_ok() && Indent::spaces(17).is_err());
    assert!(Indent::tabs(4).is_ok() && Indent::tabs(5).is_err());
}

/// Rendering writes members in the value's order, numbers as their lexemes
/// and strings as their characters, escaping only what JSON requires.
#[test]
fn test8406_rendering_is_exact() {
    let input = value(
        r#"{"z": "caf\u00e9 \"q\" \\ \n\t\r\u0008\u000c\u0001\u001f /", "a": [1.50, 1e2, -0, 12345678901234567890123], "e": {}, "f": []}"#,
    );
    let mut with_nfd = input.clone();
    with_nfd["nfd"] = Value::String("e\u{301}\u{2028}".into());
    let compact = Style {
        indent: Indent::compact(),
        colon: Colon::Tight,
        comma: Comma::Tight,
        final_newline: false,
        ..Style::default_for_new_table()
    };
    assert_eq!(
        String::from_utf8(render(&with_nfd, &compact, 0)).unwrap(),
        "{\"z\":\"café \\\"q\\\" \\\\ \\n\\t\\r\\b\\f\\u0001\\u001f /\",\"a\":[1.50,1e+2,-0,12345678901234567890123],\"e\":{},\"f\":[],\"nfd\":\"e\u{301}\u{2028}\"}"
    );
    let document = value(r#"{"a": [1, {"b": []}], "c": {"d": null}}"#);
    let crlf_tabs = Style {
        indent: Indent::tabs(1).unwrap(),
        newline: Newline::CrLf,
        bom: true,
        empty: Empty::Spaced,
        ..Style::default_for_new_table()
    };
    assert_eq!(
        String::from_utf8(render_document(&document, &crlf_tabs)).unwrap(),
        "\u{feff}{\r\n\t\"a\": [\r\n\t\t1,\r\n\t\t{\r\n\t\t\t\"b\": [ ]\r\n\t\t}\r\n\t],\r\n\t\"c\": {\r\n\t\t\"d\": null\r\n\t}\r\n}\r\n"
    );
    let inline = Style {
        inline_scalar_arrays: true,
        padding: true,
        ..Style::default_for_new_table()
    };
    assert_eq!(
        String::from_utf8(render(
            &value(r#"{"t": [1, "x"], "o": [{"k": 1}]}"#),
            &inline,
            1
        ))
        .unwrap(),
        "{\n    \"t\": [ 1, \"x\" ],\n    \"o\": [\n      {\n        \"k\": 1\n      }\n    ]\n  }"
    );
    // Rendering reads back as the value, member order included.
    let written = render_document(&with_nfd, &crlf_tabs);
    let reread = Document::parse(&written).unwrap();
    assert_eq!(first_difference(reread.value(), &with_nfd), None);
}

/// Replacing a scalar rewrites the scalar and nothing else, whatever the file
/// looks like around it.
#[test]
fn test8407_replacing_a_scalar_rewrites_only_it() {
    let cases = [
        (
            "{\n  \"id\": \"a\",\n  \"price\": 1.50,\n  \"tags\": [\"x\", \"y\"]\n}\n",
            replace("/price", number("2.25")),
            "{\n  \"id\": \"a\",\n  \"price\": 2.25,\n  \"tags\": [\"x\", \"y\"]\n}\n",
        ),
        (
            "{\"id\":\"a\",\"n\":1}",
            replace("/n", json!("x")),
            "{\"id\":\"a\",\"n\":\"x\"}",
        ),
        (
            "\u{feff}{\r\n\t\"a\": 1,\r\n\t\"b\": null\r\n}",
            replace("/a", json!(true)),
            "\u{feff}{\r\n\t\"a\": true,\r\n\t\"b\": null\r\n}",
        ),
        (
            "{\n  \"tags\": [\"x\", \"y\"]\n}",
            replace("/tags/1", json!("é\"")),
            "{\n  \"tags\": [\"x\", \"é\\\"\"]\n}",
        ),
        ("  42  ", replace("", json!(7)), "  7  "),
    ];
    for (raw, edit, expected) in cases {
        assert_eq!(edited(raw, &[edit]), expected, "{raw:?}");
    }
}

/// A container written in place of a value is indented from the line the old
/// value starts on, in the file's own indentation unit.
#[test]
fn test8408_a_replacement_is_indented_from_its_line() {
    let raw = "{\n  \"id\": \"a\",\n  \"meta\": null\n}\n";
    assert_eq!(
        edited(
            raw,
            &[replace(
                "/meta",
                value(r#"{"k": [1, {"z": true}], "e": {}}"#)
            )]
        ),
        "{\n  \"id\": \"a\",\n  \"meta\": {\n    \"k\": [\n      1,\n      {\n        \"z\": true\n      }\n    ],\n    \"e\": {}\n  }\n}\n"
    );
    let tabbed = "{\n\t\"a\": {\n\t\t\"b\": [\n\t\t\t1\n\t\t]\n\t}\n}";
    assert_eq!(
        edited(tabbed, &[replace("/a/b", value(r#"{"c": [2]}"#))]),
        "{\n\t\"a\": {\n\t\t\"b\": {\n\t\t\t\"c\": [\n\t\t\t\t2\n\t\t\t]\n\t\t}\n\t}\n}"
    );
    let crlf = "{\r\n  \"a\": 1\r\n}\r\n";
    assert_eq!(
        edited(crlf, &[replace("/a", json!({"b": 2}))]),
        "{\r\n  \"a\": {\r\n    \"b\": 2\r\n  }\r\n}\r\n"
    );
}

/// What was written on one line stays on one line: a one-line container's
/// replacement, and anything written into a one-line container.
#[test]
fn test8409_one_line_content_stays_on_one_line() {
    let raw = "{\n  \"tags\": [\"a\", \"b\"],\n  \"pos\": {\"x\": 1, \"y\": 2}\n}";
    assert_eq!(
        edited(raw, &[replace("/tags", value(r#"["c", {"d": 1}]"#))]),
        "{\n  \"tags\": [\"c\", {\"d\": 1}],\n  \"pos\": {\"x\": 1, \"y\": 2}\n}"
    );
    assert_eq!(
        edited(raw, &[replace("/pos/x", value(r#"{"deep": [1, 2]}"#))]),
        "{\n  \"tags\": [\"a\", \"b\"],\n  \"pos\": {\"x\": {\"deep\": [1, 2]}, \"y\": 2}\n}"
    );
    // A one-line document stays one line.
    assert_eq!(
        edited(
            "{\"a\":1,\"b\":2}\n",
            &[replace("/a", value(r#"{"c": [1, {"d": []}]}"#))]
        ),
        "{\"a\":{\"c\":[1,{\"d\":[]}]},\"b\":2}\n"
    );
}

/// Removing a member takes it with one comma and its own whitespace, so what
/// remains is laid out as if it had never been there.
#[test]
fn test8410_removing_from_an_indented_object() {
    let raw = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3\n}\n";
    assert_eq!(
        edited(raw, &[remove("/a")]),
        "{\n  \"b\": 2,\n  \"c\": 3\n}\n"
    );
    assert_eq!(
        edited(raw, &[remove("/b")]),
        "{\n  \"a\": 1,\n  \"c\": 3\n}\n"
    );
    assert_eq!(
        edited(raw, &[remove("/c")]),
        "{\n  \"a\": 1,\n  \"b\": 2\n}\n"
    );
    assert_eq!(
        edited(raw, &[remove("/a"), remove("/c")]),
        "{\n  \"b\": 2\n}\n"
    );
    assert_eq!(
        edited(raw, &[remove("/a"), remove("/b")]),
        "{\n  \"c\": 3\n}\n"
    );
    assert_eq!(
        edited(raw, &[remove("/b"), remove("/c")]),
        "{\n  \"a\": 1\n}\n"
    );
    assert_eq!(
        edited(raw, &[remove("/a"), remove("/b"), remove("/c")]),
        "{}\n"
    );
    let nested = "{\n  \"x\": {\n    \"a\": 1\n  },\n  \"y\": 2\n}";
    assert_eq!(
        edited(nested, &[remove("/x/a")]),
        "{\n  \"x\": {},\n  \"y\": 2\n}"
    );
    assert_eq!(edited(nested, &[remove("/x")]), "{\n  \"y\": 2\n}");
}

#[test]
fn test8411_removing_from_a_one_line_object() {
    let raw = "{\"a\":1,\"b\":2,\"c\":3}";
    assert_eq!(edited(raw, &[remove("/a")]), "{\"b\":2,\"c\":3}");
    assert_eq!(edited(raw, &[remove("/b")]), "{\"a\":1,\"c\":3}");
    assert_eq!(edited(raw, &[remove("/c")]), "{\"a\":1,\"b\":2}");
    let padded = "{ \"a\": 1, \"b\": 2 }";
    assert_eq!(edited(padded, &[remove("/a")]), "{ \"b\": 2 }");
    assert_eq!(edited(padded, &[remove("/b")]), "{ \"a\": 1 }");
    assert_eq!(edited("{ \"a\": 1 }", &[remove("/a")]), "{}");
    // An emptied container is written the way the file writes empty ones.
    assert_eq!(
        edited("{\"a\": { }, \"b\": {\"x\": 1}}", &[remove("/b/x")]),
        "{\"a\": { }, \"b\": { }}"
    );
}

#[test]
fn test8412_removing_keeps_line_endings_the_mark_and_the_missing_final_newline() {
    let raw = "\u{feff}{\r\n  \"a\": 1,\r\n  \"b\": [\r\n    1,\r\n    2\r\n  ]\r\n}";
    assert_eq!(
        edited(raw, &[remove("/a")]),
        "\u{feff}{\r\n  \"b\": [\r\n    1,\r\n    2\r\n  ]\r\n}"
    );
    assert_eq!(
        edited(raw, &[remove("/b/1")]),
        "\u{feff}{\r\n  \"a\": 1,\r\n  \"b\": [\r\n    1\r\n  ]\r\n}"
    );
    assert_eq!(
        edited(raw, &[remove("/b/0")]),
        "\u{feff}{\r\n  \"a\": 1,\r\n  \"b\": [\r\n    2\r\n  ]\r\n}"
    );
}

#[test]
fn test8413_removing_array_elements() {
    let raw = "[1, 2, 3, 4, 5]";
    assert_eq!(edited(raw, &[remove("/1"), remove("/2")]), "[1, 4, 5]");
    assert_eq!(edited(raw, &[remove("/0"), remove("/1")]), "[3, 4, 5]");
    assert_eq!(edited(raw, &[remove("/3"), remove("/4")]), "[1, 2, 3]");
    assert_eq!(edited(raw, &[remove("/0"), remove("/4")]), "[2, 3, 4]");
    assert_eq!(edited(raw, &[remove("/1"), remove("/3")]), "[1, 3, 5]");
    let all: Vec<Edit> = (0..5).map(|index| remove(&format!("/{index}"))).collect();
    assert_eq!(edited(raw, &all), "[]");
    let indented = "[\n  1,\n  2,\n  3\n]";
    assert_eq!(edited(indented, &[remove("/0"), remove("/2")]), "[\n  2\n]");
}

/// An inserted member goes on its own line, indented like its siblings, or
/// into a one-line object with that object's own spacing.
#[test]
fn test8414_inserting_members() {
    let raw = "{\n  \"a\": 1,\n  \"b\": 2\n}\n";
    assert_eq!(
        edited(raw, &[insert("", "c", json!([1, 2]), None)]),
        "{\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": [\n    1,\n    2\n  ]\n}\n"
    );
    assert_eq!(
        edited(raw, &[insert("", "x", json!("v"), Some("a"))]),
        "{\n  \"a\": 1,\n  \"x\": \"v\",\n  \"b\": 2\n}\n"
    );
    assert_eq!(
        edited("{\n  \"a\": {}\n}", &[insert("/a", "k", json!(1), None)]),
        "{\n  \"a\": {\n    \"k\": 1\n  }\n}"
    );
    assert_eq!(
        edited("{\"a\":1}", &[insert("", "b", json!({"c": [true]}), None)]),
        "{\"a\":1,\"b\":{\"c\":[true]}}"
    );
    assert_eq!(
        edited(
            "{\"a\": 1, \"b\": 2}",
            &[insert("", "c", json!(3), Some("a"))]
        ),
        "{\"a\": 1, \"c\": 3, \"b\": 2}"
    );
    assert_eq!(
        edited("{}\n", &[insert("", "a", json!(1), None)]),
        "{\"a\":1}\n"
    );
    assert_eq!(
        edited(
            "{\n\t\"a\": 1\n}",
            &[insert("", "b", json!({"c": 2}), None)]
        ),
        "{\n\t\"a\": 1,\n\t\"b\": {\n\t\t\"c\": 2\n\t}\n}"
    );
    assert_eq!(
        edited("{\r\n  \"a\": 1\r\n}", &[insert("", "b", json!(2), None)]),
        "{\r\n  \"a\": 1,\r\n  \"b\": 2\r\n}"
    );
    // The new member's colon is copied from its sibling.
    assert_eq!(
        edited("{\"a\" : 1}", &[insert("", "b", json!(2), None)]),
        "{\"a\" : 1, \"b\" : 2}"
    );
    // Names are escaped only as JSON requires.
    assert_eq!(
        edited("{\"a\":1}", &[insert("", "é\"/~", json!(2), None)]),
        "{\"a\":1,\"é\\\"/~\":2}"
    );
}

#[test]
fn test8415_inserting_array_elements() {
    assert_eq!(
        edited("[1, 2]", &[array_insert("", 0, json!(0))]),
        "[0, 1, 2]"
    );
    assert_eq!(
        edited("[1, 2]", &[array_insert("", 1, json!(9))]),
        "[1, 9, 2]"
    );
    assert_eq!(
        edited("[1, 2]", &[array_insert("", 2, json!(3))]),
        "[1, 2, 3]"
    );
    assert_eq!(
        edited("{\"a\": []}", &[array_insert("/a", 0, json!("x"))]),
        "{\"a\": [\"x\"]}"
    );
    let raw = "{\n  \"a\": [\n    1\n  ]\n}";
    assert_eq!(
        edited(raw, &[array_insert("/a", 0, json!(0))]),
        "{\n  \"a\": [\n    0,\n    1\n  ]\n}"
    );
    assert_eq!(
        edited(raw, &[array_insert("/a", 1, json!({"k": 1}))]),
        "{\n  \"a\": [\n    1,\n    {\n      \"k\": 1\n    }\n  ]\n}"
    );
    // Insertions at one index land in the order listed; at different indexes,
    // by index.
    assert_eq!(
        edited(
            "[1, 2]",
            &[
                array_insert("", 2, json!("c")),
                array_insert("", 0, json!("a")),
                array_insert("", 0, json!("b")),
            ]
        ),
        "[\"a\", \"b\", 1, 2, \"c\"]"
    );
    // Before an element that is removed: where it was.
    assert_eq!(
        edited("[1, 2, 3]", &[remove("/1"), array_insert("", 1, json!(9))]),
        "[1, 9, 3]"
    );
    assert_eq!(
        edited("[1, 2, 3]", &[remove("/0"), array_insert("", 0, json!(9))]),
        "[9, 2, 3]"
    );
}

/// The product invariant: an edit leaves every byte it does not own exactly
/// as written -- number spellings, escapes, normalization forms, offsets.
#[test]
fn test8416_untouched_values_keep_their_bytes() {
    let raw = "{\n  \"price\": 1.50,\n  \"big\": 12345678901234567890123,\n  \"exp\": 1E2,\n  \"neg\": -0,\n  \"esc\": \"caf\\u00e9 \\\"q\\\"\",\n  \"nfd\": \"cafe\u{301}\",\n  \"when\": \"2024-01-01T10:00:00+05:30\",\n  \"n\": 1\n}";
    let expected = raw.replace("\"n\": 1", "\"n\": 2");
    assert_eq!(edited(raw, &[replace("/n", json!(2))]), expected);
    let document = Document::parse(raw.as_bytes()).unwrap();
    let mut after = document.value().clone();
    after["n"] = json!(2);
    assert_eq!(
        String::from_utf8(edit_document(raw.as_bytes(), &after).unwrap()).unwrap(),
        expected
    );
    // The value, written out and read again by any JSON library, is still no
    // change at all.
    let round_tripped: Value = serde_json::from_str(&document.value().to_string()).unwrap();
    assert_eq!(
        edit_document(raw.as_bytes(), &round_tripped).unwrap(),
        raw.as_bytes()
    );
}

/// Edits that cannot all be honoured are refused, each with its own error.
#[test]
fn test8417_illegal_or_conflicting_edits_are_refused() {
    let raw = "{\"a\": {\"b\": 1}, \"c\": [1, 2], \"d\": 3}";
    let conflict = |edits: &[Edit]| {
        assert!(
            matches!(refused(raw, edits), EditError::Conflict(_)),
            "{edits:?} must conflict"
        )
    };
    conflict(&[replace("/d", json!(1)), remove("/d")]);
    conflict(&[remove("/d"), remove("/d")]);
    conflict(&[replace("/a", json!(1)), replace("/a/b", json!(2))]);
    conflict(&[remove("/a"), insert("/a", "x", json!(1), None)]);
    conflict(&[replace("/c", json!([])), array_insert("/c", 0, json!(1))]);
    conflict(&[remove("/a"), insert("", "x", json!(1), Some("a"))]);
    conflict(&[
        insert("", "x", json!(1), None),
        insert("", "x", json!(2), Some("a")),
    ]);
    assert!(matches!(
        refused(raw, &[insert("", "d", json!(1), None)]),
        EditError::Exists { name, .. } if name == "d"
    ));
    assert!(matches!(
        refused(raw, &[insert("", "x", json!(1), Some("nope"))]),
        EditError::Missing { pointer } if pointer == "/nope"
    ));
    assert!(matches!(
        refused(raw, &[remove("")]),
        EditError::RootRemoval
    ));
    assert!(matches!(
        refused(raw, &[insert("/c", "x", json!(1), None)]),
        EditError::Kind {
            expected: "an object",
            found: "an array",
            ..
        }
    ));
    assert!(matches!(
        refused(raw, &[array_insert("/a", 0, json!(1))]),
        EditError::Kind {
            expected: "an array",
            found: "an object",
            ..
        }
    ));
    assert!(matches!(
        refused(raw, &[array_insert("/c", 3, json!(1))]),
        EditError::Index {
            index: 3,
            len: 2,
            ..
        }
    ));
    assert!(matches!(
        refused(raw, &[replace("/d", value("[1e999]"))]),
        EditError::NonFinite(lexeme) if lexeme.contains("999")
    ));
    assert!(matches!(
        refused(raw, &[remove("/zz")]),
        EditError::Missing { .. }
    ));
    assert!(matches!(refused("{", &[]), EditError::Invalid(_)));
    // Replacing a member that is removed and re-inserted elsewhere is legal:
    // the name is free once its member goes.
    assert_eq!(
        edited(raw, &[remove("/d"), insert("", "d", json!(4), Some("a"))]),
        "{\"a\": {\"b\": 1}, \"d\": 4, \"c\": [1, 2]}"
    );
}

/// Edits are resolved against the document as it stands, so their order in
/// the list changes nothing.
#[test]
fn test8418_edits_apply_in_any_order() {
    let raw = "{\n  \"a\": 1,\n  \"b\": 2,\n  \"arr\": [\n    1,\n    2\n  ],\n  \"z\": true\n}\n";
    let edits = [
        replace("/a", json!("one")),
        remove("/b"),
        insert("", "c", json!(3), Some("a")),
        array_insert("/arr", 1, json!(1.5)),
        remove("/z"),
        insert("", "end", json!(null), None),
    ];
    let expected = "{\n  \"a\": \"one\",\n  \"c\": 3,\n  \"arr\": [\n    1,\n    1.5,\n    2\n  ],\n  \"end\": null\n}\n";
    let mut order: Vec<usize> = (0..edits.len()).collect();
    for rotation in 0..edits.len() {
        order.rotate_left(1);
        let mut shuffled: Vec<Edit> = order.iter().map(|index| edits[*index].clone()).collect();
        if rotation % 2 == 1 {
            shuffled.reverse();
        }
        assert_eq!(edited(raw, &shuffled), expected, "{shuffled:?}");
    }
}

/// When every child goes and new ones come, nothing of the old content is
/// left to sit beside, and the container is written afresh in its old shape.
#[test]
fn test8419_a_container_emptied_and_refilled_is_written_afresh() {
    let raw = "{\n  \"x\": {\n    \"a\": 1\n  },\n  \"y\": [1, 2]\n}";
    assert_eq!(
        edited(
            raw,
            &[remove("/x/a"), insert("/x", "b", json!({"c": 2}), None)]
        ),
        "{\n  \"x\": {\n    \"b\": {\n      \"c\": 2\n    }\n  },\n  \"y\": [1, 2]\n}"
    );
    assert_eq!(
        edited(
            raw,
            &[
                remove("/y/0"),
                remove("/y/1"),
                array_insert("/y", 2, json!(3))
            ]
        ),
        "{\n  \"x\": {\n    \"a\": 1\n  },\n  \"y\": [3]\n}"
    );
}

/// The edits a diff finds are as small as the value's change, and compare
/// numbers and strings by spelling.
#[test]
fn test8420_diff_is_minimal_and_spelling_aware() {
    assert_eq!(
        diff(&value(r#"{"a": 3}"#), &value(r#"{"a": 3.0}"#)),
        [replace("/a", number("3.0"))]
    );
    assert_eq!(
        diff(&json!({"s": "café"}), &json!({"s": "cafe\u{301}"})),
        [replace("/s", json!("cafe\u{301}"))]
    );
    assert_eq!(
        diff(&value(r#"{"a": 1, "b": 2}"#), &value(r#"{"b": 2, "a": 1}"#)),
        []
    );
    assert_eq!(
        diff(
            &value(r#"{"a": 1, "b": 2}"#),
            &value(r#"{"x": 0, "a": 1, "y": 2, "z": 3, "b": 2, "w": 4}"#)
        ),
        [
            insert("", "y", json!(2), Some("a")),
            insert("", "z", json!(3), Some("a")),
            insert("", "w", json!(4), Some("b")),
            insert("", "x", json!(0), None),
        ]
    );
    assert_eq!(
        diff(&json!({"a": [1, 2, 3, 4]}), &json!({"a": [1, 3]})),
        [remove("/a/1"), remove("/a/3")]
    );
    assert_eq!(
        diff(&json!([1, 2]), &json!([0, 1, 2, 3])),
        [array_insert("", 0, json!(0)), array_insert("", 2, json!(3))]
    );
    assert_eq!(
        diff(&json!([1, 2, 3]), &json!([3, 2, 1])),
        [replace("/2", json!(1)), replace("/0", json!(3))]
    );
    // Equally cheap ways to get there: a removal is preferred to a change.
    assert_eq!(
        diff(&json!([1, 2, 3]), &json!([4, 5])),
        [
            remove("/0"),
            replace("/2", json!(5)),
            replace("/1", json!(4))
        ]
    );
    // An element is edited into the new element it resembles, not into an
    // unrelated one: here the second array loses an element and the object
    // goes, rather than the array going and the object becoming an array.
    assert_eq!(
        diff(
            &json!([[null, null], [{"a": null}, null], {"a": null, "b": null}]),
            &json!([[null, null], [{"a": null}]])
        ),
        [remove("/2"), remove("/1/1")]
    );
    // A change mixing removal and addition keeps every element that stays,
    // and a changed element is changed only where it differs.
    assert_eq!(
        diff(&json!(["a", "b", "c", "d"]), &json!(["b", "c", "d", "e"])),
        [remove("/0"), array_insert("", 4, json!("e"))]
    );
    assert_eq!(
        diff(
            &json!([{"id": 1, "n": 1}, {"id": 2, "n": 2}]),
            &json!([{"id": 1, "n": 1}, {"id": 2, "n": 3}])
        ),
        [replace("/1/n", json!(3))]
    );
    assert_eq!(
        String::from_utf8(
            edit_document(
                b"[ 1 ,\t2 ,  {\"x\" : 1.0} ]",
                &value(r#"[2, {"x": 1.0}, 4]"#)
            )
            .unwrap()
        )
        .unwrap(),
        "[ 2 ,  {\"x\" : 1.0},  4 ]"
    );
    assert_eq!(
        diff(&json!({"a": [1]}), &json!({"a": {"0": 1}})),
        [replace("/a", json!({"0": 1}))]
    );
    assert_eq!(
        diff(&json!({"a/b": {"c~": 1}}), &json!({"a/b": {"c~": 2}})),
        [replace("/a~1b/c~0", json!(2))]
    );
}

/// A row edited end to end: one member changed, one added, one removed, and
/// the rest of the file exactly as it was. The new value lists its members in
/// another order, which moves nothing; the added member follows `note`, the
/// member it follows in the new value.
#[test]
fn test8421_a_row_is_edited_end_to_end() {
    let raw = "\u{feff}{\r\n    \"id\" : \"r1\",\r\n    \"price\" : 1.50,\r\n    \"note\" : \"caf\\u00e9\",\r\n    \"tags\" : [\r\n        \"a\",\r\n        \"b\"\r\n    ],\r\n    \"old\" : true\r\n}";
    let after = value(
        r#"{"tags": ["a", "b", "c"], "id": "r1", "price": 1.75, "note": "café", "added": {"k": []}}"#,
    );
    assert_eq!(
        String::from_utf8(edit_document(raw.as_bytes(), &after).unwrap()).unwrap(),
        "\u{feff}{\r\n    \"id\" : \"r1\",\r\n    \"price\" : 1.75,\r\n    \"note\" : \"caf\\u00e9\",\r\n    \"added\" : {\r\n        \"k\" : []\r\n    },\r\n    \"tags\" : [\r\n        \"a\",\r\n        \"b\",\r\n        \"c\"\r\n    ]\r\n}"
    );
}

/// No change, no write: the bytes come back identical, however unusual.
#[test]
fn test8422_an_unchanged_value_returns_the_same_bytes() {
    for raw in [
        "{ \"b\" :1 ,\"a\":[ 1.0 ,2e5 ] }",
        "\u{feff}[\r\n]",
        "{\n\t\"x\": \"\\u0041\"\n}\n\n\n",
        "  \"just a string\"",
    ] {
        let document = Document::parse(raw.as_bytes()).unwrap();
        assert_eq!(
            edit_document(raw.as_bytes(), document.value()).unwrap(),
            raw.as_bytes()
        );
        assert_eq!(apply(raw.as_bytes(), &[]).unwrap(), raw.as_bytes());
    }
    assert!(matches!(
        edit_document(b"{,}", &json!({})),
        Err(EditError::Invalid(_))
    ));
}

/// Every check the fuzz target makes passes on documents shaped like the ones
/// people write.
#[test]
fn test8423_the_fuzz_checks_hold_on_real_world_shapes() {
    for raw in [
        "{\n  \"id\": \"a\",\n  \"n\": [1, 2, {\"x\": null}],\n  \"o\": {}\n}\n",
        "\u{feff}{\r\n\t\"a\": [\r\n\t\t1.50,\r\n\t\t-0\r\n\t],\r\n\t\"b\": { }\r\n}",
        "{\"a\":{\"b\":{\"c\":[[],[1],{}]}},\"d\":\"e\\u0301\"}",
        "{ \"a\" : 1 , \"b\" : [ true , false ] }",
        "[]",
        "[[[]]]",
        "0",
        "{\"\":{\"\":[\"\"]}}",
        "[1e999]",
        "{\"a\":1,\"a\":2}",
    ] {
        fuzz_roundtrip(raw.as_bytes());
    }
}

/// Deep documents are handled without recursion in this module: rendering a
/// value thousands of levels deep, and editing at the bottom of one.
#[test]
fn test8424_depth_does_not_exhaust_the_stack() {
    let depth = 5000;
    let mut deep = json!(1);
    for _ in 0..depth {
        deep = Value::Array(vec![deep]);
    }
    // One line: indented, a document this deep is tens of megabytes of
    // indentation.
    let compact = Style::default_for_new_table().single_line();
    let rendered = render(&deep, &compact, 0);
    assert_eq!(rendered.iter().filter(|byte| **byte == b'[').count(), depth);
    assert_eq!(first_difference(&deep, &deep), None);
    // Reading the document back goes through the strict parser, which
    // recurses; give it the stack its depth needs.
    std::thread::Builder::new()
        .stack_size(512 << 20)
        .spawn(move || {
            crate::json::with_depth_limit(depth + 8, || {
                let mut pointer = "/0".repeat(depth);
                let edited = apply(&rendered, &[replace(&pointer, json!(2))]).unwrap();
                let document = Document::parse(&edited).unwrap();
                assert_eq!(
                    document
                        .lookup(&pointer)
                        .map(|id| &edited[document.node(id).span.clone()])
                        .unwrap(),
                    b"2"
                );
                pointer.truncate(pointer.len() - 2);
                let error = apply(
                    &rendered,
                    &[replace(&pointer, json!([[[[[[[[[[1]]]]]]]]]]))],
                )
                .unwrap_err();
                assert!(matches!(error, EditError::TooDeep(_)), "{error}");
            });
        })
        .unwrap()
        .join()
        .unwrap();
}

// ---- properties ----

fn lexeme() -> impl Strategy<Value = Value> {
    prop::sample::select(vec![
        "0",
        "-0",
        "7",
        "1.50",
        "1e2",
        "-3.25E-7",
        "12345678901234567890123",
        "0.1",
    ])
    .prop_map(number)
}

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        prop::sample::select(vec![
            "",
            "café",
            "cafe\u{301}",
            "\"q\"",
            "back\\slash",
            "line\nbreak",
            "\u{1}",
            "tab\t",
            "/~",
            "😀",
        ])
        .prop_map(str::to_string),
        "[a-z é]{0,6}",
    ]
}

fn scalar() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        lexeme(),
        text().prop_map(Value::String),
    ]
}

fn member_name() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["a", "b", "c", "id", "é", "e\u{301}", "a/b", "~", ""])
        .prop_map(str::to_string)
}

fn any_value() -> impl Strategy<Value = Value> {
    scalar().prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::vec((member_name(), inner), 0..4)
                .prop_map(|members| Value::Object(members.into_iter().collect())),
        ]
    })
}

fn row() -> impl Strategy<Value = Value> {
    prop::collection::vec((member_name(), any_value()), 0..6)
        .prop_map(|members| Value::Object(members.into_iter().collect::<Map<_, _>>()))
}

fn style() -> impl Strategy<Value = Style> {
    (
        prop::sample::select(vec!["", "  ", "    ", "\t"]),
        prop::sample::select(vec![Colon::Tight, Colon::Spaced, Colon::Surrounded]),
        prop::sample::select(vec![Comma::Tight, Comma::Spaced]),
        prop::sample::select(vec![Newline::Lf, Newline::CrLf]),
        any::<(bool, bool, bool, bool)>(),
        prop::sample::select(vec![Empty::Tight, Empty::Spaced]),
    )
        .prop_map(
            |(indent, colon, comma, newline, (final_newline, bom, padding, inline), empty)| Style {
                indent: Indent::new(indent).unwrap(),
                colon,
                comma,
                newline,
                final_newline,
                bom,
                empty,
                padding,
                inline_scalar_arrays: inline && !indent.is_empty(),
            },
        )
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    Index(usize),
}

fn to_pointer(path: &[Token]) -> String {
    path.iter()
        .fold(String::new(), |pointer, token| match token {
            Token::Name(name) => format!("{pointer}/{}", crate::schema::path::escape_pointer(name)),
            Token::Index(index) => format!("{pointer}/{index}"),
        })
}

fn paths(value: &Value) -> Vec<Vec<Token>> {
    let mut out = vec![];
    let mut work = vec![(vec![], value)];
    while let Some((path, value)) = work.pop() {
        match value {
            Value::Object(members) => {
                for (name, child) in members {
                    let mut next = path.clone();
                    next.push(Token::Name(name.clone()));
                    work.push((next, child));
                }
            }
            Value::Array(elements) => {
                for (index, child) in elements.iter().enumerate() {
                    let mut next = path.clone();
                    next.push(Token::Index(index));
                    work.push((next, child));
                }
            }
            _ => {}
        }
        out.push(path);
    }
    out.sort_by_key(|path| to_pointer(path));
    out
}

fn at<'v>(value: &'v mut Value, path: &[Token]) -> &'v mut Value {
    path.iter().fold(value, |value, token| match token {
        Token::Name(name) => &mut value[name.as_str()],
        Token::Index(index) => &mut value[*index],
    })
}

#[derive(Debug, Clone)]
enum Mutation {
    Set(Value),
    Remove,
    Add(String, Value),
}

/// Pick non-nested targets and a mutation for each, and the value they make.
fn mutate(input: &Value, picks: &[(usize, u8, Value)]) -> (Value, Vec<(Vec<Token>, Mutation)>) {
    let all = paths(input);
    let mut chosen: Vec<(Vec<Token>, Mutation)> = vec![];
    for (number, (selector, kind, payload)) in picks.iter().enumerate() {
        let path = &all[selector % all.len()];
        if chosen
            .iter()
            .any(|(other, _)| other.starts_with(path) || path.starts_with(other))
        {
            continue;
        }
        let mut probe = input.clone();
        let current = at(&mut probe, path).clone();
        let container = current.is_object() || current.is_array();
        let mutation = match (path.is_empty(), container, kind % 3) {
            (true, _, _) | (false, true, 1) => {
                Mutation::Add(format!("new\u{20ac}{number}"), payload.clone())
            }
            (false, _, 0) => Mutation::Remove,
            _ if *payload != current => Mutation::Set(payload.clone()),
            _ => continue,
        };
        chosen.push((path.clone(), mutation));
    }
    let mut target = input.clone();
    for (path, mutation) in &chosen {
        match mutation {
            Mutation::Set(value) => *at(&mut target, path) = value.clone(),
            Mutation::Add(name, value) => match at(&mut target, path) {
                Value::Object(members) => {
                    members.insert(name.clone(), value.clone());
                }
                Value::Array(elements) => elements.push(value.clone()),
                _ => unreachable!("only containers grow"),
            },
            Mutation::Remove => {}
        }
    }
    let mut removals: Vec<&Vec<Token>> = chosen
        .iter()
        .filter(|(_, mutation)| matches!(mutation, Mutation::Remove))
        .map(|(path, _)| path)
        .collect();
    // Deepest and highest-indexed first, so no removal moves another.
    removals.sort_by(|left, right| {
        let key = |path: &Vec<Token>| {
            path.iter()
                .map(|token| match token {
                    Token::Index(index) => (*index, String::new()),
                    Token::Name(name) => (0, name.clone()),
                })
                .collect::<Vec<_>>()
        };
        key(right).cmp(&key(left))
    });
    for path in removals {
        let (last, parent) = path.split_last().expect("the root is never removed");
        match (at(&mut target, parent), last) {
            (Value::Object(members), Token::Name(name)) => {
                members.shift_remove(name);
            }
            (Value::Array(elements), Token::Index(index)) => {
                elements.remove(*index);
            }
            _ => unreachable!("a path names its parent's kind"),
        }
    }
    (target, chosen)
}

/// Where a value of the input is in the output, once the removed array
/// elements before it are gone.
fn moved(path: &[Token], chosen: &[(Vec<Token>, Mutation)]) -> Vec<Token> {
    path.iter()
        .enumerate()
        .map(|(depth, token)| match token {
            Token::Index(index) => Token::Index(
                index
                    - chosen
                        .iter()
                        .filter(|(target, mutation)| {
                            matches!(mutation, Mutation::Remove)
                                && target.len() == depth + 1
                                && target[..depth] == path[..depth]
                                && matches!(target[depth], Token::Index(other) if other < *index)
                        })
                        .count(),
            ),
            name => name.clone(),
        })
        .collect()
}

/// Whitespace as a person might leave it: any of these between any two
/// tokens, chosen by the seeds.
const GAPS: [&str; 8] = ["", "", " ", "\n", "\n  ", "\t", "\r\n    ", "  "];

/// `value` written with irregular whitespace -- mixed line endings, indents
/// that follow no unit, spaces on either side of colons and commas -- and a
/// byte order mark when the first seed says so.
fn scattered(value: &Value, seeds: &[usize]) -> Vec<u8> {
    fn gap(out: &mut Vec<u8>, seeds: &mut impl Iterator<Item = usize>) {
        let seed = seeds.next().expect("the seeds cycle");
        out.extend_from_slice(GAPS[seed % GAPS.len()].as_bytes());
    }
    fn write(value: &Value, out: &mut Vec<u8>, seeds: &mut impl Iterator<Item = usize>) {
        match value {
            Value::Object(members) => {
                out.push(b'{');
                for (index, (name, value)) in members.iter().enumerate() {
                    if index > 0 {
                        gap(out, seeds);
                        out.push(b',');
                    }
                    gap(out, seeds);
                    out.extend_from_slice(&serde_json::to_vec(name).unwrap());
                    gap(out, seeds);
                    out.push(b':');
                    gap(out, seeds);
                    write(value, out, seeds);
                }
                gap(out, seeds);
                out.push(b'}');
            }
            Value::Array(elements) => {
                out.push(b'[');
                for (index, value) in elements.iter().enumerate() {
                    if index > 0 {
                        gap(out, seeds);
                        out.push(b',');
                    }
                    gap(out, seeds);
                    write(value, out, seeds);
                }
                gap(out, seeds);
                out.push(b']');
            }
            scalar => out.extend_from_slice(&serde_json::to_vec(scalar).unwrap()),
        }
    }
    let mut out = vec![];
    if seeds[0].is_multiple_of(5) {
        out.extend_from_slice(BOM);
    }
    let mut seeds = seeds.iter().copied().cycle();
    gap(&mut out, &mut seeds);
    write(value, &mut out, &mut seeds);
    gap(&mut out, &mut seeds);
    out
}

/// Whether the value at `path` lies in an array element equal to another
/// element of its array, before or after the change. Which of two equal
/// elements a diff of values keeps -- or rewrites into the other -- is not
/// determined by the values, so where they are spelled differently there is
/// no one right text to expect at a given index.
fn interchangeable(
    input: &Value,
    target: &Value,
    path: &[Token],
    chosen: &[(Vec<Token>, Mutation)],
) -> bool {
    let get = |value: &'_ Value, path: &[Token]| -> Option<Value> {
        let mut at = value;
        for token in path {
            at = match token {
                Token::Name(name) => at.get(name.as_str())?,
                Token::Index(index) => at.get(*index)?,
            };
        }
        Some(at.clone())
    };
    let repeated = |array: Option<Value>, element: &Value| {
        array
            .as_ref()
            .and_then(Value::as_array)
            .is_some_and(|elements| elements.iter().filter(|other| *other == element).count() > 1)
    };
    (0..path.len()).any(|depth| {
        if !matches!(path[depth], Token::Index(_)) {
            return false;
        }
        let element = get(input, &path[..=depth]).expect("the path is in the input");
        repeated(get(input, &path[..depth]), &element)
            || repeated(get(target, &moved(&path[..depth], chosen)), &element)
    })
}

/// Whether the value at `path` lies in an array that more than one mutation
/// changed. Several changes to one array can be aligned more than one equally
/// cheap way -- the diff is free to pick any -- so which element ends up at a
/// given index is only determined where an array changed once. The values are
/// still compared everywhere, and a mixed change is checked byte for byte by
/// [`test8420_diff_is_minimal_and_spelling_aware`].
fn changed_more_than_once(
    input: &Value,
    path: &[Token],
    chosen: &[(Vec<Token>, Mutation)],
) -> bool {
    let mut at = input;
    for depth in 0..path.len() {
        if at.is_array() {
            let inside = chosen
                .iter()
                .filter(|(target, _)| target.len() > depth && target.starts_with(&path[..depth]))
                .count();
            if inside > 1 {
                return true;
            }
        }
        at = match &path[depth] {
            Token::Name(name) => &at[name.as_str()],
            Token::Index(index) => &at[*index],
        };
    }
    false
}

/// Change the document in `raw`, holding `input`, by `picks`, and check that
/// the result holds the new value and that every value no mutation touched
/// keeps its exact text. Returns the new bytes.
fn check_edit(
    raw: &[u8],
    input: &Value,
    picks: &[(usize, u8, Value)],
) -> Result<Vec<u8>, TestCaseError> {
    let (target, chosen) = mutate(input, picks);
    let output = edit_document(raw, &target)
        .unwrap_or_else(|error| panic!("{error}\n{}", String::from_utf8_lossy(raw)));
    let reread = Document::parse(&output).unwrap();
    // The new value. Member order is the file's where the file has an
    // opinion: a new member that precedes every existing one in the target
    // goes at the end, so the comparison is of values.
    prop_assert_eq!(reread.value(), &target);

    // Every value no mutation touched keeps its text.
    let original = Document::parse(raw).unwrap();
    for path in paths(input) {
        let touched = chosen.iter().any(|(target, mutation)| {
            target.starts_with(&path)
                || (path.starts_with(target) && !matches!(mutation, Mutation::Add(..)))
        });
        if touched
            || interchangeable(input, &target, &path, &chosen)
            || changed_more_than_once(input, &path, &chosen)
        {
            continue;
        }
        let old = original
            .node(original.lookup(&to_pointer(&path)).unwrap())
            .span
            .clone();
        let new_pointer = to_pointer(&moved(&path, &chosen));
        let new = reread
            .node(reread.lookup(&new_pointer).unwrap())
            .span
            .clone();
        prop_assert_eq!(
            String::from_utf8_lossy(&raw[old]),
            String::from_utf8_lossy(&output[new]),
            "{} moved to {}",
            to_pointer(&path),
            new_pointer
        );
    }
    Ok(output)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, .. ProptestConfig::default() })]

    /// Arbitrary bytes never break the editor, and whatever parses passes
    /// every fuzz check.
    #[test]
    fn test8425_arbitrary_bytes_pass_the_fuzz_checks(data in prop::collection::vec(any::<u8>(), 0..64)) {
        fuzz_roundtrip(&data);
    }

    /// Arbitrary documents, in arbitrary styles and in no style at all, pass
    /// every fuzz check.
    #[test]
    fn test8426_generated_documents_pass_the_fuzz_checks(
        input in row(),
        style in style(),
        seeds in prop::collection::vec(any::<usize>(), 1..48),
    ) {
        fuzz_roundtrip(&render_document(&input, &style));
        fuzz_roundtrip(&scattered(&input, &seeds));
    }

    /// A document in any style, changed to any nearby value: the result holds
    /// the new value, every value the change did not touch keeps its exact
    /// text, and the file's style is unchanged wherever both files show it.
    ///
    /// "Both files show it" is the strongest form the style property can
    /// take: an edit that removes the only empty container, say, leaves a file
    /// that no longer says how empty containers are written, and its detected
    /// style falls back there. Comparing only observed properties keeps the
    /// property true and still catches any edit written in a foreign style.
    #[test]
    fn test8427_an_edit_preserves_everything_it_does_not_change(
        input in row(),
        style in style(),
        picks in prop::collection::vec((any::<usize>(), any::<u8>(), any_value()), 0..5),
    ) {
        let raw = render_document(&input, &style);
        let before = Style::detect_detailed(&raw);
        // Detection recovers every property the file shows.
        let shown = before.evidence;
        prop_assert_eq!(before.style.bom, style.bom);
        prop_assert_eq!(before.style.final_newline, style.final_newline);
        if shown.indent > 0 { prop_assert_eq!(&before.style.indent, &style.indent); }
        if shown.colon > 0 { prop_assert_eq!(before.style.colon, style.colon); }
        if shown.comma > 0 { prop_assert_eq!(before.style.comma, style.comma); }
        if shown.newline > 0 { prop_assert_eq!(before.style.newline, style.newline); }
        if shown.empty > 0 { prop_assert_eq!(before.style.empty, style.empty); }
        if shown.padding > 0 { prop_assert_eq!(before.style.padding, style.padding); }
        if shown.inline_scalar_arrays > 0 {
            prop_assert_eq!(before.style.inline_scalar_arrays, style.inline_scalar_arrays);
        }

        let output = check_edit(&raw, &input, &picks)?;

        // The style is the same wherever both files show it.
        let after = Style::detect_detailed(&output);
        let both = |left: usize, right: usize| left > 0 && right > 0;
        prop_assert_eq!(after.style.bom, before.style.bom);
        prop_assert_eq!(after.style.final_newline, before.style.final_newline);
        if both(before.evidence.indent, after.evidence.indent) {
            prop_assert_eq!(&after.style.indent, &before.style.indent);
        }
        if both(before.evidence.colon, after.evidence.colon) {
            prop_assert_eq!(after.style.colon, before.style.colon);
        }
        if both(before.evidence.comma, after.evidence.comma) {
            prop_assert_eq!(after.style.comma, before.style.comma);
        }
        if both(before.evidence.newline, after.evidence.newline) {
            prop_assert_eq!(after.style.newline, before.style.newline);
        }
        if both(before.evidence.empty, after.evidence.empty) {
            prop_assert_eq!(after.style.empty, before.style.empty);
        }
        if both(before.evidence.padding, after.evidence.padding) {
            prop_assert_eq!(after.style.padding, before.style.padding);
        }
        // Whether arrays of scalars are written on one line is not compared:
        // an array written over lines because it held a container still is
        // after its containers are removed, now as an array of scalars. The
        // edit must not reflow the elements it keeps, so that layout is the
        // file's, not the edit's.
    }

    /// The same for a file in no consistent style at all -- whitespace
    /// scattered between tokens, mixed line endings -- where keeping every
    /// untouched byte matters most and copying a style is hardest.
    #[test]
    fn test8428_an_edit_to_an_irregular_file_preserves_everything_it_does_not_change(
        input in row(),
        seeds in prop::collection::vec(any::<usize>(), 1..48),
        picks in prop::collection::vec((any::<usize>(), any::<u8>(), any_value()), 0..5),
    ) {
        check_edit(&scattered(&input, &seeds), &input, &picks)?;
    }
}
