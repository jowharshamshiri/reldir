//! Bulk import and export.
//!
//! Import reads a JSON array, JSON lines or CSV a row at a time -- from a
//! file or stdin -- and commits every row as one change, under a mode that
//! says how the rows meet the table's: `insert`, `upsert` or `replace`.
//! Export writes a table's rows, filtered and ordered as asked, a row at a
//! time, to JSON, JSON lines, CSV or Parquet; the file appears whole or not
//! at all, and never replaces one that exists.

use super::{Context, Intent, gate, require_valid, schema_of};
use crate::output::{Finish, Sink};
use reldir_core::{
    code::Code,
    diagnostic::{DbError, Result},
    rows::ImportMode,
    schema::{ColumnType, Schema},
    store::revision::Origin,
};
use serde_json::{Map, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

/// The formats rows travel in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Jsonl,
    Csv,
    Parquet,
}

impl Format {
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "json" => Ok(Self::Json),
            "jsonl" | "ndjson" => Ok(Self::Jsonl),
            "csv" => Ok(Self::Csv),
            "parquet" => Ok(Self::Parquet),
            other => Err(DbError::usage(format!(
                "{other:?} is not a format rows travel in: json, jsonl, csv or parquet"
            ))),
        }
    }

    /// The format a path's extension names.
    fn of(path: &Path) -> Option<Self> {
        Self::parse(path.extension()?.to_str()?).ok()
    }
}

fn invalid(message: impl Into<String>) -> DbError {
    DbError::new(Code::INPUT_INVALID, message)
}

/// A reader that refuses to go past `limit` bytes.
struct Limited<R> {
    inner: R,
    left: u64,
    limit: u64,
}

impl<R: Read> Read for Limited<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.left = self.left.checked_sub(read as u64).ok_or_else(|| {
            std::io::Error::other(format!(
                "the input is larger than max_transaction_size, {} bytes",
                self.limit
            ))
        })?;
        Ok(read)
    }
}

fn io_failure(source: &str, error: std::io::Error) -> DbError {
    if error.kind() == std::io::ErrorKind::Other {
        return DbError::new(Code::RESOURCE_LIMIT, error.to_string());
    }
    DbError::io(Path::new(source), error)
}

/// The elements of a JSON array, one at a time, each read with the
/// strictness every row file is read with; or the one object a file holds.
fn json_rows(
    reader: impl Read,
    source: &str,
    each: &mut dyn FnMut(Map<String, Value>) -> Result<()>,
) -> Result<()> {
    let mut bytes = BufReader::new(reader).bytes().peekable();
    let next =
        |bytes: &mut std::iter::Peekable<std::io::Bytes<BufReader<_>>>| -> Result<Option<u8>> {
            bytes
                .next()
                .transpose()
                .map_err(|error| io_failure(source, error))
        };
    let skip =
        |bytes: &mut std::iter::Peekable<std::io::Bytes<BufReader<_>>>| -> Result<Option<u8>> {
            loop {
                match bytes.peek() {
                    Some(Ok(byte)) if byte.is_ascii_whitespace() => {
                        bytes.next();
                    }
                    Some(Ok(byte)) => return Ok(Some(*byte)),
                    Some(Err(_)) => {
                        return Err(io_failure(
                            source,
                            bytes.next().expect("peeked").expect_err("an error"),
                        ));
                    }
                    None => return Ok(None),
                }
            }
        };
    let object = |value: Value, index: usize| -> Result<Map<String, Value>> {
        match value {
            Value::Object(row) => Ok(row),
            other => Err(invalid(format!(
                "element {index} of the array is {}, not an object; a row is one JSON object",
                reldir_core::canonical::compact(&other)
            ))),
        }
    };
    match skip(&mut bytes)? {
        Some(b'[') => {
            next(&mut bytes)?;
        }
        Some(b'{') => {
            let mut text = vec![];
            while let Some(byte) = next(&mut bytes)? {
                text.push(byte);
            }
            let value = reldir_core::json::parse(&text)
                .map_err(|error| invalid(format!("the input is not JSON: {error}")))?;
            return each(object(value, 1)?);
        }
        Some(other) => {
            return Err(invalid(format!(
                "the input begins with {:?}: JSON rows are an array of objects, or one object",
                other as char
            )));
        }
        None => return Err(invalid("the input is empty")),
    }
    let mut index = 0;
    loop {
        match skip(&mut bytes)? {
            Some(b']') => {
                next(&mut bytes)?;
                break;
            }
            None => return Err(invalid("the array is not closed: the input ends inside it")),
            Some(_) => {}
        }
        index += 1;
        // One element: its bytes up to the comma or bracket that ends it,
        // outside every string and nested value.
        let mut element = vec![];
        let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
        let ended = loop {
            let Some(byte) = next(&mut bytes)? else {
                return Err(invalid("the array is not closed: the input ends inside it"));
            };
            if in_string {
                element.push(byte);
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_string = false;
                }
                continue;
            }
            match byte {
                b'"' => in_string = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' if depth > 0 => depth -= 1,
                b',' | b']' if depth == 0 => break byte,
                _ => {}
            }
            element.push(byte);
        };
        let value = reldir_core::json::parse(&element).map_err(|error| {
            invalid(format!("element {index} of the array is not JSON: {error}"))
        })?;
        each(object(value, index)?)?;
        if ended == b']' {
            break;
        }
    }
    if skip(&mut bytes)?.is_some() {
        return Err(invalid("there is more after the array closes"));
    }
    Ok(())
}

