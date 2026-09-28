//! How a valid database's schemas could say more than they do.
//!
//! Lint never changes anything. Each finding carries the evidence, a stable
//! code, and -- where one exists -- its *remedy*: the exact edit that would
//! apply it, computed here once. `doctor` applies remedies; it never derives a
//! second opinion of what a finding means.

use crate::{
    analysis::references::{self, ProposedTarget, TableFacts},
    catalog::Catalog,
    config::Config,
    diagnostic::{Diagnostic, Result},
    schema::{
        AdditionalFields, ColumnType, GeneratedKind, Schema,
        document::{enum_subschema, subschema},
        path::escape_pointer,
    },
};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

/// A schema edit a finding proposes.
#[derive(Debug, Clone)]
pub enum Remedy {
    /// Replace the documents of these tables with the edited ones.
    Edit(BTreeMap<String, Value>),
    /// Move a working schema to `schema/`, making it the table's declaration.
    Pin(String),
    /// Rewrite these row files in canonical form.
    Canonicalize(Vec<std::path::PathBuf>),
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub diagnostic: Diagnostic,
    pub remedy: Option<Remedy>,
}

struct Lint<'c> {
    catalog: &'c Catalog,
    out: Vec<Finding>,
}

impl Lint<'_> {
    /// A finding about a table's schema, located at `pointer` inside its file.
    fn schema(
        &mut self,
        table: &str,
        pointer: &str,
        diagnostic: Diagnostic,
        remedy: Option<Remedy>,
    ) {
        let mut diagnostic = diagnostic.table(table).pointer(pointer);
        if let Some(file) = self.catalog.schema_files.get(table) {
            let spans = crate::locate::Spans::of(&file.bytes);
            diagnostic = diagnostic
                .at(file.relative.clone())
                .locate_in(&file.bytes, &spans);
        }
        self.out.push(Finding { diagnostic, remedy });
    }

    fn edit(
        &self,
        table: &str,
        change: impl FnOnce(&mut crate::schema::document::Editor),
    ) -> Option<Remedy> {
        let schema = self.catalog.schemas.get(table)?;
        let mut editor = schema.edit();
        change(&mut editor);
        // A remedy is offered only when the edited document is itself a valid
        // schema; the check is the decoder every schema passes.
        let document = editor.document().clone();
        editor.finish().ok()?;
        Some(Remedy::Edit(BTreeMap::from([(
            table.to_string(),
            document,
        )])))
    }
}

fn column_pointer(name: &str) -> String {
    format!("/properties/{}", escape_pointer(name))
}

/// Every lint finding over the tables whose schemas can be analysed.
pub fn lint(catalog: &Catalog, config: &Config, descriptions: bool) -> Result<Vec<Finding>> {
    let mut lint = Lint {
        catalog,
        out: vec![],
    };
    for (table, schema) in &catalog.schemas {
        if catalog.blocked(table) {
            continue;
        }
        let rows = catalog.rows(table)?;
        table_findings(&mut lint, table, schema, &rows, config, descriptions)?;
    }
    reference_findings(&mut lint, config)?;
    Ok(lint.out)
}

