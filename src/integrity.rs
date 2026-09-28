//! Whether the observed directory is a valid database, and exactly why not.
//!
//! Row-local faults -- a row that does not satisfy its schema, a file whose
//! name is not its identity -- are found when a file is read and cached with
//! it in the mirror. Everything that relates rows to each other is answered
//! here by queries over the mirror's keys and edges: duplicate primary keys and
//! unique values, keys shared across an identity domain, references that name
//! nothing, cycles in an acyclic graph, rows that fail a check, and rows an
//! assertion names.
//!
//! Every diagnostic names the file, the JSON Pointer inside it, the rule it
//! broke, and -- read from the file's own bytes -- the line and column.

use crate::{
    catalog::Catalog,
    diagnostic::{Diagnostic, Result},
    mirror,
    schema::{Action, ColumnType, Schema, Severity, Target},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// What an observation found.
#[derive(Debug, Default)]
pub struct Verdict {
    /// Faults that make the database invalid.
    pub errors: Vec<Diagnostic>,
    /// Findings that do not.
    pub warnings: Vec<Diagnostic>,
}

/// The tables a foreign key may point into.
pub fn target_tables(catalog_schemas: &BTreeMap<String, Schema>, target: &Target) -> Vec<String> {
    match target {
        Target::Table(table) => vec![table.clone()],
        Target::Tables(tables) => tables.clone(),
        Target::Domain(domain) => catalog_schemas
            .values()
            .filter(|schema| schema.identity_domain() == Some(domain.as_str()))
            .map(|schema| schema.table().to_string())
            .collect(),
    }
}

/// The columns of `target_table` a foreign key compares with.
pub fn target_columns(fk: &crate::schema::ForeignKey, target: &Schema) -> Vec<String> {
    if fk.columns().is_empty() {
        target.primary_key().to_vec()
    } else {
        fk.columns().to_vec()
    }
}

fn compatible(left: &ColumnType, right: &ColumnType) -> bool {
    left == right
        || matches!(
            (left, right),
            (ColumnType::String, ColumnType::Enum) | (ColumnType::Enum, ColumnType::String)
        )
}

/// Rules that relate one schema to another, checked once per observation.
pub fn validate_schemas(catalog: &mut Catalog) {
    let mut out = vec![];
    let schemas = &catalog.schemas;
    let fault = |table: &str, code: &str, message: String, pointer: String| {
        Diagnostic::error(code, message)
            .table(table)
            .pointer(pointer)
    };

    // Identity domains relate single-column keys of one type.
    let mut domains: BTreeMap<&str, Vec<&Schema>> = BTreeMap::new();
    for schema in schemas.values() {
        if let Some(domain) = schema.identity_domain() {
            domains.entry(domain).or_default().push(schema);
        }
    }
    for (domain, members) in &domains {
        let kinds: BTreeSet<&str> = members
            .iter()
            .filter_map(|schema| schema.column(&schema.primary_key()[0]))
            .map(|column| column.kind().name())
            .collect();
        if kinds.len() > 1 {
            for schema in members {
                out.push(fault(
                    schema.table(),
                    "SCHEMA_DOMAIN_KEY_INVALID",
                    format!(
                        "the tables of identity domain {domain:?} key on different types ({}); a \
                         domain is one key namespace, so its keys must be comparable",
                        kinds.iter().copied().collect::<Vec<_>>().join(", ")
                    ),
                    "/x-reldir/identityDomain".into(),
                ));
            }
        }
    }

    for schema in schemas.values() {
        let table = schema.table();
        for (index, fk) in schema.foreign_keys().iter().enumerate() {
            let at = format!("/x-reldir/foreignKeys/{index}");
            let targets = target_tables(schemas, fk.to());
            if targets.is_empty() {
                out.push(fault(
                    table,
                    "SCHEMA_FK_TARGET_MISSING",
                    format!(
                        "foreign key {} names {}, which no table belongs to",
                        fk.name(),
                        fk.to().describe()
                    ),
                    format!("{at}/to"),
                ));
                continue;
            }
            let leaves: Vec<Option<ColumnType>> = fk
                .from()
                .iter()
                .map(|path| {
                    crate::schema::document::resolve_path(schema, path)
                        .ok()
                        .map(|leaf| leaf.column.kind().clone())
                })
                .collect();
            for target_table in &targets {
                let Some(target) = schemas.get(target_table) else {
                    out.push(fault(
                        table,
                        "SCHEMA_FK_TARGET_MISSING",
                        format!(
                            "foreign key {} references table {target_table:?}, which has no schema",
                            fk.name()
                        ),
                        format!("{at}/to"),
                    ));
                    continue;
                };
                let columns = target_columns(fk, target);
                if !matches!(fk.to(), Target::Table(_)) && columns.len() != 1 {
                    out.push(fault(
                        table,
                        "SCHEMA_FK_TARGET_INVALID",
                        format!(
                            "{target_table:?} keys on {} columns; references into several tables compare \
                             with a single-column key",
                            columns.len()
                        ),
                        format!("{at}/to"),
                    ));
                    continue;
                }
                if !target.candidate_keys().any(|key| key == columns.as_slice()) {
                    out.push(fault(
                        table,
                        "SCHEMA_FK_TARGET_NOT_UNIQUE",
                        format!(
                            "{target_table}({}) is neither the primary key nor a unique constraint, so a \
                             reference to it could name more than one row",
                            columns.join(",")
                        ),
                        format!("{at}/columns"),
                    ));
                    continue;
                }
                if columns.len() != fk.from().len() {
                    out.push(fault(
                        table,
                        "SCHEMA_FK_ACTION_INVALID",
                        format!(
                            "{} path(s) cannot be compared with {target_table}'s {}-column key",
                            fk.from().len(),
                            columns.len()
                        ),
                        format!("{at}/from"),
                    ));
                    continue;
                }
                for ((leaf, column), path) in leaves.iter().zip(&columns).zip(fk.from()) {
                    let Some(leaf) = leaf else { continue };
                    let Some(target_column) = target.column(column) else {
                        continue;
                    };
                    if !compatible(leaf, target_column.kind()) {
                        out.push(fault(
                            table,
                            "SCHEMA_FK_TYPE_MISMATCH",
                            format!(
                                "{path} holds {} values but {target_table}.{column} is {}; they can never \
                                 be equal",
                                leaf.name(),
                                target_column.kind().name()
                            ),
                            format!("{at}/from"),
                        ));
                    }
                }
            }
        }
    }

    // Checks and assertions are SQL; each must compile against the tables it
    // may read.
    out.extend(crate::sql::compile_rules(schemas));
    catalog
        .diagnostics
        .extend(out.into_iter().map(|diagnostic| {
            match diagnostic
                .table
                .as_ref()
                .and_then(|table| catalog.schema_files.get(table))
            {
                Some(file) => {
                    let spans = crate::locate::Spans::of(&file.bytes);
                    diagnostic
                        .at(file.relative.clone())
                        .locate_in(&file.bytes, &spans)
                }
                None => diagnostic,
            }
        }));
}

/// Judge the observed state.
pub fn validate(catalog: &Catalog) -> Result<Verdict> {
    validate_through(catalog, &crate::fs::Disk)
}

/// Judge a state observed through `source`, reading files through it to
/// locate each fault in the bytes that state holds.
pub fn validate_through(catalog: &Catalog, source: &dyn crate::fs::Source) -> Result<Verdict> {
    let mut verdict = Verdict {
        errors: catalog.diagnostics.clone(),
        warnings: catalog.warnings.clone(),
    };
    for (path, diagnostics) in catalog.mirror.files_with_diagnostics()? {
        let raw = source.read(&catalog.root.join(&path)).ok();
        let spans = raw.as_deref().map(crate::locate::Spans::of);
        for mut diagnostic in diagnostics {
            diagnostic.path = Some(PathBuf::from(&path));
            if diagnostic.location.is_none()
                && let (Some(raw), Some(spans)) = (&raw, &spans)
            {
                diagnostic = diagnostic.locate_in(raw, spans);
            }
            verdict.errors.push(diagnostic);
        }
    }
    let usable: BTreeMap<String, Schema> = catalog
        .schemas
        .iter()
        .filter(|(table, _)| !catalog.blocked(table))
        .map(|(table, schema)| (table.clone(), schema.clone()))
        .collect();
    let mut located = Locator::new(&catalog.root, source);

    for (table, schema) in &usable {
        let tables = [table.clone()];
        for columns in schema.candidate_keys() {
            let constraint = mirror::constraint_name(schema, columns);
            let primary = constraint == mirror::PRIMARY;
            for duplicate in catalog.mirror.duplicates(&tables, &constraint, false)? {
                let first = &duplicate.holders[0].1;
                for (_, path) in &duplicate.holders[1..] {
                    verdict.errors.push(
                        located.at(
                            Diagnostic::error(
                                if primary {
                                    "PRIMARY_KEY_VIOLATION"
                                } else {
                                    "UNIQUE_VIOLATION"
                                },
                                format!(
                                    "{table}({}) must be unique, and {} also holds {}",
                                    columns.join(","),
                                    first,
                                    duplicate.key
                                ),
                            )
                            .table(table)
                            .observed(duplicate.key.clone())
                            .help(format!("also present in {first}")),
                            path,
                            Some(&format!(
                                "/{}",
                                crate::schema::path::escape_pointer(&columns[0])
                            )),
                        ),
                    );
                }
            }
        }
    }

    let mut domains: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for schema in usable.values() {
        if let Some(domain) = schema.identity_domain() {
            domains
                .entry(domain.to_string())
                .or_default()
                .push(schema.table().to_string());
        }
    }
    for (domain, tables) in &domains {
        for duplicate in catalog.mirror.duplicates(tables, mirror::PRIMARY, true)? {
            let (first_table, first) = &duplicate.holders[0];
            for (table, path) in &duplicate.holders[1..] {
                if table == first_table {
                    continue;
                }
                let key_column = usable[table].primary_key()[0].clone();
                verdict.errors.push(located.at(
                    Diagnostic::error(
                        "DOMAIN_KEY_VIOLATION",
                        format!(
                            "key {} is held by both {first} and {path}; identity domain {domain:?} is one \
                             namespace, so a key names one row across all of its tables",
                            duplicate.key
                        ),
                    )
                    .table(table)
                    .observed(duplicate.key.clone())
                    .help(format!("also present in {first}")),
                    path,
                    Some(&format!("/{}", crate::schema::path::escape_pointer(&key_column))),
                ));
            }
        }
    }

    for (table, schema) in &usable {
        for fk in schema.foreign_keys() {
            let targets = target_tables(&usable, fk.to());
            let all_targets = target_tables(&catalog.schemas, fk.to());
            if targets.is_empty() || targets.len() != all_targets.len() {
                // A target whose schema cannot be read holds no keys the
                // mirror knows; judging references into it would report every
                // one as dangling.
                continue;
            }
            let constraint = mirror::constraint_name(
                &usable[&targets[0]],
                &target_columns(fk, &usable[&targets[0]]),
            );
            let rule = mirror::fk_rule(table, fk.name());
            let removable = fk.from().iter().all(|path| {
                crate::schema::document::resolve_path(schema, path)
                    .is_ok_and(|leaf| leaf.iterates || leaf.column.nullable())
            });
            for edge in catalog.mirror.dangling(&rule, &constraint, &targets)? {
                let mut diagnostic = Diagnostic::error(
                    "FOREIGN_KEY_VIOLATION",
                    format!(
                        "{} references {} in {}, and no such row exists",
                        edge.pointer,
                        edge.target,
                        fk.to().describe()
                    ),
                )
                .table(table)
                .field(fk.from()[0].column().to_string())
                .expected(format!("an existing row of {}", fk.to().describe()))
                .observed(edge.target.clone())
                .fix("FIX_RESTORE_TARGET");
                if removable {
                    diagnostic = diagnostic.fix("FIX_REMOVE_REFERENCE");
                }
                diagnostic = diagnostic.fix("FIX_ORPHAN_DELETE_ROW");
                diagnostic.constraint = Some(format!(
                    "{} (onDelete: {})",
                    fk.describe(table),
                    fk.on_delete().name()
                ));
                verdict
                    .errors
                    .push(located.at(diagnostic, &edge.path, Some(&edge.pointer)));
            }
        }
        for graph in schema.acyclic() {
            let rule = mirror::acyclic_rule(table, graph.name());
            let edges = catalog.mirror.edges_of(&rule)?;
            for cycle in cycles(catalog, table, &edges)? {
                let names: Vec<&str> = cycle.iter().map(|(key, _)| key.as_str()).collect();
                let (_, path) = &cycle[0];
                verdict.errors.push(
                    located.at(
                        Diagnostic::error(
                            "CYCLE_VIOLATION",
                            format!(
                                "{} must be acyclic, but {} forms a cycle",
                                graph.name(),
                                names.join(" -> ")
                            ),
                        )
                        .table(table)
                        .constraint(format!("{table} acyclic {}", graph.name())),
                        path,
                        None,
                    ),
                );
            }
        }
        for check in schema.checks() {
            for path in crate::sql::check_violations(&catalog.mirror, schema, check)? {
                verdict.errors.push(
                    located.at(
                        Diagnostic::error(
                            "CHECK_VIOLATION",
                            format!("row violates check {:?}", check.name()),
                        )
                        .table(table)
                        .constraint(check.expr().to_string()),
                        &path,
                        None,
                    ),
                );
            }
        }
        for assertion in schema.assertions() {
            for path in crate::sql::assertion_violations(&catalog.mirror, schema, assertion)? {
                let message = assertion
                    .message()
                    .map(String::from)
                    .unwrap_or_else(|| format!("row violates assertion {:?}", assertion.name()));
                let diagnostic = match assertion.severity() {
                    Severity::Error => Diagnostic::error("ASSERTION_VIOLATION", message),
                    Severity::Warning => Diagnostic::warning("ASSERTION_VIOLATION", message),
                }
                .table(table)
                .constraint(assertion.name().to_string());
                let diagnostic = located.at(diagnostic, &path, None);
                match assertion.severity() {
                    Severity::Error => verdict.errors.push(diagnostic),
                    Severity::Warning => verdict.warnings.push(diagnostic),
                }
            }
        }
    }
    Ok(verdict)
}

/// Every elementary cycle reachable in an acyclic graph, each as its sequence of
/// (key, path), reported once regardless of where it is entered.
fn cycles(
    catalog: &Catalog,
    table: &str,
    edges: &[mirror::Edge],
) -> Result<Vec<Vec<(String, String)>>> {
    // Node identity is the key; an edge goes from the key of the row holding it
    // to the key it names. Keys this table does not hold are not nodes -- a
    // reference to a missing row is the foreign key's concern, not the graph's.
    let mut key_of_path: BTreeMap<String, String> = BTreeMap::new();
    let mut path_of_key: BTreeMap<String, String> = BTreeMap::new();
    catalog.each_row(table, |row| {
        if let Some(schema) = catalog.schemas.get(table)
            && let Some(key) = mirror::key(&row.value, schema.primary_key(), schema)
        {
            let path = crate::catalog::slash(&row.relative);
            key_of_path.insert(path.clone(), key.clone());
            path_of_key.entry(key).or_insert(path);
        }
        Ok(())
    })?;
    let mut graph: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for edge in edges {
        if let Some(from) = key_of_path.get(&edge.path)
            && path_of_key.contains_key(&edge.target)
        {
            graph
                .entry(from.clone())
                .or_default()
                .insert(edge.target.clone());
        }
    }
    const WHITE: u8 = 0;
    const GREY: u8 = 1;
    const BLACK: u8 = 2;
    let mut colour: BTreeMap<&str, u8> = BTreeMap::new();
    let mut reported = BTreeSet::new();
    let mut out = vec![];
    for start in graph.keys() {
        if colour.get(start.as_str()).copied().unwrap_or(WHITE) != WHITE {
            continue;
        }
        let mut stack: Vec<(&str, Vec<&String>, usize)> =
            vec![(start.as_str(), graph[start].iter().collect(), 0)];
        let mut trail: Vec<&str> = vec![start.as_str()];
        colour.insert(start.as_str(), GREY);
        while let Some(top) = stack.last_mut() {
            if top.2 >= top.1.len() {
                let node = top.0;
                colour.insert(node, BLACK);
                stack.pop();
                trail.pop();
                continue;
            }
            let neighbour: &str = top.1[top.2].as_str();
            top.2 += 1;
            match colour.get(neighbour).copied().unwrap_or(WHITE) {
                GREY => {
                    let from = trail.iter().position(|n| *n == neighbour).unwrap_or(0);
                    let mut cycle: Vec<&str> = trail[from..].to_vec();
                    let mut signature = cycle.clone();
                    signature.sort();
                    if reported.insert(signature.join("\u{1f}")) {
                        cycle.push(neighbour);
                        out.push(
                            cycle
                                .into_iter()
                                .map(|key| (key.to_string(), path_of_key[key].clone()))
                                .collect(),
                        );
                    }
                }
                WHITE => {
                    colour.insert(neighbour, GREY);
                    trail.push(neighbour);
                    let next_neighbours = graph
                        .get(neighbour)
                        .map(|set| set.iter().collect())
                        .unwrap_or_default();
                    stack.push((neighbour, next_neighbours, 0));
                }
                _ => {}
            }
        }
    }
    Ok(out)
}

/// Resolves diagnostics to lines and columns, reading each file once.
struct Locator<'s> {
    root: PathBuf,
    source: &'s dyn crate::fs::Source,
    cache: BTreeMap<String, Option<(Vec<u8>, crate::locate::Spans)>>,
}

