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

pub fn execute(catalog: &Catalog, text: &str, params: &[Value]) -> Result<SqlResult> {
    let params: Vec<_> = params
        .iter()
        .cloned()
        .map(|value| SqlParam { name: None, value })
        .collect();
    execute_params(catalog, text, &params)
}

pub fn execute_params(catalog: &Catalog, text: &str, params: &[SqlParam]) -> Result<SqlResult> {
    execute_params_with_limits(catalog, text, params, None, usize::MAX, u64::MAX)
}
pub fn execute_params_timeout(
    catalog: &Catalog,
    text: &str,
    params: &[SqlParam],
    timeout: Option<std::time::Duration>,
) -> Result<SqlResult> {
    execute_params_with_limits(catalog, text, params, timeout, usize::MAX, u64::MAX)
}
pub fn execute_params_with_limits(
    catalog: &Catalog,
    text: &str,
    params: &[SqlParam],
    timeout: Option<std::time::Duration>,
    max_rows: usize,
    max_memory: u64,
) -> Result<SqlResult> {
    let token = statement_kind(text)?;
    let mut conn = load(catalog, true)?;
    if let Some(limit) = timeout {
        let start = std::time::Instant::now();
        conn.progress_handler(1000, Some(move || start.elapsed() >= limit));
    }
    if matches!(token.as_str(), "SELECT" | "WITH" | "EXPLAIN") {
        let rows = query(&conn, text, params, max_rows, max_memory)?;
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
    timeout: Option<std::time::Duration>,
    max_rows: usize,
    max_memory: u64,
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
    let conn = load(catalog, true)?;
    if let Some(limit) = timeout {
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
        if emitted == max_rows {
            return Err(DbError::new(
                "RESOURCE_LIMIT",
                format!("query exceeds the configured limit of {max_rows} result rows"),
                4,
            ));
        }
        let mut object = Map::new();
        for (index, name) in names.iter().enumerate() {
            let value = from_ref(row.get_ref(index).map_err(query_err)?);
            object.insert(
                name.clone(),
                decode_declared(value, declared[index].as_deref()),
            );
        }
        let row_size = serde_json::to_vec(&object)
            .map_err(|error| DbError::new("QUERY_TYPE_ERROR", error.to_string(), 4))?
            .len() as u64;
        if row_size > max_memory {
            return Err(DbError::new(
                "RESOURCE_LIMIT",
                format!(
                    "one result row exceeds the configured {max_memory} byte query-memory limit"
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
        DbError::from_diag(diagnostic, 4)
    })?;
    if ast.len() != 1 {
        return Err(DbError::new(
            "QUERY_UNSUPPORTED",
            "exactly one SQL statement is required",
            4,
        ));
    }
    Ok(ast[0]
        .to_string()
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase())
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

fn load(c: &Catalog, enforce: bool) -> Result<Connection> {
    let conn = Connection::open_in_memory().map_err(query_err)?;
    conn.create_collation("JDB_DECIMAL", |left, right| {
        use std::str::FromStr;
        match (
            rust_decimal::Decimal::from_str(left),
            rust_decimal::Decimal::from_str(right),
        ) {
            (Ok(left), Ok(right)) => left.cmp(&right),
            // Invalid decimal text cannot occur in a valid catalog. Retaining a
            // total order here keeps SQLite's collation callback infallible.
            _ => left.cmp(right),
        }
    })
    .map_err(query_err)?;
    conn.execute_batch("PRAGMA foreign_keys=OFF; PRAGMA case_sensitive_like=ON;")
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
            obj.insert(n.clone(), decode_declared(value, declared[i].as_deref()));
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
                    v = decode(v, col);
                }
                obj.insert(n.clone(), v);
            }
            rows.push(obj)
        }
        all.insert(table.clone(), rows);
    }
    Ok(all)
}
fn diff(
    c: &Catalog,
    after: &BTreeMap<String, Vec<Map<String, Value>>>,
) -> Result<Vec<Change>> {
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
                        bytes: canonical::pretty(&canonical::canonical_row(row, s)),
                    })
                }
                None => out.push(Change::Write {
                    path,
                    bytes: canonical::pretty(&canonical::canonical_row(row, s)),
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
fn decode(v: Value, c: &crate::schema::Column) -> Value {
    match c.kind {
        ColumnType::Bool => Value::Bool(v.as_i64() == Some(1)),
        ColumnType::Array | ColumnType::Object | ColumnType::Json => v
            .as_str()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(v),
        _ => v,
    }
}
fn decode_declared(v: Value, declared: Option<&str>) -> Value {
    match declared {
        Some("JDB_BLOB_BOOL") => Value::Bool(v.as_i64() == Some(1)),
        Some("JDB_BLOB_ARRAY" | "JDB_BLOB_OBJECT" | "JDB_BLOB_JSON") => v
            .as_str()
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(v),
        _ => v,
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
    match expression {
        Expr::Identifier(identifier) => schema
            .columns
            .get(&identifier.value)
            .is_some_and(|column| column.kind == ColumnType::Bool),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .and_then(|identifier| schema.columns.get(&identifier.value))
            .is_some_and(|column| column.kind == ColumnType::Bool),
        Expr::Value(value) => matches!(value.value, sqlparser::ast::Value::Boolean(_)),
        Expr::Nested(expression) => boolean_expression(schema, expression),
        Expr::UnaryOp {
            op: UnaryOperator::Not | UnaryOperator::BangNot,
            expr,
        } => boolean_expression(schema, expr),
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And | BinaryOperator::Or | BinaryOperator::Xor => {
                boolean_expression(schema, left) && boolean_expression(schema, right)
            }
            BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Spaceship => true,
            _ => false,
        },
        Expr::IsFalse(_)
        | Expr::IsNotFalse(_)
        | Expr::IsTrue(_)
        | Expr::IsNotTrue(_)
        | Expr::IsNull(_)
        | Expr::IsNotNull(_)
        | Expr::IsUnknown(_)
        | Expr::IsNotUnknown(_)
        | Expr::IsDistinctFrom(_, _)
        | Expr::IsNotDistinctFrom(_, _)
        | Expr::InList { .. }
        | Expr::InSubquery { .. }
        | Expr::InUnnest { .. }
        | Expr::Between { .. }
        | Expr::Like { .. }
        | Expr::ILike { .. }
        | Expr::SimilarTo { .. }
        | Expr::RLike { .. }
        | Expr::AnyOp { .. }
        | Expr::AllOp { .. }
        | Expr::Exists { .. } => true,
        _ => false,
    }
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
    let code = if msg.contains("interrupted") {
        "RESOURCE_LIMIT"
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
        Err(error) => return vec![error.diagnostic],
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
                                                key_row.insert(
                                                    column.clone(),
                                                    decode(from_ref(value), &s.columns[column]),
                                                );
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
                                    if let Some(key) = crate::integrity::key(&key_row, &s.primary_key, s)
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