fn table_findings(
    lint: &mut Lint<'_>,
    table: &str,
    schema: &Schema,
    rows: &[crate::catalog::Row],
    config: &Config,
    descriptions: bool,
) -> Result<()> {
    let catalog = lint.catalog;
    if !catalog.pinned.contains(table) {
        lint.schema(
            table,
            "",
            Diagnostic::warning(
                "LINT_SCHEMA_UNPINNED",
                format!(
                    "{table} is governed by an inferred working schema; deleting .db discards any \
                     refinement inference cannot re-derive"
                ),
            )
            .fix("FIX_PIN_SCHEMA"),
            Some(Remedy::Pin(table.to_string())),
        );
    }
    if schema.additional_fields() == AdditionalFields::Allow {
        lint.schema(
            table,
            "/additionalProperties",
            Diagnostic::warning(
                "LINT_ADDITIONAL_FIELDS_ALLOWED",
                format!("{table} accepts members its schema does not declare"),
            ),
            None,
        );
    }
    for (name, column) in schema.columns() {
        let at = column_pointer(name);
        let values: Vec<Option<&Value>> = rows
            .iter()
            .map(|row| row.value.get(name).or(column.default()))
            .collect();
        let nonnull: Vec<&Value> = values
            .iter()
            .flatten()
            .copied()
            .filter(|value| !value.is_null())
            .collect();
        if !rows.is_empty() && column.nullable() && nonnull.len() == rows.len() {
            lint.schema(
                table,
                &at,
                Diagnostic::warning(
                    "LINT_NULLABLE_NEVER_NULL",
                    format!("{table}.{name} admits null, and no row holds null"),
                )
                .field(name.as_str())
                .fix("FIX_TIGHTEN_NULLABLE"),
                lint.edit(table, |editor| {
                    editor.set_nullable(name, false);
                }),
            );
        }
        if !rows.is_empty() && nonnull.is_empty() {
            lint.schema(
                table,
                &at,
                Diagnostic::warning(
                    "LINT_COLUMN_NEVER_POPULATED",
                    format!("no row of {table} holds a value in {name}"),
                )
                .field(name.as_str()),
                None,
            );
        }
        let present = rows
            .iter()
            .filter(|row| row.value.contains_key(name))
            .count();
        if present > 0
            && present < rows.len()
            && let Some(absent) = rows.iter().find(|row| !row.value.contains_key(name))
        {
            lint.out.push(Finding {
                diagnostic: Diagnostic::warning(
                    "LINT_INCONSISTENT_PRESENCE",
                    format!(
                        "{table}.{name} is present in {present} of {} rows; this row lacks it",
                        rows.len()
                    ),
                )
                .table(table)
                .field(name.as_str())
                .at(absent.relative.clone()),
                remedy: None,
            });
        }
        let keeping = |replacement: Value| -> Value {
            let mut replacement = replacement;
            for kept in ["description", "default", "title", "$comment", "examples"] {
                if let Some(value) = schema.document().pointer(&format!("{at}/{kept}")) {
                    replacement[kept] = value.clone();
                }
            }
            replacement
        };
        if column.kind() == &ColumnType::String && !nonnull.is_empty() {
            let texts: Vec<&str> = nonnull.iter().filter_map(|value| value.as_str()).collect();
            let narrower = if texts.iter().all(|s| {
                s.len() == 36 && uuid::Uuid::parse_str(s).is_ok() && *s == s.to_ascii_lowercase()
            }) {
                Some(ColumnType::Uuid)
            } else if texts.iter().all(|s| {
                s.len() == 26 && ulid::Ulid::from_string(s).is_ok() && *s == s.to_ascii_uppercase()
            }) {
                Some(ColumnType::Ulid)
            } else if texts
                .iter()
                .all(|s| chrono::DateTime::parse_from_rfc3339(s).is_ok())
            {
                Some(ColumnType::Timestamp)
            } else if texts
                .iter()
                .all(|s| s.len() == 10 && chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok())
            {
                Some(ColumnType::Date)
            } else {
                None
            };
            if let Some(kind) = narrower {
                let replacement = keeping(subschema(&kind, column.nullable()));
                lint.schema(
                    table,
                    &at,
                    Diagnostic::warning(
                        "LINT_WIDER_TYPE",
                        format!(
                            "{table}.{name} is a string, and every value is a {}",
                            kind.name()
                        ),
                    )
                    .field(name.as_str())
                    .fix("FIX_NARROW_TYPE"),
                    lint.edit(table, |editor| {
                        editor.set_column(name, replacement);
                    }),
                );
            }
            let distinct: std::collections::BTreeSet<&str> = texts.iter().copied().collect();
            if texts.len() >= 3 * distinct.len() && distinct.len() <= config.enum_max_values {
                let members: Vec<String> = distinct.iter().map(|s| s.to_string()).collect();
                let replacement = keeping(enum_subschema(&members, column.nullable()));
                lint.schema(
                    table,
                    &at,
                    Diagnostic::suggestion(
                        "LINT_ENUM_CANDIDATE",
                        format!(
                            "{table}.{name} holds only {} distinct values: {}",
                            distinct.len(),
                            members.join(", ")
                        ),
                    )
                    .field(name.as_str())
                    .fix("FIX_ADD_ENUM"),
                    lint.edit(table, |editor| {
                        editor.set_column(name, replacement);
                    }),
                );
            }
        }
        if column.kind() == &ColumnType::Float
            && !nonnull.is_empty()
            && nonnull.iter().all(|value| value.as_i64().is_some())
        {
            let replacement = keeping(subschema(&ColumnType::Int, column.nullable()));
            lint.schema(
                table,
                &at,
                Diagnostic::warning(
                    "LINT_WIDER_TYPE",
                    format!("{table}.{name} is a float, and every value is an int"),
                )
                .field(name.as_str())
                .fix("FIX_NARROW_TYPE"),
                lint.edit(table, |editor| {
                    editor.set_column(name, replacement);
                }),
            );
        }
        let keyed = schema
            .candidate_keys()
            .any(|key| key == std::slice::from_ref(name));
        if !rows.is_empty() && !keyed && column.kind().is_scalar() && nonnull.len() == rows.len() {
            let distinct: HashSet<String> = nonnull
                .iter()
                .map(|value| crate::canonical::compact(value))
                .collect();
            if distinct.len() == rows.len() && rows.len() >= config.unique_min_rows {
                lint.schema(
                    table,
                    &at,
                    Diagnostic::suggestion(
                        "LINT_UNIQUE_CANDIDATE",
                        format!(
                            "every one of the {} rows of {table} holds a different {name}",
                            rows.len()
                        ),
                    )
                    .field(name.as_str())
                    .fix("FIX_ADD_UNIQUE"),
                    lint.edit(table, |editor| {
                        editor.add_list("unique", std::slice::from_ref(name));
                    }),
                );
            }
        }
        if rows.len() >= config.unique_min_rows && !nonnull.is_empty() {
            let quoted = crate::mirror::quote(name);
            let candidate = if column.kind() == &ColumnType::Int
                && nonnull.iter().all(|v| v.as_i64().is_some_and(|x| x >= 0))
            {
                Some((
                    format!("{name}_nonnegative"),
                    format!("{quoted} >= 0"),
                    "is never negative",
                ))
            } else if column.kind() == &ColumnType::String
                && nonnull
                    .iter()
                    .all(|v| v.as_str().is_some_and(|x| !x.is_empty()))
            {
                Some((
                    format!("{name}_nonempty"),
                    format!("{quoted} <> ''"),
                    "is never empty",
                ))
            } else {
                None
            };
            if let Some((check, expr, what)) = candidate
                && !schema
                    .checks()
                    .iter()
                    .any(|existing| existing.expr() == expr || existing.name() == check)
            {
                lint.schema(
                    table,
                    &at,
                    Diagnostic::suggestion(
                        "LINT_CHECK_CANDIDATE",
                        format!("{table}.{name} {what}"),
                    )
                    .field(name.as_str())
                    .fix("FIX_ADD_CHECK"),
                    lint.edit(table, |editor| {
                        editor.add_check(&check, &expr);
                    }),
                );
            }
        }
        if descriptions && column.description().is_none() {
            lint.schema(
                table,
                &at,
                Diagnostic::suggestion(
                    "LINT_NO_DESCRIPTION",
                    format!("{table}.{name} has no description"),
                )
                .field(name.as_str()),
                None,
            );
        }
    }
    for (index, fk) in schema.foreign_keys().iter().enumerate() {
        let declared = schema
            .document()
            .pointer(&format!("/x-reldir/foreignKeys/{index}"));
        let unsaid: Vec<&str> = [("onDelete", "deleted"), ("onUpdate", "re-keyed")]
            .into_iter()
            .filter(|(key, _)| declared.is_some_and(|fk| fk.get(*key).is_none()))
            .map(|(_, event)| event)
            .collect();
        if !unsaid.is_empty() {
            lint.schema(
                table,
                &format!("/x-reldir/foreignKeys/{index}"),
                Diagnostic::suggestion(
                    "LINT_FK_ACTION_DEFAULTED",
                    format!(
                        "{} does not say what happens when its target is {}, so {} refused",
                        fk.describe(table),
                        unsaid.join(" or "),
                        if unsaid.len() == 1 {
                            "that is"
                        } else {
                            "both are"
                        }
                    ),
                ),
                None,
            );
        }
    }
    if schema.primary_key().len() == 1 {
        let key = &schema.primary_key()[0];
        let generator = match schema.column(key).map(|column| column.kind()) {
            Some(ColumnType::Uuid) => Some(GeneratedKind::Uuid),
            Some(ColumnType::Ulid) => Some(GeneratedKind::Ulid),
            _ => None,
        };
        if let Some(kind) = generator
            && schema
                .column(key)
                .is_some_and(|column| column.generated().is_none())
        {
            lint.schema(
                table,
                &column_pointer(key),
                Diagnostic::suggestion(
                    "LINT_PK_NOT_GENERATED",
                    format!(
                        "{table}.{key} is a {} key that every insert must supply",
                        kind.name()
                    ),
                )
                .field(key.as_str())
                .fix("FIX_ADD_GENERATOR"),
                lint.edit(table, |editor| {
                    editor.set_generated(key, Some(kind));
                }),
            );
        }
    }
    let mut noncanonical = vec![];
    catalog.mirror.each_file(table, |entry| {
        if let Some(doc) = &entry.doc {
            let canonical = crate::canonical::pretty_with_indent(
                &Value::Object(doc.clone()),
                config.indentation_width,
            );
            if crate::canonical::hash_bytes(&canonical) != entry.raw_hash {
                noncanonical.push(std::path::PathBuf::from(entry.path));
            }
        }
        Ok(())
    })?;
    if let Some(first) = noncanonical.first() {
        lint.out.push(Finding {
            diagnostic: Diagnostic::info(
                "LINT_NON_CANONICAL_FORMATTING",
                format!(
                    "{} row(s) of {table} are not in canonical formatting",
                    noncanonical.len()
                ),
            )
            .table(table)
            .at(first.clone())
            .fix("FIX_CANONICALIZE"),
            remedy: Some(Remedy::Canonicalize(noncanonical)),
        });
    }
    if descriptions && schema.description().is_none() {
        lint.schema(
            table,
            "",
            Diagnostic::suggestion(
                "LINT_NO_DESCRIPTION",
                format!("table {table} has no description"),
            ),
            None,
        );
    }
    Ok(())
}

