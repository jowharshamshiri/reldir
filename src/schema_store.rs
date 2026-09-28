//! Where schemas live, and what the two locations mean.
//!
//! A table's schema is one document, in one of two places:
//!
//! - `schema/<table>.json`, a **pin**: a declaration the user keeps beside
//!   their data and in version control. A pinned table's schema *is* its pin;
//!   reldir reads it where it lies, and a change reldir makes to a pinned
//!   table's schema (a migration, an applied fix) is written to the pin.
//! - `.db/schema/<table>.json`, a **working schema**: one reldir inferred for a
//!   table nobody pinned. It is reconstructible from the rows it describes, so
//!   deleting `.db/` loses nothing a pin would keep.
//!
//! A working schema left beside a pin for the same table is a stale copy from
//! before the pin existed. It governs nothing, and a writing command removes
//! it.
//!
//! Recorded history names every table's schema `schema/<table>.json`,
//! whichever place it lives in: where a schema is kept is not part of what the
//! database says, so pinning a table does not change the state.

use crate::{
    catalog::Catalog,
    schema::{Schema, SchemaFileKind},
};
use std::path::{Path, PathBuf};

/// The directory holding working schemas, which reldir maintains.
pub fn working_dir(root: &Path) -> PathBuf {
    root.join(".db/schema")
}

/// The directory holding pinned schemas, which the user declares.
pub fn pin_dir(root: &Path) -> PathBuf {
    root.join("schema")
}

/// The working schema file for one table.
pub fn working_path(root: &Path, table: &str) -> PathBuf {
    working_dir(root).join(format!("{table}.json"))
}

/// The pin file for one table.
pub fn pin_path(root: &Path, table: &str) -> PathBuf {
    pin_dir(root).join(format!("{table}.json"))
}

/// The working schema's path relative to the database root, as transactions
/// name it.
pub fn working_relative(table: &str) -> PathBuf {
    PathBuf::from(format!(".db/schema/{table}.json"))
}

/// A table's schema as recorded history names it, wherever it lives.
pub fn pin_relative(table: &str) -> String {
    format!("schema/{table}.json")
}

/// The file a table's schema is written to: its pin when it has one, its
/// working schema otherwise. Relative to the database root.
pub fn home(catalog: &Catalog, table: &str) -> PathBuf {
    match catalog.schema_files.get(table).map(|file| file.kind) {
        Some(SchemaFileKind::Pin) => PathBuf::from(pin_relative(table)),
        _ => working_relative(table),
    }
}

/// The table a relative path's schema file governs, if it is one.
pub fn schema_table(path: &Path) -> Option<&str> {
    let parent = path.parent()?;
    if parent != Path::new("schema") && parent != Path::new(".db/schema") {
        return None;
    }
    if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
        return None;
    }
    path.file_stem().and_then(|stem| stem.to_str())
}

/// Whether two schemas impose the same rules. Formatting, member order and
/// annotations are not rules.
pub fn equivalent(left: &Schema, right: &Schema) -> bool {
    left.identity() == right.identity()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(extra_annotation: Option<&str>, allow_additional: bool) -> Schema {
        let mut document = json!({
            "$schema": crate::schema::meta::DIALECT_URI,
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"],
            "additionalProperties": allow_additional,
            "x-reldir": { "table": "users", "primaryKey": ["id"] }
        });
        if let Some(text) = extra_annotation {
            document["description"] = json!(text);
        }
        Schema::from_document(document, None).unwrap()
    }

    #[test]
    fn test1120_working_schemas_and_pins_occupy_separate_locations() {
        let root = Path::new("/db");
        assert_eq!(working_path(root, "users"), root.join(".db/schema/users.json"));
        assert_eq!(pin_path(root, "users"), root.join("schema/users.json"));
        assert!(working_path(root, "users").starts_with(root.join(".db")));
        assert!(!pin_path(root, "users").starts_with(root.join(".db")));
    }

    #[test]
    fn test1121_schema_paths_are_recognised_in_both_locations_only() {
        assert_eq!(schema_table(Path::new("schema/users.json")), Some("users"));
        assert_eq!(schema_table(Path::new(".db/schema/users.json")), Some("users"));
        for other in ["users/u1.json", "schema/users.txt", "x/schema/users.json", ".db/config"] {
            assert_eq!(schema_table(Path::new(other)), None, "{other}");
        }
    }

    #[test]
    fn test1122_equivalence_is_identity_ignoring_annotations_but_not_rules() {
        let plain = schema(None, false);
        assert!(equivalent(&plain, &schema(Some("people we know"), false)), "an annotation is not a rule");
        assert!(!equivalent(&plain, &schema(None, true)), "a different rule is a different schema");
        let wide = Schema::from_bytes(&plain.bytes(4)).unwrap();
        assert!(equivalent(&plain, &wide), "formatting is not a rule");
    }

}
