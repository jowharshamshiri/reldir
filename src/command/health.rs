//! Whether the database is valid, how its schemas could be stronger, and what
//! can be repaired.

use super::{Context, Intent, empty};
use crate::{
    db::{Admission, Database, Request},
    diagnostic::{DbError, Result, exit_code_for_diagnostics},
    doctor::{self, Class},
    output::{Finish, Sink},
};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

fn state_name(database: &Database) -> &'static str {
    if !database.is_valid() {
        "INVALID"
    } else if database
        .events
        .iter()
        .any(|event| matches!(event, crate::db::Event::Recorded { origin, .. } if origin == "external" || origin == "recovery"))
    {
        "VALID_CHANGED_EXTERNALLY"
    } else if !database.unrecorded.is_empty() {
        "VALID_UNRECORDED"
    } else {
        "VALID"
    }
}

fn counts<'a>(codes: impl Iterator<Item = &'a str>) -> Value {
    let mut out: BTreeMap<&str, usize> = BTreeMap::new();
    for code in codes {
        *out.entry(code).or_default() += 1;
    }
    json!(out)
}

/// Validity, what changed, and where history stands. Always exits 0 unless the
/// database cannot be read: status reports, `check` judges.
pub fn status(context: &Context, sink: &mut dyn Sink) -> Result<Finish> {
    let Some(database) = context.open(sink, Intent::Read)? else { return Ok(empty()) };
    for diagnostic in database.verdict.errors.iter().chain(&database.verdict.warnings) {
        sink.diagnostic(diagnostic)?;
    }
    for change in &database.unrecorded {
        let mut record = Map::new();
        record.insert("kind".into(), json!("unrecorded_change"));
        record.insert("change".into(), json!(change));
        sink.record(record)?;
    }
    let head = database.history.head();
    let state = state_name(&database);
    let summary = match head {
        Some(head) => format!("{state}   revision {}   root {}", head.revision, &head.new_root_hash[..12]),
        None => format!("{state}   nothing recorded yet"),
    };
    let mut finish = Finish::ok(summary)
        .with("valid", database.is_valid())
        .with("state", state)
        .with("tables", database.catalog.schemas.len())
        .with("rows", database.catalog.row_count()?)
        .with("violations", database.verdict.errors.len());
    if let Some(head) = head {
        finish = finish.with("revision", head.revision).with("root", head.new_root_hash.clone());
    }
    if !database.is_valid() {
        sink.note("run `reldir doctor` for repairs");
    }
    Ok(finish)
}

/// Full validation. Exits 2 when invalid, and -- with `strict` -- 7 when lint
/// has findings.
pub fn check(context: &Context, sink: &mut dyn Sink, strict: bool) -> Result<Finish> {
    let Some(database) = context.open(sink, Intent::Read)? else { return Ok(empty()) };
    for diagnostic in &database.verdict.errors {
        sink.diagnostic(diagnostic)?;
    }
    let findings = if database.is_valid() {
        crate::lint::lint(&database.catalog, &database.config, false)?
    } else {
        vec![]
    };
    if strict {
        for diagnostic in database.verdict.warnings.iter().chain(findings.iter().map(|f| &f.diagnostic)) {
            sink.diagnostic(diagnostic)?;
        }
    } else {
        for warning in &database.verdict.warnings {
            sink.diagnostic(warning)?;
        }
    }
    let valid = database.is_valid();
    let exit = if !valid {
        exit_code_for_diagnostics(&database.verdict.errors)
    } else if strict && (!findings.is_empty() || !database.verdict.warnings.is_empty()) {
        7
    } else {
        0
    };
    let rows = database.catalog.row_count()?;
    let tables = database.catalog.schemas.len();
    let summary = format!(
        "{}: {tables} table(s), {rows} row(s), {} violation(s), {} warning(s), {} lint finding(s), {} ms",
        if valid { "VALID" } else { "INVALID" },
        database.verdict.errors.len(),
        database.verdict.warnings.len(),
        findings.len(),
        database.validation_elapsed.as_millis()
    );
    if !valid {
        sink.note("run `reldir doctor` for repairs, least destructive first");
    }
    Ok(Finish::ok(summary)
        .exit(exit)
        .with("valid", valid)
        .with("state", state_name(&database))
        .with("tables", tables)
        .with("rows", rows)
        .with("violations", counts(database.verdict.errors.iter().map(|d| d.code.as_str())))
        .with("warnings", counts(database.verdict.warnings.iter().map(|d| d.code.as_str())))
        .with("lint", counts(findings.iter().map(|f| f.diagnostic.code.as_str())))
        .with("elapsed_ms", database.validation_elapsed.as_millis() as u64))
}