impl<'s> Locator<'s> {
    fn new(root: &Path, source: &'s dyn crate::fs::Source) -> Self {
        Self {
            root: root.to_path_buf(),
            source,
            cache: BTreeMap::new(),
        }
    }

    fn at(&mut self, diagnostic: Diagnostic, path: &str, pointer: Option<&str>) -> Diagnostic {
        let mut diagnostic = diagnostic.at(path);
        if let Some(pointer) = pointer {
            diagnostic = diagnostic.pointer(pointer.to_string());
        } else if diagnostic.pointer.is_none() {
            diagnostic = diagnostic.pointer(String::new());
        }
        let root = self.root.clone();
        let source = self.source;
        let entry = self.cache.entry(path.to_string()).or_insert_with(|| {
            source.read(&root.join(path)).ok().map(|raw| {
                let spans = crate::locate::Spans::of(&raw);
                (raw, spans)
            })
        });
        match entry {
            Some((raw, spans)) => diagnostic.locate_in(raw, spans),
            None => diagnostic,
        }
    }
}

/// Whether an action could apply to a reference of this shape; used to decide
/// which fixes a violation can be offered.
pub fn action_applies(action: Action, iterates: bool, nullable: bool) -> bool {
    match action {
        Action::Remove => iterates || nullable,
        Action::SetNull => nullable,
        _ => true,
    }
}