fn jsonl_rows(
    reader: impl Read,
    source: &str,
    each: &mut dyn FnMut(Map<String, Value>) -> Result<()>,
) -> Result<()> {
    for (index, line) in BufReader::new(reader).lines().enumerate() {
        let line = line.map_err(|error| io_failure(source, error))?;
        if line.trim().is_empty() {
            continue;
        }
        let value = reldir_core::json::parse_str(&line)
            .map_err(|error| invalid(format!("line {} is not JSON: {error}", index + 1)))?;
        match value {
            Value::Object(row) => each(row)?,
            _ => {
                return Err(invalid(format!(
                    "line {} is not one JSON object",
                    index + 1
                )));
            }
        }
    }
    Ok(())
}

/// A CSV cell as the value its column holds.
fn cell_value(cell: &str, column: &reldir_core::schema::Column) -> Value {
    if cell.is_empty() && column.nullable() {
        return Value::Null;
    }
    match column.kind() {
        ColumnType::String
        | ColumnType::Enum
        | ColumnType::Decimal
        | ColumnType::Bytes
        | ColumnType::Date
        | ColumnType::Timestamp
        | ColumnType::Uuid
        | ColumnType::Ulid => Value::String(cell.to_string()),
        kind => {
            let parsed = reldir_core::json::parse_str(cell)
                .unwrap_or_else(|_| Value::String(cell.to_string()));
            reldir_core::value::lossless_convert(&parsed, kind).unwrap_or(parsed)
        }
    }
}

fn csv_rows(
    reader: impl Read,
    schema: &Schema,
    each: &mut dyn FnMut(Map<String, Value>) -> Result<()>,
) -> Result<()> {
    let mut reader = csv::Reader::from_reader(reader);
    let malformed = |error: csv::Error| invalid(format!("the CSV is malformed: {error}"));
    let headers = reader.headers().map_err(malformed)?.clone();
    for header in headers.iter() {
        if schema.column(header).is_none() {
            return Err(DbError::new(
                Code::UNKNOWN_COLUMN,
                format!(
                    "the CSV header names {header:?}, which {} does not have",
                    schema.table()
                ),
            ));
        }
    }
    for record in reader.records() {
        let record = record.map_err(malformed)?;
        let row = headers
            .iter()
            .zip(record.iter())
            .map(|(header, cell)| {
                let column = schema.column(header).expect("checked above");
                (header.to_string(), cell_value(cell, column))
            })
            .collect();
        each(row)?;
    }
    Ok(())
}

pub struct ImportOptions<'a> {
    pub source: &'a str,
    pub format: Option<Format>,
    pub mode: ImportMode,
}

