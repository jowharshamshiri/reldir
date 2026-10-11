//! Widening a schema document so it admits values it refuses.
//!
//! A table's contract only ever widens because someone said so: `--widen` on
//! a change, `reldir schema widen`, or doctor's `FIX_WIDEN_CONTRACT`. Each of
//! those asks this module for the least edit that admits the rows in
//! question, made where the refusing rule is written:
//!
//! | The rule refused | The widening |
//! |---|---|
//! | `type` | adds the value's type (and drops a type tag the union outgrows) |
//! | `required` | makes the member optional |
//! | `additionalProperties: false` | declares the member, shaped as its value |
//! | a `false` subschema | admits the value's shape there |
//! | `enum`, `const` | adds the value |
//! | `minimum`, `maxLength`, ... | moves the bound to the value |
//! | `format`, `pattern`, `multipleOf`, `uniqueItems` | drops the keyword |
//! | a type tag, or `contentEncoding` | drops the lexical rule |
//!
//! A rule written under `$defs` is shared by every `$ref` to it, and `anyOf`, `oneOf`, `not` and the like refuse as a whole:
//! no edit of them is the least one. Those are refused with the reason, for a
//! person to decide -- widening never guesses.

use super::{
    Schema,
    path::pointer_tokens,
    row::{Failure, Refused},
};
use crate::shape::Shape;
use serde_json::{Map, Value, json};

/// One edit a widening made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Widened {
    /// Where in the schema document the edit was made.
    pub rule: String,
    /// What the edit did, as a sentence fragment: `adds "string" to type`.
    pub change: String,
}

impl std::fmt::Display for Widened {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let rule = if self.rule.is_empty() {
            "/"
        } else {
            &self.rule
        };
        write!(formatter, "{rule}: {}", self.change)
    }
}

/// How many rounds of edits a widening may take: each round can uncover a
/// rule the previous one hid (a member declared, then its bounds). Documents
/// that need more are not widened by rule.
const ROUNDS: usize = 16;

