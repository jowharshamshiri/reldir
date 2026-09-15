use crate::{
    canonical,
    catalog::Catalog,
    diagnostic::{DbError, Diagnostic, Result},
    schema::{Action, ColumnType},
    transaction::Change,
};
use rusqlite::{
    Connection,
    types::{Value as SqlValue, ValueRef},
};
use serde_json::{Map, Number, Value};
use sqlparser::{
    ast::{BinaryOperator, Expr, SelectItem, SetExpr, Statement, UnaryOperator},
    dialect::GenericDialect,
    parser::Parser,
};
use std::{collections::BTreeMap, path::PathBuf};

pub struct SqlResult {
    pub rows: Vec<Map<String, Value>>,
    pub changes: Vec<Change>,
    pub mutation: bool,
}
#[derive(Clone)]
pub struct SqlParam {
    pub name: Option<String>,
    pub value: Value,
}

#[derive(Clone, Copy)]
pub struct QueryLimits {
    pub timeout: Option<std::time::Duration>,
    pub max_rows: usize,
    pub max_memory: u64,
    pub max_sort_memory: u64,
    pub max_temporary_disk: u64,
}

impl QueryLimits {
    fn unbounded(timeout: Option<std::time::Duration>) -> Self {
        Self {
            timeout,
            max_rows: usize::MAX,
            max_memory: u64::MAX,
            max_sort_memory: u64::MAX,
            max_temporary_disk: u64::MAX,
        }
    }
}

pub fn execute(catalog: &Catalog, text: &str, params: &[Value]) -> Result<SqlResult> {
    let params: Vec<_> = params
        .iter()
        .cloned()
        .map(|value| SqlParam { name: None, value })
        .collect();
    execute_params(catalog, text, &params)
}

pub fn execute_with_limits(
    catalog: &Catalog,
    text: &str,
    params: &[Value],
    limits: QueryLimits,
) -> Result<SqlResult> {
    let params = params
        .iter()
        .cloned()
        .map(|value| SqlParam { name: None, value })
        .collect::<Vec<_>>();
    execute_params_with_limits(catalog, text, &params, limits)
}

pub fn execute_params(catalog: &Catalog, text: &str, params: &[SqlParam]) -> Result<SqlResult> {
    execute_params_with_limits(catalog, text, params, QueryLimits::unbounded(None))
}
pub fn execute_params_timeout(
    catalog: &Catalog,
    text: &str,
    params: &[SqlParam],
    timeout: Option<std::time::Duration>,
) -> Result<SqlResult> {
    execute_params_with_limits(catalog, text, params, QueryLimits::unbounded(timeout))
}
pub fn execute_params_with_limits(
    catalog: &Catalog,
    text: &str,
    params: &[SqlParam],
    limits: QueryLimits,
) -> Result<SqlResult> {
    let token = statement_kind(text)?;
    enforce_query_workspace(catalog, text, &limits)?;
    let mut conn = load(catalog, true)?;
    if let Some(limit) = limits.timeout {
        let start = std::time::Instant::now();
        conn.progress_handler(1000, Some(move || start.elapsed() >= limit));
    }
    if matches!(token.as_str(), "SELECT" | "WITH" | "EXPLAIN") {
        let rows = query(&conn, text, params, limits.max_rows, limits.max_memory)?;
        return Ok(SqlResult {
            rows,
            changes: vec![],
            mutation: false,
        });
    }
    if !matches!(token.as_str(), "INSERT" | "UPDATE" | "DELETE") {
        return Err(DbError::new(
            "QUERY_UNSUPPORTED",
            format!("unsupported SQL statement {token}"),
            4,
        ));
    }
    let existing_rowids = snapshot_rowids(&conn, catalog)?;
    execute_bound(&mut conn, text, params)?;
    let after = dump(&conn, catalog, &existing_rowids)?;
    let changes = diff(catalog, &after)?;
    Ok(SqlResult {
        rows: vec![],
        changes,
        mutation: true,
    })
}

pub fn is_read_statement(text: &str) -> Result<bool> {
    Ok(matches!(
        statement_kind(text)?.as_str(),
        "SELECT" | "WITH" | "EXPLAIN"
    ))
}

pub fn query_each_timeout<F>(
    catalog: &Catalog,
    text: &str,
    params: &[SqlParam],
    limits: QueryLimits,
    mut emit: F,
) -> Result<usize>
where
    F: FnMut(Map<String, Value>) -> Result<()>,
{
    if !is_read_statement(text)? {
        return Err(DbError::new(
            "QUERY_UNSUPPORTED",
            "streaming execution requires a read-only SQL statement",
            4,
        ));
    }
    enforce_query_workspace(catalog, text, &limits)?;
    let conn = load(catalog, true)?;
    if let Some(limit) = limits.timeout {
        let start = std::time::Instant::now();
        conn.progress_handler(1000, Some(move || start.elapsed() >= limit));
    }
    let mut statement = conn.prepare(text).map_err(query_err)?;
    bind(&mut statement, params)?;
    let count = statement.column_count();
    let names: Vec<_> = (0..count)
        .map(|index| statement.column_name(index).unwrap_or("").to_string())
        .collect();
    let declared: Vec<_> = statement
        .columns()
        .into_iter()
        .map(|column| column.decl_type().map(String::from))
        .collect();
    let mut cursor = statement.raw_query();
    let mut emitted = 0usize;
    while let Some(row) = cursor.next().map_err(query_err)? {
        if emitted == limits.max_rows {
            return Err(DbError::new(
                "RESOURCE_LIMIT",
                format!(
                    "query exceeds the configured limit of {} result rows",
                    limits.max_rows
                ),
                4,
            ));
        }
        let mut object = Map::new();
        for (index, name) in names.iter().enumerate() {
            let value = from_ref(row.get_ref(index).map_err(query_err)?);
            object.insert(
                name.clone(),
                decode_declared(value, declared[index].as_deref())?,
            );
        }
        let row_size = serde_json::to_vec(&object)
            .map_err(|error| DbError::new("QUERY_TYPE_ERROR", error.to_string(), 4))?
            .len() as u64;
        if row_size > limits.max_memory {
            return Err(DbError::new(
                "RESOURCE_LIMIT",
                format!(
                    "one result row exceeds the configured {} byte query-memory limit",
                    limits.max_memory
                ),
                4,
            ));
        }
        emit(object)?;
        emitted += 1;
    }
    Ok(emitted)
}