/// Import rows, every one in one change.
pub fn import(
    context: &Context,
    sink: &mut dyn Sink,
    table: &str,
    options: ImportOptions<'_>,
) -> Result<Finish> {
    let format = match (options.format, options.source) {
        (Some(format), _) => format,
        (None, "-") => {
            return Err(DbError::usage(
                "rows from stdin need their format named: --input-format json, jsonl or csv",
            ));
        }
        (None, source) => Format::of(Path::new(source)).ok_or_else(|| {
            DbError::usage(format!(
                "{source} does not say its format by its extension; name it with --input-format"
            ))
        })?,
    };
    if format == Format::Parquet {
        return Err(DbError::usage(
            "Parquet is an export format; import JSON, JSON lines or CSV",
        ));
    }
    let mut database = context.open_database(sink, Intent::Write)?;
    require_valid(&database)?;
    let schema = schema_of(&database, table)?.clone();
    let limit = database.config.max_transaction_size;
    let reader: Box<dyn Read> = if options.source == "-" {
        Box::new(std::io::stdin().lock())
    } else {
        let path = Path::new(options.source);
        Box::new(std::fs::File::open(path).map_err(|error| DbError::io(path, error))?)
    };
    let reader = Limited {
        inner: reader,
        left: limit,
        limit,
    };
    let mut rows = vec![];
    let mut collect = |row| {
        rows.push(row);
        Ok(())
    };
    match format {
        Format::Json => json_rows(reader, options.source, &mut collect)?,
        Format::Jsonl => jsonl_rows(reader, options.source, &mut collect)?,
        Format::Csv => csv_rows(reader, &schema, &mut collect)?,
        Format::Parquet => unreachable!("refused above"),
    }
    let read = rows.len();
    let (outcome, counts) = database.apply_with(context.request(Origin::Import), |catalog| {
        reldir_core::rows::import(catalog, table, rows, options.mode)
    })?;
    let counts = counts.unwrap_or_default();
    let mut finish = super::committed(sink, &outcome, "imported")?;
    if !outcome.replayed {
        finish.summary = format!(
            "{}; {read} row(s) read: {} inserted, {} updated, {} deleted, {} unchanged",
            finish.summary, counts.inserted, counts.updated, counts.deleted, counts.unchanged
        );
    }
    Ok(finish
        .with("read", read)
        .with("inserted", counts.inserted)
        .with("updated", counts.updated)
        .with("deleted", counts.deleted)
        .with("unchanged", counts.unchanged))
}

pub struct ExportOptions<'a> {
    pub out: Option<&'a Path>,
    pub format: Option<Format>,
    pub filter: Option<&'a str>,
    pub order: Option<&'a str>,
}

/// One format's writer: rows in, bytes out.
trait Writer {
    fn row(&mut self, row: &Map<String, Value>) -> Result<()>;
    fn finish(self: Box<Self>) -> Result<()>;
}

fn write_failure(error: impl std::fmt::Display) -> DbError {
    DbError::new(Code::IO_ERROR, format!("cannot write the export: {error}"))
}

/// A value as compact JSON, exactly as its file holds it: members in the
/// file's order, every number spelled as the file spells it. The canonical
/// form -- members sorted, numbers respelled -- is for comparing values, never
/// for handing them to someone.
fn as_written(value: &(impl serde::Serialize + ?Sized)) -> String {
    serde_json::to_string(value).expect("a JSON value serializes")
}

struct JsonWriter<W: Write> {
    out: W,
    first: bool,
}

impl<W: Write> Writer for JsonWriter<W> {
    fn row(&mut self, row: &Map<String, Value>) -> Result<()> {
        let lead = if self.first { "[\n  " } else { ",\n  " };
        self.first = false;
        write!(self.out, "{lead}{}", as_written(row)).map_err(write_failure)
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        let close = if self.first { "[]\n" } else { "\n]\n" };
        self.out
            .write_all(close.as_bytes())
            .map_err(write_failure)?;
        self.out.flush().map_err(write_failure)
    }
}

struct JsonlWriter<W: Write> {
    out: W,
}