/// The document widened just enough that every row satisfies it, and each
/// edit made. Refused, with the reason, when a refusing rule cannot be
/// widened by an edit of its own.
pub fn admit(document: &Value, rows: &[Value]) -> Result<(Value, Vec<Widened>), String> {
    let mut document = document.clone();
    let mut made: Vec<Widened> = vec![];
    for _ in 0..ROUNDS {
        let schema = Schema::from_document(document.clone(), None).map_err(|problems| {
            format!(
                "the widened schema would not load: {}",
                problems
                    .iter()
                    .map(|problem| problem.message.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        })?;
        let failures: Vec<Failure> = rows
            .iter()
            .flat_map(|row| schema.validator().failures(row))
            .collect();
        if failures.is_empty() {
            return Ok((document, made));
        }
        let mut progressed = false;
        for failure in &failures {
            if let Some(widened) = edit(&mut document, failure)? {
                if !made.contains(&widened) {
                    made.push(widened);
                }
                progressed = true;
            }
        }
        if !progressed {
            let failure = &failures[0];
            return Err(format!(
                "the rule at {} still refuses the value at {} after widening",
                display(&failure.rule),
                display(&failure.at)
            ));
        }
    }
    Err(format!(
        "admitting the values takes more than {ROUNDS} rounds of widening; edit the schema"
    ))
}

fn display(pointer: &str) -> &str {
    if pointer.is_empty() { "/" } else { pointer }
}

/// The JSON Schema type a value has, as `type` names it.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(number) => {
            if number.to_string().contains(['.', 'e', 'E']) {
                "number"
            } else {
                "integer"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Make one edit that admits the failure's value, or `None` when an earlier
/// edit already did.
fn edit(document: &mut Value, failure: &Failure) -> Result<Option<Widened>, String> {
    let tokens = pointer_tokens(&failure.rule);
    if let Some(definitions) = tokens
        .iter()
        .find(|token| matches!(token.as_str(), "$defs" | "definitions"))
    {
        return Err(format!(
            "the rule at {} is a definition under `{definitions}`, which every `$ref` to it \
             shares; widen the definition by editing the schema",
            display(&failure.rule)
        ));
    }
    let widened = |change: String, rule: &str| Widened {
        rule: rule.to_string(),
        change,
    };
    if failure.refused == Refused::Nothing {
        let node = document
            .pointer_mut(&failure.rule)
            .ok_or_else(|| lost(&failure.rule))?;
        if node != &Value::Bool(false) {
            return Ok(None);
        }
        *node = Shape::observe(&failure.instance).to_subschema();
        return Ok(Some(widened(
            "admits the shape of the value it refused".into(),
            &failure.rule,
        )));
    }
    let (parent, keyword) = match failure.rule.rsplit_once('/') {
        Some((parent, keyword)) => (
            parent.to_string(),
            keyword.replace("~1", "/").replace("~0", "~"),
        ),
        None => return Err(lost(&failure.rule)),
    };
    let subschema = document
        .pointer_mut(&parent)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| lost(&parent))?;
    let instance = &failure.instance;
    let change = match (&failure.refused, keyword.as_str()) {
        (Refused::Required(member), _) => {
            let Some(Value::Array(required)) = subschema.get_mut("required") else {
                return Err(lost(&failure.rule));
            };
            let before = required.len();
            required.retain(|name| name.as_str() != Some(member));
            if required.len() == before {
                return Ok(None);
            }
            if required.is_empty() {
                subschema.remove("required");
            }
            format!("makes {member:?} optional")
        }
        (Refused::Additional(members), _) => {
            let properties = subschema
                .entry("properties")
                .or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut()
                .ok_or_else(|| lost(&parent))?;
            let mut declared = vec![];
            for member in members {
                if properties.contains_key(member) {
                    continue;
                }
                let value = instance.get(member).unwrap_or(&Value::Null);
                properties.insert(member.clone(), Shape::observe(value).to_subschema());
                declared.push(format!("{member:?}"));
            }
            if declared.is_empty() {
                return Ok(None);
            }
            format!("declares {}", declared.join(", "))
        }
        (Refused::Lexical, tag) => {
            if subschema.remove(tag).is_none() {
                return Ok(None);
            }
            format!("drops `{tag}`, which refused how the value is written")
        }
        (_, "type") => {
            let name = type_name(instance);
            let mut names: Vec<String> = match subschema.get("type") {
                Some(Value::String(one)) => vec![one.clone()],
                Some(Value::Array(many)) => many
                    .iter()
                    .filter_map(|name| name.as_str().map(String::from))
                    .collect(),
                _ => return Err(lost(&failure.rule)),
            };
            let covered = names
                .iter()
                .any(|known| known == name || (known == "number" && name == "integer"));
            if covered {
                return Ok(None);
            }
            names.push(name.to_string());
            subschema.insert("type".into(), json!(names));
            let mut change = format!("adds {name:?} to `type`");
            // A type tag says how one type is written; a union with another
            // non-null type has outgrown it.
            if name != "null"
                && let Some(tag) = subschema.remove(super::meta::TYPE_TAG)
            {
                change.push_str(&format!(", and drops the type tag {tag}"));
            }
            change
        }
        (_, "enum") => {
            let Some(Value::Array(values)) = subschema.get_mut("enum") else {
                return Err(lost(&failure.rule));
            };
            if values.contains(instance) {
                return Ok(None);
            }
            values.push(instance.clone());
            format!("adds {} to `enum`", crate::canonical::compact(instance))
        }
        (_, "const") => {
            let Some(constant) = subschema.remove("const") else {
                return Ok(None);
            };
            subschema.insert("enum".into(), json!([constant, instance]));
            format!(
                "turns `const` into an `enum` that also admits {}",
                crate::canonical::compact(instance)
            )
        }
        (_, bound @ ("minimum" | "maximum")) => {
            let Value::Number(_) = instance else {
                return Err(lost(&failure.rule));
            };
            subschema.insert(bound.to_string(), instance.clone());
            format!("moves `{bound}` to {instance}")
        }
        (_, bound @ ("exclusiveMinimum" | "exclusiveMaximum")) => {
            let Value::Number(number) = instance else {
                return Err(lost(&failure.rule));
            };
            subschema.remove(bound);
            let inclusive = if bound == "exclusiveMinimum" {
                "minimum"
            } else {
                "maximum"
            };
            let loosest = match subschema.get(inclusive).and_then(Value::as_number) {
                Some(existing) => {
                    let ordering =
                        crate::value::compare_numeric(&existing.to_string(), &number.to_string());
                    let keep_existing = matches!(
                        (inclusive, ordering),
                        ("minimum", Some(std::cmp::Ordering::Less))
                            | ("maximum", Some(std::cmp::Ordering::Greater))
                    );
                    if keep_existing {
                        Value::Number(existing.clone())
                    } else {
                        instance.clone()
                    }
                }
                None => instance.clone(),
            };
            subschema.insert(inclusive.into(), loosest.clone());
            format!("replaces `{bound}` with `{inclusive}` {loosest}")
        }
        (
            _,
            bound @ ("minLength" | "maxLength" | "minItems" | "maxItems" | "minProperties"
            | "maxProperties"),
        ) => {
            let size = match instance {
                Value::String(text) => text.chars().count(),
                Value::Array(items) => items.len(),
                Value::Object(members) => members.len(),
                _ => return Err(lost(&failure.rule)),
            };
            subschema.insert(bound.to_string(), json!(size));
            format!("moves `{bound}` to {size}")
        }
        (_, dropped @ ("format" | "pattern" | "multipleOf" | "uniqueItems")) => {
            let Some(rule) = subschema.remove(dropped) else {
                return Ok(None);
            };
            format!("drops `{dropped}` {}", crate::canonical::compact(&rule))
        }
        (_, other) => {
            return Err(format!(
                "the rule at {} (`{other}`) refuses the value at {} as a whole, and no edit of it \
                 alone is the least widening; edit the schema to say what it should admit",
                display(&failure.rule),
                display(&failure.at)
            ));
        }
    };
    Ok(Some(widened(change, &parent)))
}

/// A widening a person states for one member, rather than one computed from
/// values: `reldir schema widen users.nick --null`.
#[derive(Debug, Clone, Default)]
pub struct Stated {
    /// Admit null.
    pub null: bool,
    /// Let a row leave the member out.
    pub optional: bool,
    /// Admit these JSON types.
    pub types: Vec<String>,
    /// Admit any value at all.
    pub any: bool,
}

const JSON_TYPES: [&str; 7] = [
    "null", "boolean", "integer", "number", "string", "array", "object",
];

/// The document widened as stated at a member path. The path is a reference
/// path through `properties` and `items` (`tags[]`, `address.city`); a member
/// the document does not declare that way is not guessed at.
pub fn stated(
    document: &Value,
    path: &super::RefPath,
    widening: &Stated,
) -> Result<(Value, Vec<Widened>), String> {
    use super::path::{Step, escape_pointer};
    if widening.any && (widening.null || !widening.types.is_empty()) {
        return Err("--any admits every value already; it takes no --null or --type".into());
    }
    if let Some(unknown) = widening
        .types
        .iter()
        .find(|name| !JSON_TYPES.contains(&name.as_str()))
    {
        return Err(format!(
            "{unknown:?} is not a JSON type; the types are {}",
            JSON_TYPES.join(", ")
        ));
    }
    let mut document = document.clone();
    let mut pointer = String::new();
    let mut parent: Option<(String, String)> = None;
    for step in path.steps() {
        let node = document.pointer(&pointer).ok_or_else(|| lost(&pointer))?;
        match step {
            Step::Member(name) => {
                if node
                    .pointer(&format!("/properties/{}", escape_pointer(name)))
                    .is_none()
                {
                    return Err(format!(
                        "the schema declares no member {name:?} under `properties` at {}",
                        display(&pointer)
                    ));
                }
                parent = Some((pointer.clone(), name.clone()));
                pointer = format!("{pointer}/properties/{}", escape_pointer(name));
            }
            Step::Each => {
                if !node.get("items").is_some_and(Value::is_object) {
                    return Err(format!(
                        "the schema has no `items` subschema at {} to widen",
                        display(&pointer)
                    ));
                }
                parent = None;
                pointer = format!("{pointer}/items");
            }
            Step::Where { .. } => {
                return Err(
                    "a member path names members and `[]` for every element; a filter selects \
                     values, not a place in the schema"
                        .into(),
                );
            }
        }
    }
    let mut made = vec![];
    if widening.optional {
        let Some((at, member)) = &parent else {
            return Err(
                "--optional makes an object member optional, and the path ends at array elements"
                    .into(),
            );
        };
        let object = document
            .pointer_mut(at)
            .and_then(Value::as_object_mut)
            .ok_or_else(|| lost(at))?;
        if let Some(Value::Array(required)) = object.get_mut("required") {
            let before = required.len();
            required.retain(|name| name.as_str() != Some(member));
            if required.len() != before {
                if required.is_empty() {
                    object.remove("required");
                }
                made.push(Widened {
                    rule: at.clone(),
                    change: format!("makes {member:?} optional"),
                });
            }
        }
    }
    let node = document
        .pointer_mut(&pointer)
        .ok_or_else(|| lost(&pointer))?;
    if widening.any {
        if node != &json!({}) {
            *node = json!({});
            made.push(Widened {
                rule: pointer.clone(),
                change: "admits any value".into(),
            });
        }
        return Ok((document, made));
    }
    let mut wanted: Vec<&str> = widening.types.iter().map(String::as_str).collect();
    if widening.null {
        wanted.push("null");
    }
    if !wanted.is_empty() {
        let written = node.to_string();
        let subschema = node.as_object_mut().ok_or_else(|| {
            format!(
                "the subschema at {} is `{written}`; widen it with --any",
                display(&pointer)
            )
        })?;
        let mut names: Vec<String> = match subschema.get("type") {
            Some(Value::String(one)) => vec![one.clone()],
            Some(Value::Array(many)) => many
                .iter()
                .filter_map(|name| name.as_str().map(String::from))
                .collect(),
            _ => {
                return Err(format!(
                    "the subschema at {} states no `type` to add to; it admits what its other \
                     rules admit -- edit the schema, or widen it with --any",
                    display(&pointer)
                ));
            }
        };
        for name in wanted {
            let covered = names
                .iter()
                .any(|known| known == name || (known == "number" && name == "integer"));
            if covered {
                continue;
            }
            names.push(name.to_string());
            let mut change = format!("adds {name:?} to `type`");
            if name != "null"
                && let Some(tag) = subschema.remove(super::meta::TYPE_TAG)
            {
                change.push_str(&format!(", and drops the type tag {tag}"));
            }
            made.push(Widened {
                rule: pointer.clone(),
                change,
            });
        }
        subschema.insert("type".into(), json!(names));
    }
    Ok((document, made))
}

fn lost(pointer: &str) -> String {
    format!(
        "the rule the validator reported at {} is not where the schema document says; widen it by \
         editing the schema",
        display(pointer)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(properties: Value, required: &[&str]) -> Value {
        json!({
            "$schema": crate::schema::meta::DIALECT_URI,
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
            "x-reldir": {"table": "t", "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}}}
        })
    }

    fn changes(made: &[Widened]) -> Vec<String> {
        made.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn test8930_each_refusing_rule_is_widened_where_it_is_written_and_no_further() {
        let document = table(
            json!({
                "id": {"type": "string"},
                "n": {"type": "integer", "x-reldir-type": "int", "minimum": 0},
                "state": {"type": "string", "enum": ["open", "closed"]},
                "tags": {"type": "array", "items": false},
                "note": {"type": "string", "maxLength": 3}
            }),
            &["id", "n", "note"],
        );
        let rows = vec![
            json!({"id": "a", "n": -2, "state": "held", "tags": ["x"], "note": "long note", "extra": {"k": 1}}),
            json!({"id": "b", "n": null, "note": "ok"}),
            json!({"id": "c", "n": 1}),
        ];
        let (widened, made) = admit(&document, &rows).unwrap();
        let schema = Schema::from_document(widened.clone(), None).unwrap();
        for row in &rows {
            assert!(
                schema.validator().failures(row).is_empty(),
                "{row} still refused"
            );
        }
        let made = changes(&made);
        for expected in [
            "/properties/n: moves `minimum` to -2",
            "/properties/state: adds \"held\" to `enum`",
            "/properties/tags/items: admits the shape of the value it refused",
            "/properties/note: moves `maxLength` to 9",
            "/: declares \"extra\"",
            "/properties/n: adds \"null\" to `type`",
            "/: makes \"note\" optional",
        ] {
            assert!(
                made.contains(&expected.to_string()),
                "{expected} is missing from {made:#?}"
            );
        }
        assert_eq!(
            widened["properties"]["n"]["x-reldir-type"], "int",
            "admitting null does not outgrow the type tag"
        );
        assert_eq!(widened["required"], json!(["id", "n"]));
        assert_eq!(
            widened["properties"]["extra"],
            Shape::observe(&json!({"k": 1})).to_subschema()
        );
        // The widening is the least: a value the original refused for another
        // reason is still refused.
        assert!(
            !schema
                .validator()
                .failures(&json!({"id": "d", "n": 1, "state": "lost"}))
                .is_empty()
        );
    }

    #[test]
    fn test8931_a_union_outgrows_its_type_tag_and_a_lexical_rule_is_dropped() {
        let document = table(
            json!({
                "id": {"type": "string"},
                "n": {"type": "integer", "x-reldir-type": "int"},
                "f": {"type": "number", "x-reldir-type": "float"}
            }),
            &["id"],
        );
        let (widened, made) =
            admit(&document, &[json!({"id": "a", "n": "seven", "f": 3})]).unwrap();
        assert_eq!(
            widened["properties"]["n"],
            json!({"type": ["integer", "string"]})
        );
        assert_eq!(widened["properties"]["f"], json!({"type": "number"}));
        assert_eq!(made.len(), 2, "{made:#?}");
    }

    #[test]
    fn test8932_shared_and_whole_rules_are_refused_with_the_reason() {
        let mut shared = table(
            json!({"id": {"type": "string"}, "a": {"$ref": "#/$defs/small"}}),
            &["id"],
        );
        shared["$defs"] = json!({"small": {"type": "integer", "maximum": 3}});
        let refusal = admit(&shared, &[json!({"id": "x", "a": 9})]).unwrap_err();
        assert!(refusal.contains("$ref"), "{refusal}");

        let choice = table(
            json!({"id": {"type": "string"}, "a": {"anyOf": [{"type": "integer"}, {"type": "boolean"}]}}),
            &["id"],
        );
        let refusal = admit(&choice, &[json!({"id": "x", "a": "s"})]).unwrap_err();
        assert!(refusal.contains("anyOf"), "{refusal}");

        let keyed = table(json!({"id": {"type": "string"}}), &["id"]);
        let refusal = admit(&keyed, &[json!({})]).unwrap_err();
        assert!(
            refusal.contains("would not load"),
            "an identity column cannot become optional: {refusal}"
        );
    }

    #[test]
    fn test8934_a_stated_widening_edits_exactly_the_member_named() {
        let mut document = table(
            json!({
                "id": {"type": "string"},
                "n": {"type": "integer", "x-reldir-type": "int"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "address": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"], "additionalProperties": false}
            }),
            &["id", "n"],
        );
        document["required"] = json!(["id", "n"]);
        let path = |text: &str| super::super::RefPath::parse(text).unwrap();

        let (widened, made) = admit_stated(
            &document,
            "n",
            Stated {
                null: true,
                optional: true,
                ..Stated::default()
            },
        );
        assert_eq!(
            widened["properties"]["n"],
            json!({"type": ["integer", "null"], "x-reldir-type": "int"})
        );
        assert_eq!(widened["required"], json!(["id"]));
        assert_eq!(made.len(), 2);

        let (widened, _) = admit_stated(
            &document,
            "tags[]",
            Stated {
                types: vec!["integer".into()],
                ..Stated::default()
            },
        );
        assert_eq!(
            widened["properties"]["tags"]["items"],
            json!({"type": ["string", "integer"]})
        );

        let (widened, _) = admit_stated(
            &document,
            "address.city",
            Stated {
                optional: true,
                ..Stated::default()
            },
        );
        assert!(widened["properties"]["address"].get("required").is_none());

        let (widened, made) = admit_stated(
            &document,
            "n",
            Stated {
                types: vec!["string".into()],
                ..Stated::default()
            },
        );
        assert_eq!(
            widened["properties"]["n"],
            json!({"type": ["integer", "string"]})
        );
        assert!(made[0].change.contains("drops the type tag"), "{made:?}");

        let (_, made) = admit_stated(
            &document,
            "id",
            Stated {
                types: vec!["string".into()],
                ..Stated::default()
            },
        );
        assert!(
            made.is_empty(),
            "what is admitted already needs no widening"
        );

        assert!(
            stated(
                &document,
                &path("ghost"),
                &Stated {
                    null: true,
                    ..Stated::default()
                }
            )
            .is_err()
        );
        assert!(
            stated(
                &document,
                &path("n"),
                &Stated {
                    types: vec!["text".into()],
                    ..Stated::default()
                }
            )
            .is_err()
        );
        assert!(
            stated(
                &document,
                &path("tags[]"),
                &Stated {
                    optional: true,
                    ..Stated::default()
                }
            )
            .is_err()
        );
    }

    fn admit_stated(document: &Value, path: &str, widening: Stated) -> (Value, Vec<Widened>) {
        stated(
            document,
            &super::super::RefPath::parse(path).unwrap(),
            &widening,
        )
        .unwrap()
    }

    #[test]
    fn test8933_rows_the_document_admits_need_no_widening() {
        let document = table(json!({"id": {"type": "string"}}), &["id"]);
        let (widened, made) = admit(&document, &[json!({"id": "a"})]).unwrap();
        assert_eq!(widened, document);
        assert!(made.is_empty());
    }
}