fn statement_kind(text: &str) -> Result<String> {
    let ast = Parser::parse_sql(&GenericDialect {}, text).map_err(|error| {
        let message = error.to_string();
        let mut diagnostic = Diagnostic::error("QUERY_UNSUPPORTED", &message);
        diagnostic.location = sql_error_location(&message);
        diagnostic.source_line = diagnostic
            .location
            .as_ref()
            .and_then(|location| text.lines().nth(location.line.saturating_sub(1)))
            .map(String::from);
        DbError::from_diag(diagnostic, 4)
    })?;
    if ast.len() != 1 {
        return Err(DbError::new(
            "QUERY_UNSUPPORTED",
            "exactly one SQL statement is required",
            4,
        ));
    }
    Ok(match &ast[0] {
        Statement::Query(query) => match query.body.as_ref() {
            SetExpr::Insert(_) => "INSERT".into(),
            SetExpr::Update(_) => "UPDATE".into(),
            SetExpr::Delete(_) => "DELETE".into(),
            _ => "SELECT".into(),
        },
        Statement::Insert(_) => "INSERT".into(),
        Statement::Update { .. } => "UPDATE".into(),
        Statement::Delete(_) => "DELETE".into(),
        Statement::Explain { .. } | Statement::ExplainTable { .. } => "EXPLAIN".into(),
        statement => statement
            .to_string()
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase(),
    })
}

fn sql_error_location(message: &str) -> Option<crate::diagnostic::Location> {
    let marker = "Line: ";
    let start = message.find(marker)? + marker.len();
    let rest = &message[start..];
    let (line, rest) = rest.split_once(", Column: ")?;
    let column = rest
        .split(|character: char| !character.is_ascii_digit())
        .next()?;
    Some(crate::diagnostic::Location {
        line: line.parse().ok()?,
        column: column.parse().ok()?,
    })
}

fn enforce_query_workspace(catalog: &Catalog, text: &str, limits: &QueryLimits) -> Result<()> {
    if limits.max_memory == 0 || limits.max_sort_memory == 0 || limits.max_temporary_disk == 0 {
        return Err(DbError::new(
            "RESOURCE_LIMIT",
            "query resource limits must be greater than zero",
            4,
        ));
    }
    let workspace = catalog
        .rows
        .values()
        .flatten()
        .try_fold(0u64, |total, row| {
            total
                .checked_add(row.raw.len() as u64)
                .ok_or_else(|| DbError::new("RESOURCE_LIMIT", "query workspace size overflow", 4))
        })?;
    if workspace > limits.max_memory {
        return Err(DbError::new(
            "RESOURCE_LIMIT",
            format!(
                "query requires {workspace} bytes of relational workspace, exceeding the configured {} byte query-memory limit",
                limits.max_memory
            ),
            4,
        ));
    }
    let normalized = normalized_statement(text)?.to_ascii_uppercase();
    let may_sort = normalized.contains(" ORDER BY ")
        || normalized.contains(" GROUP BY ")
        || normalized.contains(" DISTINCT ")
        || normalized.starts_with("SELECT DISTINCT ");
    if may_sort && workspace > limits.max_sort_memory {
        return Err(DbError::new(
            "RESOURCE_LIMIT",
            format!(
                "sort may require {workspace} bytes, exceeding the configured {} byte sort-memory limit",
                limits.max_sort_memory
            ),
            4,
        ));
    }
    // SQLite temporary storage is forced to memory for this ephemeral engine,
    // so query execution consumes zero temporary-disk bytes.
    Ok(())
}

