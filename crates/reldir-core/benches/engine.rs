//! The engine's hot paths, each on its own: editing a row file, merging
//! three versions of one, rendering keys, and judging a change by its scope
//! against judging the whole state. `cargo bench -p reldir-core`.

use criterion::{Criterion, criterion_group, criterion_main};
use reldir_core::{
    catalog::Catalog,
    config::Config,
    engine::fs::{Disk, Overlay},
    integrity,
    merge::{Labels, merge_documents},
    mirror::Mirror,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    hint::black_box,
    path::PathBuf,
    rc::Rc,
};

fn row(index: usize) -> Value {
    json!({
        "id": format!("r{index}"),
        "name": format!("Row number {index}"),
        "amount": format!("{}.{}", index % 1000, index % 100),
        "when": "2024-03-01T10:00:00+02:00",
        "tags": ["a", "b", "c"],
        "nested": {"depth": 1, "label": "inner"}
    })
}

fn edit(c: &mut Criterion) {
    let raw = serde_json::to_vec_pretty(&row(7)).unwrap();
    let mut after = row(7);
    after["amount"] = json!("1.25");
    c.bench_function("json_edit: change one member of a row file", |b| {
        b.iter(|| {
            reldir_core::json_edit::edit_document(black_box(&raw), black_box(&after)).unwrap()
        })
    });
}

fn merge(c: &mut Criterion) {
    let base = serde_json::to_vec_pretty(&row(1)).unwrap();
    let mut ours = row(1);
    ours["name"] = json!("Ours");
    let mut theirs = row(1);
    theirs["amount"] = json!("9.99");
    let (ours, theirs) = (
        serde_json::to_vec_pretty(&ours).unwrap(),
        serde_json::to_vec_pretty(&theirs).unwrap(),
    );
    c.bench_function("merge: three versions of a row file", |b| {
        b.iter(|| merge_documents(&base, &ours, &theirs, &Labels::default()).unwrap())
    });
}

/// A table of `rows` rows with a unique key, a reference and a check, and
/// a catalog observing it.
fn table(rows: usize) -> (tempfile::TempDir, Catalog) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    std::fs::create_dir_all(root.join(".db/schema")).unwrap();
    std::fs::create_dir_all(root.join("items")).unwrap();
    let schema = json!({
        "$schema": reldir_core::schema::meta::DIALECT_URI, "type": "object",
        "properties": {"id": {"type": "string"}, "n": {"type": "integer", "x-reldir-type": "int"}, "parent": {"type": "string"}},
        "required": ["id", "n"], "additionalProperties": false,
        "x-reldir": {"table": "items", "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}},
                     "unique": [["n"]], "checks": [{"name": "non_negative", "expr": "n >= 0"}],
                     "foreignKeys": [{"from": ["parent"], "to": {"table": "items"}}]}
    });
    std::fs::write(root.join(".db/schema/items.json"), schema.to_string()).unwrap();
    for index in 0..rows {
        let mut row = json!({"id": format!("i{index}"), "n": index});
        if index > 0 {
            row["parent"] = json!(format!("i{}", index / 2));
        }
        std::fs::write(root.join(format!("items/i{index}.json")), row.to_string()).unwrap();
    }
    let catalog = Catalog::observe(
        root,
        &Config::default(),
        &Disk,
        Rc::new(Mirror::open_memory().unwrap()),
        false,
    )
    .unwrap();
    (directory, catalog)
}

fn validation(c: &mut Criterion) {
    let (directory, catalog) = table(20_000);
    let root = directory.path();
    let verdict = integrity::validate(&catalog).unwrap();
    let path = PathBuf::from("items/i77.json");
    let bytes = json!({"id": "i77", "n": 20_077, "parent": "i3"})
        .to_string()
        .into_bytes();
    let writes = BTreeMap::from([(path.clone(), bytes.clone())]);
    let overlay_writes = BTreeMap::from([(root.join(&path), bytes)]);
    let deletes = BTreeSet::new();
    let config = Config::default();
    c.bench_function(
        "validate: one row changed, judged by its scope (20k rows)",
        |b| {
            b.iter(|| {
                let overlay = Overlay::new(&Disk, overlay_writes.clone(), BTreeSet::new());
                let prospect = catalog
                    .prospect_rows(&config, &writes, &deletes, &overlay, Some(&verdict))
                    .unwrap()
                    .unwrap();
                prospect.abandon().unwrap()
            })
        },
    );
    c.bench_function("validate: the whole state (20k rows)", |b| {
        b.iter(|| integrity::validate(black_box(&catalog)).unwrap())
    });
}

fn keys(c: &mut Criterion) {
    let (_directory, catalog) = table(10);
    let schema = &catalog.schemas["items"];
    let row = json!({"id": "i3", "n": 3}).as_object().cloned().unwrap();
    c.bench_function("mirror: the keys a row holds", |b| {
        b.iter(|| reldir_core::mirror::row_keys(black_box(&row), schema))
    });
}

criterion_group! {
    name = engine;
    config = Criterion::default().sample_size(20);
    targets = edit, merge, keys, validation
}
criterion_main!(engine);