/// How the schemas could say more than they do.
pub fn lint(context: &Context, sink: &mut dyn Sink, table: Option<&str>, strict: bool, descriptions: bool) -> Result<Finish> {
    let Some(database) = context.open(sink, Intent::Read)? else { return Ok(empty()) };
    if let Some(table) = table {
        super::schema_of(&database, table)?;
    }
    let findings = crate::lint::lint(&database.catalog, &database.config, descriptions)?;
    let shown: Vec<_> = findings
        .iter()
        .filter(|finding| table.is_none_or(|table| finding.diagnostic.table.as_deref() == Some(table)))
        .collect();
    for finding in &shown {
        sink.diagnostic(&finding.diagnostic)?;
    }
    if !database.is_valid() {
        sink.note("the database is invalid; lint judges schemas, and `reldir check` lists the violations");
    }
    Ok(Finish::ok(format!("{} lint finding(s)", shown.len()))
        .exit(if strict && !shown.is_empty() { 7 } else { 0 })
        .with("findings", shown.len())
        .with("valid", database.is_valid()))
}

pub struct DoctorOptions<'a> {
    pub fix: bool,
    pub allow_data: bool,
    pub only: Option<&'a str>,
    pub explain: Option<&'a str>,
    pub no_snapshot: bool,
}

/// Diagnose, and with `fix` repair.
pub fn doctor(context: &Context, sink: &mut dyn Sink, options: DoctorOptions<'_>) -> Result<Finish> {
    if let Some(id) = options.explain {
        let (id, text) = doctor::FIXES
            .iter()
            .find(|(fix, _)| *fix == id)
            .ok_or_else(|| DbError::usage(format!("{id:?} is not a fix; `reldir doctor` lists the fixes that apply")))?;
        let mut record = Map::new();
        record.insert("kind".into(), json!("fix_explanation"));
        record.insert("id".into(), json!(id));
        record.insert("explanation".into(), json!(text));
        sink.record(record)?;
        return Ok(Finish::ok(format!("{id}: {text}")));
    }
    let intent = if options.fix { Intent::Write } else { Intent::Read };
    let Some(mut database) = context.open(sink, intent)? else { return Ok(empty()) };
    let fixes = doctor::plan(&database)?;
    if let Some(only) = options.only
        && !fixes.iter().any(|fix| fix.id == only || fix.resolves == only)
    {
        return Err(DbError::usage(format!("no fix here matches {only:?}")));
    }
    for fix in fixes.iter().filter(|fix| options.only.is_none_or(|only| fix.id == only || fix.resolves == only)) {
        let mut record = fix.to_json(&database.catalog).as_object().cloned().unwrap_or_default();
        record.insert("kind".into(), json!("fix"));
        sink.record(record)?;
    }
    let (chosen, deferred) = doctor::select(&database.catalog, &fixes, options.only, options.allow_data);
    let manual = fixes.iter().filter(|fix| fix.class == Class::Manual).count();
    if !options.fix {
        let data_waiting = fixes.iter().filter(|fix| fix.class == Class::Data && fix.rank == 0).count();
        let mut summary = format!(
            "{} fix(es) would apply, {} need a decision, {} alternative(s) in all",
            chosen.len(),
            manual,
            fixes.len()
        );
        if !options.allow_data && data_waiting > 0 {
            summary.push_str(&format!("; {data_waiting} rewrite rows and need --allow-data"));
        }
        sink.note("apply with `reldir doctor --fix`; choose an alternative with --only <FIX_ID>");
        return Ok(Finish::ok(summary)
            .with("valid", database.is_valid())
            .with("fixes", fixes.len())
            .with("applicable", chosen.len())
            .with("manual", manual));
    }
    if chosen.is_empty() {
        return Ok(Finish::ok("nothing to apply").with("valid", database.is_valid()).with("manual", manual));
    }
    let destructive: Vec<&&doctor::Fix> = chosen.iter().filter(|fix| fix.destructive).collect();
    if !destructive.is_empty() {
        super::confirm(
            context,
            &format!(
                "{} of the fixes remove data ({}); apply them",
                destructive.len(),
                destructive.iter().map(|fix| fix.id).collect::<Vec<_>>().join(", ")
            ),
        )?;
    }
    let rewrites = chosen.iter().any(|fix| matches!(fix.class, Class::Data | Class::Layout));
    if rewrites && !options.no_snapshot && !context.dry_run {
        let revision = database.history.head().map_or(0, |head| head.revision);
        let mut name = format!("pre-doctor-{revision}");
        let mut attempt = 1;
        while crate::snapshot::exists(&database.root, &name) {
            attempt += 1;
            name = format!("pre-doctor-{revision}-{attempt}");
        }
        crate::snapshot::create(&database, &name)?;
        sink.event(
            "snapshot",
            json!({"name": name, "action": "created"}),
            &format!("snapshot {name} taken; `reldir snapshot restore {name} --yes` puts everything back"),
        )?;
    }
    let (rows, files) = doctor::changes(&database, &chosen)?;
    let outcome = database.change(
        rows,
        files,
        &crate::db::Expected::new(),
        Request { origin: "repair", admission: Admission::NoNewFaults, dry_run: context.dry_run },
    )?;
    for fix in &deferred {
        sink.note(&format!("{} deferred: it touches a file another fix changes; run doctor again", fix.id));
    }
    let applied: Vec<&str> = chosen.iter().map(|fix| fix.id).collect();
    let mut finish = super::committed(sink, &outcome, "repaired")?;
    finish.summary = format!("{} ({} fix(es): {})", finish.summary, applied.len(), applied.join(", "));
    Ok(finish.with("applied", applied.len()).with("deferred", deferred.len()).with("valid", database.is_valid()))
}