/// References the data supports and no schema declares, from the analysis
/// inference uses.
fn reference_findings(lint: &mut Lint<'_>, config: &Config) -> Result<()> {
    let catalog = lint.catalog;
    let mut facts = BTreeMap::new();
    for (table, schema) in &catalog.schemas {
        if catalog.blocked(table) {
            continue;
        }
        let rows = catalog.rows(table)?;
        let key = (schema.primary_key().len() == 1)
            .then(|| {
                schema
                    .column(&schema.primary_key()[0])
                    .map(|c| (schema.primary_key()[0].clone(), c.kind().clone()))
            })
            .flatten();
        facts.insert(
            table.clone(),
            TableFacts::gather(table, key, Some(schema), rows.iter().map(|row| &row.value)),
        );
    }
    for proposal in references::detect(&facts, config) {
        let code = match &proposal.target {
            ProposedTarget::Table(_) => "LINT_FK_CANDIDATE",
            ProposedTarget::Domain { .. } => "LINT_DOMAIN_CANDIDATE",
        };
        let mut edits = BTreeMap::new();
        let mut valid = true;
        if let ProposedTarget::Domain { name, join, .. } = &proposal.target {
            for table in join {
                let Some(schema) = catalog.schemas.get(table) else {
                    valid = false;
                    break;
                };
                let mut editor = schema.edit();
                editor.set_identity_domain(Some(name));
                edits.insert(table.clone(), editor.document().clone());
            }
        }
        let base = edits.get(&proposal.table).cloned().or_else(|| {
            catalog
                .schemas
                .get(&proposal.table)
                .map(|schema| schema.document().clone())
        });
        if let Some(document) = base {
            let mut editor = Schema::from_document(document, None)
                .ok()
                .map(|schema| schema.edit());
            if let Some(editor) = editor.as_mut() {
                editor.add_foreign_key(proposal.definition());
                edits.insert(proposal.table.clone(), editor.document().clone());
            } else {
                valid = false;
            }
        }
        valid &= edits
            .values()
            .all(|document| Schema::from_document(document.clone(), None).is_ok());
        let column = proposal.path.column().to_string();
        lint.schema(
            &proposal.table,
            &column_pointer(&column),
            Diagnostic::suggestion(code, proposal.describe())
                .field(proposal.path.to_string())
                .expected(proposal.definition().to_string())
                .fix("FIX_ADD_FK"),
            valid.then_some(Remedy::Edit(edits)),
        );
    }
    Ok(())
}