impl<W: Write> Writer for JsonlWriter<W> {
    fn row(&mut self, row: &Map<String, Value>) -> Result<()> {
        writeln!(self.out, "{}", as_written(row)).map_err(write_failure)
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.out.flush().map_err(write_failure)
    }
}

struct CsvWriter<W: Write> {
    out: csv::Writer<W>,
    columns: Vec<String>,
}

impl<W: Write> Writer for CsvWriter<W> {
    fn row(&mut self, row: &Map<String, Value>) -> Result<()> {
        self.out
            .write_record(self.columns.iter().map(|column| match row.get(column) {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(text)) => text.clone(),
                Some(other) => as_written(other),
            }))
            .map_err(write_failure)
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.out.flush().map_err(write_failure)
    }
}

/// Export a table's rows: to a new file, or as the command's rows.
pub fn export(
    context: &Context,
    sink: &mut dyn Sink,
    table: &str,
    options: ExportOptions<'_>,
) -> Result<Finish> {
    let database = context.open_database(sink, Intent::Read)?;
    gate(context, &database, sink)?;
    let schema = schema_of(&database, table)?.clone();
    // Which rows, in which order, is a query: the rows' keys come from it,
    // and each row is then read whole, exactly as its file holds it.
    let key = schema.primary_key().to_vec();
    let quoted: Vec<String> = key
        .iter()
        .map(|column| reldir_core::mirror::quote(column))
        .collect();
    let mut text = format!(
        "SELECT {} FROM {}",
        quoted.join(", "),
        reldir_core::mirror::quote(table)
    );
    if let Some(filter) = options.filter {
        text.push_str(&format!(" WHERE ({filter})"));
    }
    let order = match options.order {
        Some(order) => {
            let (column, direction) = match order.strip_prefix('-') {
                Some(column) => (column, "DESC"),
                None => (order, "ASC"),
            };
            if schema.column(column).is_none() {
                return Err(DbError::new(
                    Code::UNKNOWN_COLUMN,
                    format!("{table} has no column {column:?} to order by"),
                ));
            }
            format!(
                "{} {direction}, {}",
                reldir_core::mirror::quote(column),
                quoted.join(", ")
            )
        }
        None => quoted.join(", "),
    };
    text.push_str(&format!(" ORDER BY {order}"));
    let format = match (options.format, options.out) {
        (Some(format), _) => Some(format),
        (None, Some(path)) => Some(Format::of(path).ok_or_else(|| {
            DbError::usage(format!(
                "{} does not say its format by its extension; name it with --as",
                path.display()
            ))
        })?),
        (None, None) => None,
    };
    let Some(path) = options.out else {
        if format.is_some() {
            return Err(DbError::usage(
                "--as writes a file; name it with --out, or print rows with --format",
            ));
        }
        let mut count = 0;
        each_row(&database, &text, table, &key, &mut |row| {
            count += 1;
            sink.row(row)
        })?;
        return Ok(Finish::ok(format!("{count} row(s)")).with("rows", count));
    };
    let format = format.expect("a file has a format");
    let directory = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if std::fs::symlink_metadata(path).is_ok() {
        return Err(DbError::usage(format!(
            "{} exists; export writes a new file and never replaces one",
            path.display()
        )));
    }
    let staged = tempfile::NamedTempFile::new_in(directory)
        .map_err(|error| DbError::io(directory, error))?;
    let file = std::io::BufWriter::new(
        staged
            .reopen()
            .map_err(|error| DbError::io(staged.path(), error))?,
    );
    let columns: Vec<String> = schema.columns().keys().cloned().collect();
    let mut writer: Box<dyn Writer> = match format {
        Format::Json => Box::new(JsonWriter {
            out: file,
            first: true,
        }),
        Format::Jsonl => Box::new(JsonlWriter { out: file }),
        Format::Csv => {
            let mut out = csv::Writer::from_writer(file);
            out.write_record(&columns).map_err(write_failure)?;
            Box::new(CsvWriter { out, columns })
        }
        Format::Parquet => parquet_writer(file, &schema)?,
    };
    let mut count = 0;
    each_row(&database, &text, table, &key, &mut |row| {
        count += 1;
        writer.row(&row)
    })?;
    writer.finish()?;
    staged
        .persist_noclobber(path)
        .map_err(|error| match error.error.kind() {
            std::io::ErrorKind::AlreadyExists => DbError::usage(format!(
                "{} appeared while exporting; export never replaces a file",
                path.display()
            )),
            _ => DbError::io(path, error.error),
        })?;
    Ok(Finish::ok(format!("{count} row(s) to {}", path.display()))
        .with("rows", count)
        .with("path", path.to_string_lossy().into_owned()))
}