fn load(c: &Catalog, enforce: bool) -> Result<Connection> {
    let conn = Connection::open_in_memory().map_err(query_err)?;
    conn.create_collation("JDB_DECIMAL", |left, right| {
        match crate::value::compare_decimal(left, right) {
            Some(ordering) => ordering,
            // Invalid decimal text cannot occur in a valid catalog. Retaining a
            // total order here keeps SQLite's collation callback infallible.
            None => left.cmp(right),
        }
    })
    .map_err(query_err)?;
    conn.execute_batch(
        "PRAGMA foreign_keys=OFF; PRAGMA case_sensitive_like=ON; PRAGMA temp_store=MEMORY;",
    )
    .map_err(query_err)?;
    for (table, s) in &c.schemas {
        let mut defs: Vec<_> = s
            .columns
            .iter()
            .map(|(n, col)| {
                let mut definition = format!("{} {}", q(n), sqlite_type(&col.kind));
                if !col.nullable && col.generated.is_none() {
                    definition.push_str(" NOT NULL");
                }
                if let Some(default) = &col.default {
                    definition.push_str(" DEFAULT ");
                    definition.push_str(&sql_literal(&to_sql(default, col)));
                }
                definition
            })
            .collect();
        if enforce {
            defs.push(format!(
                "PRIMARY KEY ({})",
                s.primary_key
                    .iter()
                    .map(|x| q(x))
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            defs.extend(s.unique.iter().map(|u| {
                format!(
                    "UNIQUE ({})",
                    u.iter().map(|x| q(x)).collect::<Vec<_>>().join(",")
                )
            }));
            for fk in &s.foreign_keys {
                defs.push(format!(
                    "FOREIGN KEY ({}) REFERENCES {} ({}) ON DELETE {} ON UPDATE {}",
                    fk.columns
                        .iter()
                        .map(|x| q(x))
                        .collect::<Vec<_>>()
                        .join(","),
                    q(&fk.references.table),
                    fk.references
                        .columns
                        .iter()
                        .map(|x| q(x))
                        .collect::<Vec<_>>()
                        .join(","),
                    action(fk.delete_action()),
                    action(fk.update_action())
                ));
            }
            for check in &s.check {
                defs.push(format!(
                    "CONSTRAINT {} CHECK ({})",
                    q(&check.name),
                    check.expr
                ));
            }
        }
        conn.execute(
            &format!("CREATE TABLE {} ({})", q(table), defs.join(",")),
            [],
        )
        .map_err(query_err)?;
    }
    for (table, rows) in &c.rows {
        let s = &c.schemas[table];
        let names: Vec<_> = s.columns.keys().collect();
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            q(table),
            names.iter().map(|x| q(x)).collect::<Vec<_>>().join(","),
            vec!["?"; names.len()].join(",")
        );
        for row in rows {
            let vals: Vec<_> = names
                .iter()
                .map(|n| {
                    to_sql(
                        row.value
                            .get(*n)
                            .or(s.columns[*n].default.as_ref())
                            .unwrap_or(&Value::Null),
                        &s.columns[*n],
                    )
                })
                .collect();
            conn.execute(&sql, rusqlite::params_from_iter(vals))
                .map_err(query_err)?;
        }
    }
    for (table, schema) in &c.schemas {
        for columns in &schema.indexes {
            conn.execute(
                &format!(
                    "CREATE INDEX {} ON {} ({})",
                    q(&index_name(table, columns)),
                    q(table),
                    columns
                        .iter()
                        .map(|column| q(column))
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                [],
            )
            .map_err(query_err)?;
        }
    }
    if enforce {
        conn.execute_batch("PRAGMA foreign_keys=ON;")
            .map_err(query_err)?
    }
    Ok(conn)
}
fn query(
    conn: &Connection,
    text: &str,
    params: &[SqlParam],
    max_rows: usize,
    max_memory: u64,
) -> Result<Vec<Map<String, Value>>> {
    let mut st = conn.prepare(text).map_err(query_err)?;
    bind(&mut st, params)?;
    let count = st.column_count();
    let names: Vec<_> = (0..count)
        .map(|i| st.column_name(i).unwrap_or("").to_string())
        .collect();
    let declared: Vec<_> = st
        .columns()
        .into_iter()
        .map(|column| column.decl_type().map(String::from))
        .collect();
    let mut cursor = st.raw_query();
    let mut out = vec![];
    let mut memory = 0u64;
    while let Some(row) = cursor.next().map_err(query_err)? {
        if out.len() == max_rows {
            return Err(DbError::new(
                "RESOURCE_LIMIT",
                format!("query exceeds the configured limit of {max_rows} result rows"),
                4,
            ));
        }
        let mut obj = Map::new();
        for (i, n) in names.iter().enumerate() {
            let value = from_ref(row.get_ref(i).map_err(query_err)?);
            obj.insert(n.clone(), decode_declared(value, declared[i].as_deref())?);
        }
        memory = memory
            .checked_add(
                serde_json::to_vec(&obj)
                    .map_err(|error| DbError::new("QUERY_TYPE_ERROR", error.to_string(), 4))?
                    .len() as u64,
            )
            .ok_or_else(|| DbError::new("RESOURCE_LIMIT", "query-memory accounting overflow", 4))?;
        if memory > max_memory {
            return Err(DbError::new(
                "RESOURCE_LIMIT",
                format!("query results exceed the configured {max_memory} byte memory limit"),
                4,
            ));
        }
        out.push(obj)
    }
    Ok(out)
}
fn execute_bound(conn: &mut Connection, text: &str, params: &[SqlParam]) -> Result<usize> {
    let mut st = conn.prepare(text).map_err(query_err)?;
    bind(&mut st, params)?;
    st.raw_execute().map_err(query_err)
}
fn bind(st: &mut rusqlite::Statement<'_>, params: &[SqlParam]) -> Result<()> {
    if st.parameter_count() != params.len() {
        return Err(DbError::new(
            "QUERY_TYPE_ERROR",
            format!(
                "query requires {} parameters but {} were supplied",
                st.parameter_count(),
                params.len()
            ),
            4,
        ));
    }
    let mut used = std::collections::BTreeSet::new();
    let mut positional = 1;
    for p in params {
        let index = if let Some(name) = &p.name {
            [format!(":{name}"), format!("@{name}"), format!("${name}")]
                .iter()
                .find_map(|n| st.parameter_index(n).ok().flatten())
                .ok_or_else(|| {
                    DbError::new(
                        "QUERY_TYPE_ERROR",
                        format!("query has no parameter named {name:?}"),
                        4,
                    )
                })?
        } else {
            while used.contains(&positional) {
                positional += 1
            }
            let i = positional;
            positional += 1;
            i
        };
        used.insert(index);
        st.raw_bind_parameter(index, to_sql_generic(&p.value))
            .map_err(query_err)?
    }
    Ok(())
}

fn snapshot_rowids(
    conn: &Connection,
    catalog: &Catalog,
) -> Result<BTreeMap<String, std::collections::BTreeSet<i64>>> {
    let mut all = BTreeMap::new();
    for table in catalog.schemas.keys() {
        let mut statement = conn
            .prepare(&format!("SELECT rowid FROM {}", q(table)))
            .map_err(query_err)?;
        let ids = statement
            .query_map([], |row| row.get::<_, i64>(0))
            .map_err(query_err)?
            .collect::<std::result::Result<std::collections::BTreeSet<_>, _>>()
            .map_err(query_err)?;
        all.insert(table.clone(), ids);
    }
    Ok(all)
}

fn dump(
    conn: &Connection,
    c: &Catalog,
    existing_rowids: &BTreeMap<String, std::collections::BTreeSet<i64>>,
) -> Result<BTreeMap<String, Vec<Map<String, Value>>>> {
    let mut all = BTreeMap::new();
    for (table, s) in &c.schemas {
        let names: Vec<_> = s.columns.keys().cloned().collect();
        let sql = format!(
            "SELECT rowid, {} FROM {}",
            names.iter().map(|x| q(x)).collect::<Vec<_>>().join(","),
            q(table)
        );
        let mut st = conn.prepare(&sql).map_err(query_err)?;
        let mut cursor = st.query([]).map_err(query_err)?;
        let mut rows = vec![];
        let mut next_sequence = s
            .columns
            .iter()
            .filter(|(_, column)| {
                column.generated.as_ref().is_some_and(|generated| {
                    matches!(generated.kind, crate::schema::GeneratedKind::Sequence)
                })
            })
            .flat_map(|(name, _)| {
                c.rows[table]
                    .iter()
                    .filter_map(move |row| row.value.get(name).and_then(Value::as_i64))
            })
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| DbError::new("RESOURCE_LIMIT", "generated sequence exhausted i64", 2))?;
        while let Some(r) = cursor.next().map_err(query_err)? {
            let rowid = r.get::<_, i64>(0).map_err(query_err)?;
            let existing = existing_rowids[table].contains(&rowid);
            let mut obj = Map::new();
            for (i, n) in names.iter().enumerate() {
                let mut v = from_ref(r.get_ref(i + 1).map_err(query_err)?);
                let col = &s.columns[n];
                if v.is_null() {
                    if !existing && let Some(generated) = &col.generated {
                        let sequence = next_sequence;
                        if matches!(generated.kind, crate::schema::GeneratedKind::Sequence) {
                            next_sequence = next_sequence.checked_add(1).ok_or_else(|| {
                                DbError::new(
                                    "RESOURCE_LIMIT",
                                    "generated sequence exhausted i64",
                                    2,
                                )
                            })?;
                        }
                        if let Some(generated) = crate::value::generate(col, sequence) {
                            v = generated;
                        }
                    }
                } else {
                    v = decode(v, col)?;
                }
                obj.insert(n.clone(), v);
            }
            rows.push(obj)
        }
        all.insert(table.clone(), rows);
    }
    Ok(all)
}
fn diff(c: &Catalog, after: &BTreeMap<String, Vec<Map<String, Value>>>) -> Result<Vec<Change>> {
    let mut out = vec![];
    for (table, s) in &c.schemas {
        let mut old: BTreeMap<String, (&Map<String, Value>, PathBuf)> = BTreeMap::new();
        for r in &c.rows[table] {
            if let Some(k) = crate::integrity::key(&r.value, &s.primary_key, s) {
                old.insert(k, (&r.value, r.relative.clone()));
            }
        }
        let mut new = BTreeMap::new();
        for r in &after[table] {
            let k = crate::integrity::key(r, &s.primary_key, s).ok_or_else(|| {
                DbError::new(
                    "NOT_NULL_VIOLATION",
                    "SQL mutation produced a null primary key",
                    2,
                )
            })?;
            new.insert(k, r);
        }
        for (k, (_, path)) in &old {
            if !new.contains_key(k) {
                out.push(Change::Delete { path: path.clone() })
            }
        }
        for (k, row) in new {
            let path = PathBuf::from(table).join(canonical::filename(s, row).ok_or_else(|| {
                DbError::new("IDENTITY_MISMATCH", "cannot derive row filename", 2)
            })?);
            match old.get(&k) {
                Some((oldrow, oldpath)) if *oldrow == row && *oldpath == path => {}
                Some((_, oldpath)) => {
                    if *oldpath != path {
                        out.push(Change::Delete {
                            path: oldpath.clone(),
                        })
                    }
                    out.push(Change::Write {
                        path,
                        bytes: canonical::pretty_with_indent(
                            &canonical::canonical_row(row, s),
                            c.indentation_width,
                        ),
                    })
                }
                None => out.push(Change::Write {
                    path,
                    bytes: canonical::pretty_with_indent(
                        &canonical::canonical_row(row, s),
                        c.indentation_width,
                    ),
                }),
            }
        }
    }
    Ok(out)
}

fn to_sql(v: &Value, c: &crate::schema::Column) -> SqlValue {
    if v.is_null() {
        return SqlValue::Null;
    }
    if !crate::value::matches_column(v, c) {
        return to_sql_generic(v);
    }
    match c.kind {
        ColumnType::Bool => v
            .as_bool()
            .map(|value| SqlValue::Integer(i64::from(value)))
            .unwrap_or_else(|| to_sql_generic(v)),
        ColumnType::Int => v
            .as_i64()
            .map(SqlValue::Integer)
            .unwrap_or_else(|| to_sql_generic(v)),
        ColumnType::Float => v
            .as_f64()
            .map(SqlValue::Real)
            .unwrap_or_else(|| to_sql_generic(v)),
        ColumnType::Array | ColumnType::Object | ColumnType::Json => {
            SqlValue::Text(canonical::compact(v))
        }
        ColumnType::Timestamp => crate::value::textual(v, c)
            .map(SqlValue::Text)
            .unwrap_or_else(|| to_sql_generic(v)),
        _ => v
            .as_str()
            .map(|value| SqlValue::Text(value.into()))
            .unwrap_or_else(|| to_sql_generic(v)),
    }
}
fn to_sql_generic(v: &Value) -> SqlValue {
    match v {
        Value::Null => SqlValue::Null,
        Value::Bool(x) => SqlValue::Integer(*x as i64),
        Value::Number(n) => n
            .as_i64()
            .map(SqlValue::Integer)
            .or_else(|| n.as_f64().map(SqlValue::Real))
            .unwrap_or(SqlValue::Null),
        Value::String(s) => SqlValue::Text(s.clone()),
        _ => SqlValue::Text(canonical::compact(v)),
    }
}
fn from_ref(v: ValueRef<'_>) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(x) => Value::Number(x.into()),
        ValueRef::Real(x) => Number::from_f64(x)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ValueRef::Text(x) => Value::String(String::from_utf8_lossy(x).into()),
        ValueRef::Blob(x) => Value::String(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            x,
        )),
    }
}
fn decode(v: Value, c: &crate::schema::Column) -> Result<Value> {
    if v.is_null() {
        return Ok(v);
    }
    let decoded = match c.kind {
        ColumnType::Bool => match v.as_i64() {
            Some(0) => Value::Bool(false),
            Some(1) => Value::Bool(true),
            _ => {
                return Err(DbError::new(
                    "TYPE_MISMATCH",
                    "SQLite returned a non-boolean value for a bool column",
                    4,
                ));
            }
        },
        ColumnType::Array | ColumnType::Object | ColumnType::Json => {
            let text = v.as_str().ok_or_else(|| {
                DbError::new(
                    "TYPE_MISMATCH",
                    "SQLite returned non-text encoded structured JSON",
                    4,
                )
            })?;
            crate::json::parse_str(text).map_err(|error| {
                DbError::new(
                    "TYPE_MISMATCH",
                    format!("SQLite returned invalid encoded JSON: {error}"),
                    4,
                )
            })?
        }
        _ => v,
    };
    if crate::value::matches_column(&decoded, c) {
        Ok(decoded)
    } else {
        Err(DbError::new(
            "TYPE_MISMATCH",
            "SQLite returned a value incompatible with its declared column type",
            4,
        ))
    }
}
fn decode_declared(v: Value, declared: Option<&str>) -> Result<Value> {
    if v.is_null() {
        return Ok(v);
    }
    match declared {
        Some("JDB_BLOB_BOOL") => match v.as_i64() {
            Some(0) => Ok(Value::Bool(false)),
            Some(1) => Ok(Value::Bool(true)),
            _ => Err(DbError::new(
                "QUERY_TYPE_ERROR",
                "SQLite returned a non-boolean value for a bool result column",
                4,
            )),
        },
        Some(kind @ ("JDB_BLOB_ARRAY" | "JDB_BLOB_OBJECT" | "JDB_BLOB_JSON")) => {
            let text = v.as_str().ok_or_else(|| {
                DbError::new(
                    "QUERY_TYPE_ERROR",
                    format!("SQLite returned non-text encoded data for {kind}"),
                    4,
                )
            })?;
            let decoded = crate::json::parse_str(text).map_err(|error| {
                DbError::new(
                    "QUERY_TYPE_ERROR",
                    format!("SQLite returned invalid encoded JSON: {error}"),
                    4,
                )
            })?;
            let correct_shape = match kind {
                "JDB_BLOB_ARRAY" => decoded.is_array(),
                "JDB_BLOB_OBJECT" => decoded.is_object(),
                _ => true,
            };
            if correct_shape {
                Ok(decoded)
            } else {
                Err(DbError::new(
                    "QUERY_TYPE_ERROR",
                    format!("SQLite returned the wrong JSON shape for {kind}"),
                    4,
                ))
            }
        }
        _ => Ok(v),
    }
}
fn sqlite_type(kind: &ColumnType) -> &'static str {
    match kind {
        // BLOB affinity deliberately preserves the storage class supplied by
        // SQL and bound parameters. SQLite's numeric/text affinities otherwise
        // coerce values silently before jdb can enforce its strict type system.
        ColumnType::Bool => "JDB_BLOB_BOOL",
        ColumnType::Int => "JDB_BLOB_I64",
        ColumnType::Float => "JDB_BLOB_F64",
        ColumnType::Decimal => "JDB_BLOB_DECIMAL COLLATE JDB_DECIMAL",
        ColumnType::String => "JDB_BLOB_STRING",
        ColumnType::Bytes => "JDB_BLOB_BYTES",
        ColumnType::Date => "JDB_BLOB_DATE",
        ColumnType::Timestamp => "JDB_BLOB_TIMESTAMP",
        ColumnType::Uuid => "JDB_BLOB_UUID",
        ColumnType::Ulid => "JDB_BLOB_ULID",
        ColumnType::Enum => "JDB_BLOB_ENUM",
        ColumnType::Array => "JDB_BLOB_ARRAY",
        ColumnType::Object => "JDB_BLOB_OBJECT",
        ColumnType::Json => "JDB_BLOB_JSON",
    }
}
fn sql_literal(value: &SqlValue) -> String {
    match value {
        SqlValue::Null => "NULL".into(),
        SqlValue::Integer(value) => value.to_string(),
        SqlValue::Real(value) => value.to_string(),
        SqlValue::Text(value) => format!("'{}'", value.replace('\'', "''")),
        SqlValue::Blob(value) => format!("X'{}'", hex::encode(value)),
    }
}
fn q(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
pub fn index_name(table: &str, columns: &[String]) -> String {
    let mut identity = String::new();
    for column in columns {
        identity.push_str(&column.len().to_string());
        identity.push(':');
        identity.push_str(column);
        identity.push(';');
    }
    format!(
        "jdb_{table}_{}",
        &crate::canonical::hash_bytes(identity.as_bytes())[..12]
    )
}

pub fn normalized_statement(text: &str) -> Result<String> {
    let ast = Parser::parse_sql(&GenericDialect {}, text).map_err(|error| {
        DbError::from_diag(Diagnostic::error("QUERY_UNSUPPORTED", error.to_string()), 4)
    })?;
    if ast.len() != 1 {
        return Err(DbError::new(
            "QUERY_UNSUPPORTED",
            "exactly one SQL statement is required",
            4,
        ));
    }
    Ok(ast[0].to_string())
}

pub fn check_expression_is_boolean(schema: &crate::schema::Schema, expression: &str) -> bool {
    let wrapper = format!("SELECT ({expression}) FROM {}", q(&schema.table));
    let Ok(statements) = Parser::parse_sql(&GenericDialect {}, &wrapper) else {
        return false;
    };
    let Some(Statement::Query(query)) = statements.first() else {
        return false;
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return false;
    };
    let Some(SelectItem::UnnamedExpr(expression)) = select.projection.first() else {
        return false;
    };
    boolean_expression(schema, expression)
}

fn boolean_expression(schema: &crate::schema::Schema, expression: &Expr) -> bool {
    expression_kind(schema, expression) == Some(ExpressionKind::Bool)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExpressionKind {
    Bool,
    Numeric,
    Text,
    Json,
    Null,
}

fn expression_kind(schema: &crate::schema::Schema, expression: &Expr) -> Option<ExpressionKind> {
    match expression {
        Expr::Identifier(identifier) => schema
            .columns
            .get(&identifier.value)
            .map(|column| column_expression_kind(&column.kind)),
        Expr::CompoundIdentifier(parts) => {
            if parts.len() == 2 && parts[0].value != schema.table {
                return None;
            }
            parts
                .last()
                .and_then(|identifier| schema.columns.get(&identifier.value))
                .map(|column| column_expression_kind(&column.kind))
        }
        Expr::Value(value) => match &value.value {
            sqlparser::ast::Value::Boolean(_) => Some(ExpressionKind::Bool),
            sqlparser::ast::Value::Number(_, _) => Some(ExpressionKind::Numeric),
            sqlparser::ast::Value::Null => Some(ExpressionKind::Null),
            sqlparser::ast::Value::SingleQuotedString(_)
            | sqlparser::ast::Value::DoubleQuotedString(_)
            | sqlparser::ast::Value::TripleSingleQuotedString(_)
            | sqlparser::ast::Value::TripleDoubleQuotedString(_)
            | sqlparser::ast::Value::EscapedStringLiteral(_)
            | sqlparser::ast::Value::NationalStringLiteral(_)
            | sqlparser::ast::Value::HexStringLiteral(_)
            | sqlparser::ast::Value::SingleQuotedByteStringLiteral(_)
            | sqlparser::ast::Value::DoubleQuotedByteStringLiteral(_)
            | sqlparser::ast::Value::TripleSingleQuotedByteStringLiteral(_)
            | sqlparser::ast::Value::TripleDoubleQuotedByteStringLiteral(_) => {
                Some(ExpressionKind::Text)
            }
            _ => None,
        },
        Expr::Nested(expression) => expression_kind(schema, expression),
        Expr::UnaryOp { op, expr } => match op {
            UnaryOperator::Not | UnaryOperator::BangNot
                if expression_kind(schema, expr) == Some(ExpressionKind::Bool) =>
            {
                Some(ExpressionKind::Bool)
            }
            UnaryOperator::Plus | UnaryOperator::Minus
                if expression_kind(schema, expr) == Some(ExpressionKind::Numeric) =>
            {
                Some(ExpressionKind::Numeric)
            }
            _ => None,
        },
        Expr::BinaryOp { left, op, right } => {
            let left = expression_kind(schema, left)?;
            let right = expression_kind(schema, right)?;
            match op {
                BinaryOperator::And | BinaryOperator::Or | BinaryOperator::Xor
                    if left == ExpressionKind::Bool && right == ExpressionKind::Bool =>
                {
                    Some(ExpressionKind::Bool)
                }
                BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Spaceship
                    if comparable_kinds(left, right) =>
                {
                    Some(ExpressionKind::Bool)
                }
                BinaryOperator::Plus
                | BinaryOperator::Minus
                | BinaryOperator::Multiply
                | BinaryOperator::Divide
                | BinaryOperator::Modulo
                    if left == ExpressionKind::Numeric && right == ExpressionKind::Numeric =>
                {
                    Some(ExpressionKind::Numeric)
                }
                BinaryOperator::StringConcat
                    if left == ExpressionKind::Text && right == ExpressionKind::Text =>
                {
                    Some(ExpressionKind::Text)
                }
                _ => None,
            }
        }
        Expr::IsFalse(inner)
        | Expr::IsNotFalse(inner)
        | Expr::IsTrue(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsUnknown(inner)
        | Expr::IsNotUnknown(inner)
            if expression_kind(schema, inner) == Some(ExpressionKind::Bool) =>
        {
            Some(ExpressionKind::Bool)
        }
        Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            expression_kind(schema, inner).map(|_| ExpressionKind::Bool)
        }
        Expr::IsDistinctFrom(left, right) | Expr::IsNotDistinctFrom(left, right) => {
            let left = expression_kind(schema, left)?;
            let right = expression_kind(schema, right)?;
            comparable_kinds(left, right).then_some(ExpressionKind::Bool)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            let value = expression_kind(schema, expr)?;
            let low = expression_kind(schema, low)?;
            let high = expression_kind(schema, high)?;
            (comparable_kinds(value, low) && comparable_kinds(value, high))
                .then_some(ExpressionKind::Bool)
        }
        Expr::InList { expr, list, .. } => {
            let value = expression_kind(schema, expr)?;
            list.iter()
                .map(|item| expression_kind(schema, item))
                .collect::<Option<Vec<_>>>()?
                .into_iter()
                .all(|item| comparable_kinds(value, item))
                .then_some(ExpressionKind::Bool)
        }
        Expr::Like { expr, pattern, .. }
        | Expr::ILike { expr, pattern, .. }
        | Expr::SimilarTo { expr, pattern, .. }
        | Expr::RLike { expr, pattern, .. }
            if expression_kind(schema, expr) == Some(ExpressionKind::Text)
                && expression_kind(schema, pattern) == Some(ExpressionKind::Text) =>
        {
            Some(ExpressionKind::Bool)
        }
        _ => None,
    }
}

fn column_expression_kind(kind: &ColumnType) -> ExpressionKind {
    match kind {
        ColumnType::Bool => ExpressionKind::Bool,
        ColumnType::Int | ColumnType::Float | ColumnType::Decimal => ExpressionKind::Numeric,
        ColumnType::String
        | ColumnType::Bytes
        | ColumnType::Date
        | ColumnType::Timestamp
        | ColumnType::Uuid
        | ColumnType::Ulid
        | ColumnType::Enum => ExpressionKind::Text,
        ColumnType::Array | ColumnType::Object | ColumnType::Json => ExpressionKind::Json,
    }
}

fn comparable_kinds(left: ExpressionKind, right: ExpressionKind) -> bool {
    left == ExpressionKind::Null || right == ExpressionKind::Null || left == right
}
fn action(a: Action) -> &'static str {
    match a {
        Action::Restrict => "RESTRICT",
        Action::Cascade => "CASCADE",
        Action::SetNull => "SET NULL",
        Action::SetDefault => "SET DEFAULT",
        Action::NoAction => "NO ACTION",
    }
}
fn query_err(e: rusqlite::Error) -> DbError {
    let msg = e.to_string();
    if let rusqlite::Error::SqliteFailure(error, _) = &e {
        let code = match error.extended_code {
            rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY => Some("PRIMARY_KEY_VIOLATION"),
            rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE => Some("UNIQUE_VIOLATION"),
            rusqlite::ffi::SQLITE_CONSTRAINT_NOTNULL => Some("NOT_NULL_VIOLATION"),
            rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY => Some("FOREIGN_KEY_VIOLATION"),
            rusqlite::ffi::SQLITE_CONSTRAINT_CHECK => Some("CHECK_VIOLATION"),
            // SQLite enforces RESTRICT referential actions through internal
            // triggers, so a blocked RESTRICT surfaces as SQLITE_CONSTRAINT_TRIGGER
            // carrying SQLite's foreign-key message rather than as
            // SQLITE_CONSTRAINT_FOREIGNKEY. It is a referential violation
            // (Section 37), not a query type error, so it must carry the
            // FOREIGN_KEY_VIOLATION code and the INVALID exit status.
            rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER
                if msg.contains("FOREIGN KEY constraint failed") =>
            {
                Some("FOREIGN_KEY_VIOLATION")
            }
            _ => None,
        };
        if let Some(code) = code {
            return DbError::new(code, msg, 2);
        }
    }
    let interrupted = matches!(
        &e,
        rusqlite::Error::SqliteFailure(error, _)
            if error.code == rusqlite::ffi::ErrorCode::OperationInterrupted
    );
    let code = if interrupted {
        "RESOURCE_LIMIT"
    } else if msg.contains("syntax error") {
        "QUERY_UNSUPPORTED"
    } else if msg.contains("no such table") {
        "UNKNOWN_TABLE"
    } else if msg.contains("no such column") {
        "UNKNOWN_COLUMN"
    } else {
        "QUERY_TYPE_ERROR"
    };
    DbError::new(code, msg, 4)
}

pub fn validate_checks(c: &Catalog) -> Vec<Diagnostic> {
    if !c.schemas.values().any(|s| !s.check.is_empty()) {
        return vec![];
    }
    let conn = match load(c, false) {
        Ok(connection) => connection,
        Err(error) => return vec![*error.diagnostic],
    };
    let mut out = vec![];
    for (table, s) in &c.schemas {
        for check in &s.check {
            let sql = format!(
                "SELECT {} FROM {} WHERE NOT ({})",
                s.primary_key
                    .iter()
                    .map(|column| q(column))
                    .collect::<Vec<_>>()
                    .join(","),
                q(table),
                check.expr
            );
            match conn.prepare(&sql) {
                Err(e) => out.push(
                    Diagnostic::error(
                        "SCHEMA_CHECK_INVALID",
                        format!("check {:?} is invalid: {e}", check.name),
                    )
                    .table(table),
                ),
                Ok(mut statement) => match statement.query([]) {
                    Err(error) => out.push(
                        Diagnostic::error("SCHEMA_CHECK_INVALID", error.to_string()).table(table),
                    ),
                    Ok(mut rows) => {
                        let by_key = crate::integrity::rows_by_key(c, table);
                        loop {
                            match rows.next() {
                                Err(error) => {
                                    out.push(
                                        Diagnostic::error(
                                            "SCHEMA_CHECK_INVALID",
                                            error.to_string(),
                                        )
                                        .table(table),
                                    );
                                    break;
                                }
                                Ok(None) => break,
                                Ok(Some(result)) => {
                                    let mut key_row = Map::new();
                                    let mut failed = false;
                                    for (index, column) in s.primary_key.iter().enumerate() {
                                        match result.get_ref(index) {
                                            Ok(value) => {
                                                match decode(from_ref(value), &s.columns[column]) {
                                                    Ok(value) => {
                                                        key_row.insert(column.clone(), value);
                                                    }
                                                    Err(error) => {
                                                        out.push(
                                                            Diagnostic::error(
                                                                "SCHEMA_CHECK_INVALID",
                                                                error.to_string(),
                                                            )
                                                            .table(table),
                                                        );
                                                        failed = true;
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(error) => {
                                                out.push(
                                                    Diagnostic::error(
                                                        "SCHEMA_CHECK_INVALID",
                                                        error.to_string(),
                                                    )
                                                    .table(table),
                                                );
                                                failed = true;
                                                break;
                                            }
                                        }
                                    }
                                    if failed {
                                        break;
                                    }
                                    if let Some(key) =
                                        crate::integrity::key(&key_row, &s.primary_key, s)
                                        && let Some(row) = by_key.get(&key)
                                    {
                                        let mut diagnostic = Diagnostic::error(
                                            "CHECK_VIOLATION",
                                            format!("row violates check {:?}", check.name),
                                        )
                                        .at(row.relative.clone())
                                        .table(table);
                                        diagnostic.constraint = Some(check.expr.clone());
                                        out.push(diagnostic);
                                    }
                                }
                            }
                        }
                    }
                },
            }
        }
    }
    out
}

pub fn export_sqlite(c: &Catalog, table: &str, path: &std::path::Path) -> Result<()> {
    if path.exists() {
        return Err(DbError::usage(format!(
            "refusing to overwrite {}",
            path.display()
        )));
    }
    let mut one = c.clone();
    one.schemas.retain(|name, _| name == table);
    one.rows.retain(|name, _| name == table);
    let conn = load(&one, true)?;
    let target = path.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!("VACUUM INTO '{target}'"))
        .map_err(query_err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(extended_code: std::os::raw::c_int, message: &str) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(extended_code),
            Some(message.to_string()),
        )
    }

    /// Sections 51 and 75: a constraint failure is a relational violation of the
    /// database, so it carries its own stable code and the INVALID exit status,
    /// never the generic query-error status.
    #[test]
    fn test9999_constraint_failures_map_to_relational_codes() {
        for (extended_code, expected) in [
            (
                rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY,
                "PRIMARY_KEY_VIOLATION",
            ),
            (rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE, "UNIQUE_VIOLATION"),
            (
                rusqlite::ffi::SQLITE_CONSTRAINT_NOTNULL,
                "NOT_NULL_VIOLATION",
            ),
            (
                rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY,
                "FOREIGN_KEY_VIOLATION",
            ),
            (rusqlite::ffi::SQLITE_CONSTRAINT_CHECK, "CHECK_VIOLATION"),
        ] {
            let error = query_err(failure(extended_code, "constraint failed"));
            assert_eq!(error.diagnostic.code, expected);
            assert_eq!(error.exit, 2, "{expected} is a database-invalid condition");
        }
    }

    /// Section 37: SQLite enforces a RESTRICT referential action with an
    /// internal trigger, so a blocked RESTRICT arrives as a trigger constraint
    /// carrying SQLite's foreign-key message. It is a referential violation and
    /// must not be reported as a query type error.
    #[test]
    fn test9999_restrict_violations_are_classified_as_referential_violations() {
        let error = query_err(failure(
            rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER,
            "FOREIGN KEY constraint failed",
        ));
        assert_eq!(error.diagnostic.code, "FOREIGN_KEY_VIOLATION");
        assert_eq!(error.exit, 2);

        // A trigger failure that is not a foreign-key message keeps the generic
        // classification: the mapping is deliberately narrow.
        let other = query_err(failure(
            rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER,
            "some other trigger aborted",
        ));
        assert_ne!(other.diagnostic.code, "FOREIGN_KEY_VIOLATION");
    }

    /// Section 25 and 51: query faults are distinguished so a caller can tell a
    /// malformed query from an unknown name, and all use the query exit status.
    #[test]
    fn test9999_query_faults_are_classified_by_kind() {
        for (message, expected) in [
            (r#"near "SELCT": syntax error"#, "QUERY_UNSUPPORTED"),
            ("no such table: ghosts", "UNKNOWN_TABLE"),
            ("no such column: ghost", "UNKNOWN_COLUMN"),
            ("datatype mismatch", "QUERY_TYPE_ERROR"),
        ] {
            let error = query_err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
                Some(message.to_string()),
            ));
            assert_eq!(error.diagnostic.code, expected, "for message {message:?}");
            assert_eq!(error.exit, 4);
        }
    }

    /// Section 61: an interrupted statement is a resource-limit outcome (the
    /// timeout fired), not a malformed query.
    #[test]
    fn test9999_interrupted_statements_report_a_resource_limit() {
        let error = query_err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
            Some("interrupted".to_string()),
        ));
        assert_eq!(error.diagnostic.code, "RESOURCE_LIMIT");
    }
}