/// The diagnostics alone, for commands that only report.
pub fn diagnostics(findings: &[Finding]) -> Vec<Diagnostic> {
    findings
        .iter()
        .map(|finding| finding.diagnostic.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, fs::Disk, mirror::Mirror};
    use std::{fs, path::Path, rc::Rc};

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn catalog(root: &Path) -> Catalog {
        Catalog::observe(
            root,
            &Config::default(),
            &Disk,
            Rc::new(Mirror::open_memory().unwrap()),
            false,
        )
        .unwrap()
    }

    fn schema(table: &str, properties: &str, extra: &str) -> String {
        format!(
            r#"{{"$schema":"{}","type":"object","properties":{properties},"required":["id"],"additionalProperties":false,"x-reldir":{{"table":"{table}","primaryKey":["id"]{extra}}}}}"#,
            crate::schema::meta::DIALECT_URI
        )
    }

    /// Every fix the documentation pairs with a lint is attached where that
    /// finding is raised, and comes with a remedy that applies it: a database
    /// is built that exhibits each finding, and each finding is judged by what
    /// lint actually reports for it.
    #[test]
    fn test1146_every_documented_fix_is_attached_to_its_finding() {
        let documentation = fs::read_to_string("docs/validation.md").unwrap();
        let mut promised: Vec<(String, String)> = vec![];
        for line in documentation.lines() {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            if cells.len() < 5 || !cells[1].starts_with("`FIX_") {
                continue;
            }
            let fix = cells[1].trim_matches('`');
            for code in cells[3]
                .split('`')
                .filter(|token| token.starts_with("LINT_"))
            {
                promised.push((fix.to_string(), code.to_string()));
            }
        }
        assert!(promised.len() >= 10, "found {}", promised.len());

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        // `things` is unpinned, nullable-but-never-null, float-but-integral,
        // enum-like, unique, never-negative, and references `owners` both by a
        // conventional name and by one inference would not declare.
        write(
            root,
            ".db/schema/things.json",
            &schema(
                "things",
                r#"{"id":{"type":"string"},"n":{"type":["number","null"]},"state":{"type":"string"},"code":{"type":"string"},"count":{"type":"integer","x-reldir-type":"int"},"owned_by":{"type":"string"}}"#,
                "",
            ),
        );
        write(
            root,
            "schema/owners.json",
            &schema("owners", r#"{"id":{"type":"string"}}"#, ""),
        );
        write(
            root,
            "schema/tokens.json",
            &schema("tokens", r#"{"id":{"type":"string","format":"uuid"}}"#, ""),
        );
        write(root, "owners/o1.json", r#"{"id":"o1"}"#);
        write(
            root,
            "tokens/0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e.json",
            "{\"id\":\"0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e\"}\n",
        );
        for index in 0..24 {
            let state = ["open", "closed"][index % 2];
            write(
                root,
                &format!("things/t{index:02}.json"),
                &format!(
                    r#"{{"id":"t{index:02}","n":{index},"state":"{state}","code":"c{index}","count":{index},"owned_by":"o1"}}"#
                ),
            );
        }
        let findings = lint(&catalog(root), &Config::default(), false).unwrap();
        for (fix, code) in promised {
            if code == "LINT_DOMAIN_CANDIDATE" {
                continue; // exercised by test2171 and the behavior suite
            }
            let raised: Vec<&Finding> = findings
                .iter()
                .filter(|f| f.diagnostic.code == code)
                .collect();
            assert!(
                !raised.is_empty(),
                "{code} is documented as the source of {fix}, and this database exhibits it"
            );
            for finding in raised {
                assert!(
                    finding.diagnostic.fixes.contains(&fix),
                    "{code} does not offer {fix}: {:?}",
                    finding.diagnostic
                );
                assert!(
                    finding.remedy.is_some(),
                    "{code} offers {fix} with no remedy to apply"
                );
            }
        }
    }

    #[test]
    fn test2190_remedies_are_valid_schemas_that_apply_the_finding() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(
            root,
            ".db/schema/things.json",
            &schema(
                "things",
                r#"{"id":{"type":"string"},"n":{"type":["number","null"]}}"#,
                "",
            ),
        );
        for index in 0..3 {
            write(
                root,
                &format!("things/t{index}.json"),
                &format!("{{\"id\":\"t{index}\",\"n\":{index}}}"),
            );
        }
        let findings = lint(&catalog(root), &Config::default(), false).unwrap();
        let codes: Vec<&str> = findings
            .iter()
            .map(|f| f.diagnostic.code.as_str())
            .collect();
        for expected in [
            "LINT_SCHEMA_UNPINNED",
            "LINT_NULLABLE_NEVER_NULL",
            "LINT_WIDER_TYPE",
        ] {
            assert!(codes.contains(&expected), "{expected} in {codes:?}");
        }
        let narrow = findings
            .iter()
            .find(|f| f.diagnostic.code == "LINT_WIDER_TYPE")
            .unwrap();
        let Some(Remedy::Edit(edits)) = &narrow.remedy else {
            panic!("no remedy")
        };
        let edited = Schema::from_document(edits["things"].clone(), None).unwrap();
        assert_eq!(edited.column("n").unwrap().kind(), &ColumnType::Int);
        assert!(
            edited.column("n").unwrap().nullable(),
            "narrowing keeps what the column admitted"
        );
        assert_eq!(narrow.diagnostic.pointer.as_deref(), Some("/properties/n"));
        assert!(
            narrow.diagnostic.location.is_some(),
            "located in the schema file"
        );
    }

    #[test]
    fn test2191_undeclared_nested_references_are_proposed_with_a_remedy() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write(
            root,
            ".db/schema/lessons.json",
            &schema("lessons", r#"{"id":{"type":"string"}}"#, ""),
        );
        write(
            root,
            ".db/schema/courses.json",
            &schema(
                "courses",
                r#"{"id":{"type":"string"},"lesson_refs":{"type":"array","items":{"type":"string"}}}"#,
                "",
            ),
        );
        write(root, "lessons/l1.json", r#"{"id":"l1"}"#);
        write(
            root,
            "courses/c1.json",
            r#"{"id":"c1","lesson_refs":["l1"]}"#,
        );
        let findings = lint(&catalog(root), &Config::default(), false).unwrap();
        let candidate = findings
            .iter()
            .find(|f| f.diagnostic.code == "LINT_FK_CANDIDATE")
            .expect("proposed");
        assert_eq!(candidate.diagnostic.field.as_deref(), Some("lesson_refs[]"));
        let Some(Remedy::Edit(edits)) = &candidate.remedy else {
            panic!()
        };
        let edited = Schema::from_document(edits["courses"].clone(), None).unwrap();
        assert_eq!(
            edited.foreign_keys()[0].from()[0].to_string(),
            "lesson_refs[]"
        );
        assert!(
            findings
                .iter()
                .all(|f| f.diagnostic.code != "LINT_FK_NO_INDEX"),
            "reference indexes are automatic"
        );
    }
}