/// Every row the key query selects, in its order, as the files hold them.
fn each_row(
    database: &reldir_core::db::Database,
    text: &str,
    table: &str,
    key: &[String],
    each: &mut dyn FnMut(Map<String, Value>) -> Result<()>,
) -> Result<()> {
    let catalog = &database.catalog;
    reldir_core::sql::query(
        &catalog.mirror,
        &catalog.schemas,
        text,
        &[],
        reldir_core::sql::QueryLimits {
            max_rows: usize::MAX,
            ..database.query_limits()
        },
        |selected| {
            let values: Vec<Value> = key
                .iter()
                .map(|column| selected.get(column).cloned().unwrap_or(Value::Null))
                .collect();
            let row = reldir_core::rows::existing(catalog, table, &values)?;
            each(row.value)
        },
    )
    .map_err(|error| reldir_core::sql::explain_unknown_table(catalog, error))?;
    Ok(())
}

#[cfg(feature = "parquet")]
fn parquet_writer(file: impl Write + Send + 'static, schema: &Schema) -> Result<Box<dyn Writer>> {
    Ok(Box::new(parquet_export::ParquetWriter::new(file, schema)?))
}

#[cfg(not(feature = "parquet"))]
fn parquet_writer(_file: impl Write + Send + 'static, _schema: &Schema) -> Result<Box<dyn Writer>> {
    Err(DbError::usage(
        "this build of reldir cannot write Parquet: it was built without the `parquet` feature",
    ))
}

#[cfg(feature = "parquet")]
mod parquet_export {
    //! Parquet, column by column: an `int` column as INT64, a `float` or
    //! `number` column as DOUBLE -- each value exactly, or the export is
    //! refused -- `bool` as BOOLEAN, and everything else as UTF-8 text: a
    //! string as itself, an integer of any size or a decimal as its digits,
    //! an array or object as its JSON.

    use super::{Writer, write_failure};
    use arrow_array::{
        ArrayRef, RecordBatch,
        builder::{BooleanBuilder, Float64Builder, Int64Builder, StringBuilder},
    };
    use arrow_schema::{DataType, Field, Schema as ArrowSchema};
    use parquet::{arrow::ArrowWriter, basic::Compression, file::properties::WriterProperties};
    use reldir_core::{
        code::Code,
        diagnostic::{DbError, Result},
        schema::{ColumnType, Schema},
    };
    use serde_json::{Map, Value};
    use std::{io::Write, sync::Arc};

    const BATCH: usize = 8192;

    enum Builder {
        Int(Int64Builder),
        Double(Float64Builder),
        Bool(BooleanBuilder),
        Text(StringBuilder),
    }

    pub struct ParquetWriter<W: Write + Send> {
        writer: ArrowWriter<W>,
        schema: Arc<ArrowSchema>,
        columns: Vec<(String, Builder)>,
        rows: usize,
        table: String,
    }

    fn data_type(kind: &ColumnType) -> DataType {
        match kind {
            ColumnType::Int => DataType::Int64,
            ColumnType::Float | ColumnType::Number => DataType::Float64,
            ColumnType::Bool => DataType::Boolean,
            _ => DataType::Utf8,
        }
    }

    fn builder(kind: &DataType) -> Builder {
        match kind {
            DataType::Int64 => Builder::Int(Int64Builder::new()),
            DataType::Float64 => Builder::Double(Float64Builder::new()),
            DataType::Boolean => Builder::Bool(BooleanBuilder::new()),
            _ => Builder::Text(StringBuilder::new()),
        }
    }

    impl<W: Write + Send> ParquetWriter<W> {
        pub fn new(out: W, schema: &Schema) -> Result<Self> {
            let fields: Vec<Field> = schema
                .columns()
                .iter()
                .map(|(name, column)| Field::new(name, data_type(column.kind()), true))
                .collect();
            let arrow = Arc::new(ArrowSchema::new(fields));
            let columns = arrow
                .fields()
                .iter()
                .map(|field| (field.name().clone(), builder(field.data_type())))
                .collect();
            let properties = WriterProperties::builder()
                .set_compression(Compression::SNAPPY)
                .build();
            let writer = ArrowWriter::try_new(out, Arc::clone(&arrow), Some(properties))
                .map_err(write_failure)?;
            Ok(Self {
                writer,
                schema: arrow,
                columns,
                rows: 0,
                table: schema.table().to_string(),
            })
        }

        fn flush_batch(&mut self) -> Result<()> {
            if self.rows == 0 {
                return Ok(());
            }
            let arrays: Vec<ArrayRef> = self
                .columns
                .iter_mut()
                .map(|(_, builder)| -> ArrayRef {
                    match builder {
                        Builder::Int(b) => Arc::new(b.finish()),
                        Builder::Double(b) => Arc::new(b.finish()),
                        Builder::Bool(b) => Arc::new(b.finish()),
                        Builder::Text(b) => Arc::new(b.finish()),
                    }
                })
                .collect();
            let batch =
                RecordBatch::try_new(Arc::clone(&self.schema), arrays).map_err(write_failure)?;
            self.writer.write(&batch).map_err(write_failure)?;
            self.rows = 0;
            Ok(())
        }
    }

    fn unfit(table: &str, column: &str, value: &Value, what: &str) -> DbError {
        DbError::new(
            Code::INPUT_INVALID,
            format!(
                "{table}.{column} holds {}, which a Parquet {what} column cannot hold exactly; export it as JSON",
                reldir_core::canonical::compact(value)
            ),
        )
    }

    impl<W: Write + Send> Writer for ParquetWriter<W> {
        fn row(&mut self, row: &Map<String, Value>) -> Result<()> {
            for (name, builder) in &mut self.columns {
                let value = row.get(name.as_str()).filter(|value| !value.is_null());
                match builder {
                    Builder::Int(b) => match value {
                        None => b.append_null(),
                        Some(Value::Number(n)) if n.as_i64().is_some() => {
                            b.append_value(n.as_i64().expect("checked"))
                        }
                        Some(other) => return Err(unfit(&self.table, name, other, "INT64")),
                    },
                    Builder::Double(b) => match value {
                        None => b.append_null(),
                        Some(Value::Number(n)) => {
                            let lexeme = n.to_string();
                            let double: f64 = lexeme.parse().map_err(|_| {
                                unfit(&self.table, name, &Value::Number(n.clone()), "DOUBLE")
                            })?;
                            let exact = double.is_finite()
                                && reldir_core::value::compare_numeric(
                                    &lexeme,
                                    &format!("{double}"),
                                ) == Some(std::cmp::Ordering::Equal);
                            if !exact {
                                return Err(unfit(
                                    &self.table,
                                    name,
                                    &Value::Number(n.clone()),
                                    "DOUBLE",
                                ));
                            }
                            b.append_value(double);
                        }
                        Some(other) => return Err(unfit(&self.table, name, other, "DOUBLE")),
                    },
                    Builder::Bool(b) => match value {
                        None => b.append_null(),
                        Some(Value::Bool(flag)) => b.append_value(*flag),
                        Some(other) => return Err(unfit(&self.table, name, other, "BOOLEAN")),
                    },
                    Builder::Text(b) => match value {
                        None => b.append_null(),
                        Some(Value::String(text)) => b.append_value(text),
                        Some(other) => b.append_value(super::as_written(other)),
                    },
                }
            }
            self.rows += 1;
            if self.rows == BATCH {
                self.flush_batch()?;
            }
            Ok(())
        }

        fn finish(mut self: Box<Self>) -> Result<()> {
            self.flush_batch()?;
            self.writer.close().map_err(write_failure)?;
            Ok(())
        }
    }
}
