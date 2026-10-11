//! The observed contract: what the JSON at each path of a folder has been seen
//! to be.
//!
//! A folder of JSON that other programs read has a schema whether or not
//! anyone wrote it down: the structure those programs rely on. reldir infers
//! it the first time it sees the folder, and from then on nothing reldir writes
//! on its own authority may leave that contract. The contract therefore records
//! every *structural* fact a consumer could rely on, and nothing that is merely
//! a coincidence of the data so far:
//!
//! - which JSON types occur, and whether `null` does;
//! - how numbers are spelled: as integers within 64 bits, as integers beyond
//!   them, with a fraction or exponent, or both;
//! - which members an object has, whether every instance has each, and that it
//!   has no others -- objects are closed at every depth;
//! - what array elements are, and that an array whose elements were never seen
//!   has none reldir may invent;
//! - the strong string forms that hold of *every* observed string at a path:
//!   RFC 3339 date-times with an offset, full dates, uuids (with their letter
//!   case), and ulids.
//!
//! Statistical facts -- lengths, ranges, enumerations, minimum array sizes,
//! weak formats such as `email` or `uri` -- are never recorded: a value the
//! data has not happened to contain yet is not one its consumers forbid.
//! Unobserved *structure*, though, is forbidden: a type, member, null or number
//! spelling never seen is outside the contract.
//!
//! # The lattice
//!
//! [`Shape`] is a join-semilattice. [`Shape::bottom`] is "nothing observed";
//! [`Shape::observe`] is the exact shape of one value; [`Shape::join`] is the
//! least shape covering both arguments, and is commutative, associative and
//! idempotent. [`Shape::le`] is the lattice order, and [`Shape::admits`] asks
//! whether observing a value would leave a shape unchanged -- whether the value
//! is inside the contract.
//!
//! Every component is itself a lattice whose elements are exactly the
//! contracts that can be emitted, so a shape never carries a fact its schema
//! does not state:
//!
//! - numbers: `int` and `float` are incomparable; `int ⊑ bigint`; the join of
//!   `float` with either integer class is `number`;
//! - strings: lowercase and uppercase uuids join to mixed-case uuids; any other
//!   two different forms join to a plain string;
//! - objects: records join member-wise (a member is required only where both
//!   sides require it); a map-like object absorbs a record into its value
//!   shape. Maps sit above records.
//!
//! # Map-like objects
//!
//! An object path whose instances carry more than [`MAP_MIN_NAMES`] distinct
//! member names, none of them present in more than half of the instances, is a
//! map keyed by data rather than a record with fields. It is modelled as
//! `additionalProperties: <join of every value>`, with `propertyNames` stating a
//! strong form every name shares. Telling a map from a record needs counts, so
//! the judgment is made only by [`Shape::infer`] and [`TableShape::from_rows`],
//! which see the whole multiset of values. [`Shape::observe`] describes one
//! value exactly and never produces a map; a record is always below the map its
//! path was judged to be, so every inferred shape admits each value it was
//! inferred from. A table's rows are never judged a map: rows have columns.
//!
//! # Determinism
//!
//! A shape built from the same values in any order is the same shape, and
//! emits the same schema, with one deliberate exception: object members are
//! kept in first-observed order, because that is the order a schema author
//! reads and the order reldir writes members in. [`TableShape::from_rows`]
//! sorts rows by their path before observing them, so member order -- and
//! therefore the emitted document -- depends only on the folder's contents.
//! Equality of shapes ignores member order; emission does not.
//!
//! # Emission
//!
//! [`Shape::to_subschema`] writes the reldir dialect of JSON Schema 2020-12.
//! Spelling is a reldir lexical fact, not a JSON Schema one: JSON Schema's
//! `integer` accepts `3.0` and its `number` accepts `3`. The `int` and `float`
//! column tags carry the spelling to reldir's lexical checks; a number class
//! without a tag (`bigint` as `"type": "integer"`, `number` as
//! `"type": "number"`) states only what JSON Schema can. Where the emitted
//! schema is wider than the shape -- a spelling JSON Schema cannot see, an
//! all-digit uuid admitted by an uppercase pattern -- [`Shape::admits`] is the
//! authority.

use crate::schema::{meta, path::escape_pointer};
use crate::value::ULID_PATTERN;
use indexmap::IndexMap;
use serde_json::{Map, Number, Value, json};

/// An object path with more distinct member names than this, none present in
/// more than half of the instances, is a map rather than a record.
pub const MAP_MIN_NAMES: usize = 32;

/// The pattern a lowercase hyphenated uuid matches.
pub const UUID_LOWER_PATTERN: &str =
    "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$";

/// The pattern an uppercase hyphenated uuid matches.
pub const UUID_UPPER_PATTERN: &str =
    "^[0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12}$";

/// The pattern a hyphenated uuid in either letter case matches.
pub const UUID_MIXED_PATTERN: &str =
    "^[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}$";

/// How the numbers observed at a path are spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NumberClass {
    /// Every number is spelled as an integer -- no fraction, no exponent --
    /// within the 64-bit signed range.
    Int,
    /// Every number is spelled as an integer, and some lie beyond the 64-bit
    /// signed range. Still JSON integers, so `"type": "integer"`, but not the
    /// `int` column type.
    BigInt,
    /// Every number is spelled with a fraction or an exponent.
    Float,
    /// Both integer and fraction-or-exponent spellings occur.
    Number,
}

impl NumberClass {
    /// The class of one number, read from its exact spelling.
    pub fn of(number: &Number) -> Self {
        let text = number.to_string();
        if text.contains(['.', 'e', 'E']) {
            Self::Float
        } else if text.parse::<i64>().is_ok() {
            Self::Int
        } else {
            Self::BigInt
        }
    }

    /// The least class covering both.
    pub fn join(self, other: Self) -> Self {
        match (self, other) {
            (left, right) if left == right => left,
            (Self::Int, Self::BigInt) | (Self::BigInt, Self::Int) => Self::BigInt,
            _ => Self::Number,
        }
    }

    pub fn le(self, other: Self) -> bool {
        self.join(other) == other
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Int => "integers within the 64-bit range",
            Self::BigInt => "integers",
            Self::Float => "numbers written with a fraction or exponent",
            Self::Number => "numbers",
        }
    }

    fn fragment(self) -> Map<String, Value> {
        let value = match self {
            Self::Int => json!({ "type": "integer", meta::TYPE_TAG: "int" }),
            Self::BigInt => json!({ "type": "integer" }),
            Self::Float => json!({ "type": "number", meta::TYPE_TAG: "float" }),
            Self::Number => json!({ "type": "number" }),
        };
        into_object(value)
    }
}

/// The letter case of the uuids observed at a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UuidCase {
    /// No uuid has an uppercase hex letter: reldir's canonical spelling. A uuid
    /// of digits alone is lowercase.
    Lower,
    /// No uuid has a lowercase hex letter, and some have an uppercase one.
    Upper,
    /// Both cases occur, within a value or across values.
    Mixed,
}

/// The strongest form every string observed at a path has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StringForm {
    /// No strong form holds of every string.
    Plain,
    /// RFC 3339 `full-date`: `YYYY-MM-DD`, a real calendar day.
    Date,
    /// RFC 3339 `date-time` with an explicit offset (`Z` or `±hh:mm`), without
    /// a leap second.
    DateTime,
    /// A hyphenated uuid, 36 characters.
    Uuid(UuidCase),
    /// A ulid in canonical form: 26 uppercase Crockford base32 characters.
    Ulid,
}

impl StringForm {
    /// The strongest form of one string.
    pub fn of(text: &str) -> Self {
        if is_date_time(text) {
            Self::DateTime
        } else if is_date(text) {
            Self::Date
        } else if let Some(case) = uuid_case(text) {
            Self::Uuid(case)
        } else if is_ulid(text) {
            Self::Ulid
        } else {
            Self::Plain
        }
    }

    /// The least form covering both.
    pub fn join(self, other: Self) -> Self {
        match (self, other) {
            (left, right) if left == right => left,
            (Self::Uuid(_), Self::Uuid(_)) => Self::Uuid(UuidCase::Mixed),
            _ => Self::Plain,
        }
    }

    pub fn le(self, other: Self) -> bool {
        self.join(other) == other
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Plain => "strings",
            Self::Date => "dates (YYYY-MM-DD)",
            Self::DateTime => "RFC 3339 date-times with an offset",
            Self::Uuid(UuidCase::Lower) => "lowercase uuids",
            Self::Uuid(UuidCase::Upper) => "uppercase uuids",
            Self::Uuid(UuidCase::Mixed) => "uuids",
            Self::Ulid => "ulids",
        }
    }

    /// The subschema of a string of this form, as a column.
    fn fragment(self) -> Map<String, Value> {
        let value = match self {
            Self::Plain => json!({ "type": "string" }),
            Self::Date => json!({ "type": "string", "format": "date" }),
            Self::DateTime => json!({ "type": "string", "format": "date-time" }),
            // Only the lowercase form is reldir's uuid column type, whose
            // lexical check refuses upper case; the others are stated by
            // pattern so that the case observed is the case required.
            Self::Uuid(UuidCase::Lower) => json!({ "type": "string", "format": "uuid" }),
            Self::Uuid(UuidCase::Upper) => {
                json!({ "type": "string", "pattern": UUID_UPPER_PATTERN })
            }
            Self::Uuid(UuidCase::Mixed) => {
                json!({ "type": "string", "pattern": UUID_MIXED_PATTERN })
            }
            Self::Ulid => {
                json!({ "type": "string", meta::TYPE_TAG: "ulid", "pattern": ULID_PATTERN })
            }
        };
        into_object(value)
    }

    /// The `propertyNames` subschema for member names of this form, if the
    /// form says anything. Column tags type columns, not names, so every form
    /// is stated with standard keywords here.
    fn names_subschema(self) -> Option<Value> {
        match self {
            Self::Plain => None,
            Self::Date => Some(json!({ "format": "date" })),
            Self::DateTime => Some(json!({ "format": "date-time" })),
            Self::Uuid(UuidCase::Lower) => Some(json!({ "pattern": UUID_LOWER_PATTERN })),
            Self::Uuid(UuidCase::Upper) => Some(json!({ "pattern": UUID_UPPER_PATTERN })),
            Self::Uuid(UuidCase::Mixed) => Some(json!({ "pattern": UUID_MIXED_PATTERN })),
            Self::Ulid => Some(json!({ "pattern": ULID_PATTERN })),
        }
    }
}

fn digits(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        bytes
            .iter()
            .fold(0u32, |total, byte| total * 10 + u32::from(byte - b'0')),
    )
}

/// RFC 3339 `full-date`, with the calendar checked.
fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    let (Some(year), Some(month), Some(day)) = (
        digits(&bytes[0..4]),
        digits(&bytes[5..7]),
        digits(&bytes[8..10]),
    ) else {
        return false;
    };
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let last = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=last).contains(&day)
}

/// RFC 3339 `date-time` with an offset. Leap seconds are not recognised: a
/// path holding one is a plain string, which is the weaker contract.
fn is_date_time(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() < 20 || !text.is_char_boundary(10) || !is_date(&text[..10]) {
        return false;
    }
    if !matches!(bytes[10], b'T' | b't') || bytes[13] != b':' || bytes[16] != b':' {
        return false;
    }
    let (Some(hour), Some(minute), Some(second)) = (
        digits(&bytes[11..13]),
        digits(&bytes[14..16]),
        digits(&bytes[17..19]),
    ) else {
        return false;
    };
    if hour > 23 || minute > 59 || second > 59 {
        return false;
    }
    let mut index = 19;
    if bytes[index] == b'.' {
        let start = index + 1;
        index = start;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == start {
            return false;
        }
    }
    let offset = &bytes[index..];
    let offset_ok = match offset {
        [b'Z' | b'z'] => true,
        [b'+' | b'-', h1, h2, b':', m1, m2] => {
            matches!((digits(&[*h1, *h2]), digits(&[*m1, *m2])), (Some(h), Some(m)) if h <= 23 && m <= 59)
        }
        _ => false,
    };
    // The emitted `date-time` makes the path reldir's timestamp type, whose
    // values are read with chrono; a spelling chrono refuses is not one.
    offset_ok && chrono::DateTime::parse_from_rfc3339(text).is_ok()
}

fn uuid_case(text: &str) -> Option<UuidCase> {
    let bytes = text.as_bytes();
    if bytes.len() != 36 {
        return None;
    }
    let (mut lower, mut upper) = (false, false);
    for (index, byte) in bytes.iter().enumerate() {
        match (index, byte) {
            (8 | 13 | 18 | 23, b'-') => {}
            (8 | 13 | 18 | 23, _) => return None,
            (_, b'0'..=b'9') => {}
            (_, b'a'..=b'f') => lower = true,
            (_, b'A'..=b'F') => upper = true,
            _ => return None,
        }
    }
    Some(match (lower, upper) {
        (true, true) => UuidCase::Mixed,
        (false, true) => UuidCase::Upper,
        _ => UuidCase::Lower,
    })
}

/// Exactly [`ULID_PATTERN`].
fn is_ulid(text: &str) -> bool {
    const CROCKFORD: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let bytes = text.as_bytes();
    bytes.len() == 26
        && (b'0'..=b'7').contains(&bytes[0])
        && bytes.iter().all(|byte| CROCKFORD.contains(byte))
}

fn into_object(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(members) => members,
        other => unreachable!("a fragment literal is an object, not {other}"),
    }
}

/// One member of an observed record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    shape: Shape,
    required: bool,
}

impl Member {
    /// What the member's values have been.
    pub fn shape(&self) -> &Shape {
        &self.shape
    }

    /// Whether every observed instance of the parent object has the member.
    pub fn required(&self) -> bool {
        self.required
    }
}

/// The observed objects at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectShape {
    /// Objects with named members, closed to every other name. Members are in
    /// first-observed order; equality ignores the order.
    Record(IndexMap<String, Member>),
    /// Objects keyed by data: any name of the observed form, every value of the
    /// observed shape.
    Map {
        names: StringForm,
        values: Box<Shape>,
    },
}

impl ObjectShape {
    fn join(&self, other: &Self) -> Self {
        match (self, other) {
            (Self::Record(left), Self::Record(right)) => {
                let mut members = IndexMap::with_capacity(left.len().max(right.len()));
                for (name, member) in left {
                    let joined = match right.get(name) {
                        Some(theirs) => Member {
                            shape: member.shape.join(&theirs.shape),
                            required: member.required && theirs.required,
                        },
                        None => Member {
                            shape: member.shape.clone(),
                            required: false,
                        },
                    };
                    members.insert(name.clone(), joined);
                }
                for (name, member) in right {
                    if !left.contains_key(name) {
                        members.insert(
                            name.clone(),
                            Member {
                                shape: member.shape.clone(),
                                required: false,
                            },
                        );
                    }
                }
                Self::Record(members)
            }
            (Self::Map { names, values }, Self::Record(members))
            | (Self::Record(members), Self::Map { names, values }) => Self::Map {
                names: members
                    .keys()
                    .fold(*names, |form, name| form.join(StringForm::of(name))),
                values: Box::new(members.values().fold((**values).clone(), |shape, member| {
                    shape.join(&member.shape)
                })),
            },
            (
                Self::Map { names, values },
                Self::Map {
                    names: other_names,
                    values: other_values,
                },
            ) => Self::Map {
                names: names.join(*other_names),
                values: Box::new(values.join(other_values)),
            },
        }
    }

    fn fragment(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("type".into(), json!("object"));
        match self {
            Self::Record(members) => {
                if !members.is_empty() {
                    let properties: Map<String, Value> = members
                        .iter()
                        .map(|(name, member)| (name.clone(), member.shape.to_subschema()))
                        .collect();
                    out.insert("properties".into(), Value::Object(properties));
                    let required: Vec<Value> = members
                        .iter()
                        .filter(|(_, member)| member.required)
                        .map(|(name, _)| Value::String(name.clone()))
                        .collect();
                    if !required.is_empty() {
                        out.insert("required".into(), Value::Array(required));
                    }
                }
                out.insert("additionalProperties".into(), Value::Bool(false));
            }
            Self::Map { names, values } => {
                out.insert("additionalProperties".into(), values.to_subschema());
                if let Some(names) = names.names_subschema() {
                    out.insert("propertyNames".into(), names);
                }
            }
        }
        out
    }
}

/// What the values at one JSON path have been observed to be. See the module
/// documentation for the lattice and what it records.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Shape {
    null: bool,
    boolean: bool,
    number: Option<NumberClass>,
    string: Option<StringForm>,
    object: Option<ObjectShape>,
    /// `Some(items)` once an array has been observed; `items` is bottom while
    /// no element has been.
    array: Option<Box<Shape>>,
}

fn join_option<T: Copy>(left: Option<T>, right: Option<T>, join: impl Fn(T, T) -> T) -> Option<T> {
    match (left, right) {
        (Some(left), Some(right)) => Some(join(left, right)),
        (one, other) => one.or(other),
    }
}

impl Shape {
    /// Nothing observed: the shape of a path no value has reached. It admits
    /// nothing, and emits `false`.
    pub fn bottom() -> Self {
        Self::default()
    }

    /// The exact shape of one value. Objects are records whose members are
    /// all required; arrays' items are the join of their elements.
    pub fn observe(value: &Value) -> Self {
        let mut shape = Self::bottom();
        match value {
            Value::Null => shape.null = true,
            Value::Bool(_) => shape.boolean = true,
            Value::Number(number) => shape.number = Some(NumberClass::of(number)),
            Value::String(text) => shape.string = Some(StringForm::of(text)),
            Value::Array(items) => {
                shape.array = Some(Box::new(
                    items.iter().fold(Self::bottom(), |items, item| {
                        items.join(&Self::observe(item))
                    }),
                ));
            }
            Value::Object(members) => {
                shape.object = Some(ObjectShape::Record(
                    members
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.clone(),
                                Member {
                                    shape: Self::observe(value),
                                    required: true,
                                },
                            )
                        })
                        .collect(),
                ));
            }
        }
        shape
    }

    /// The shape of a multiset of values, judging map-like objects at every
    /// depth. Values are observed in the order given, which fixes member
    /// order.
    pub fn infer<'a>(values: impl IntoIterator<Item = &'a Value>) -> Self {
        let mut tally = Tally::default();
        for value in values {
            tally.add(value);
        }
        tally.shape(true)
    }

    /// The least shape covering both.
    pub fn join(&self, other: &Self) -> Self {
        Self {
            null: self.null || other.null,
            boolean: self.boolean || other.boolean,
            number: join_option(self.number, other.number, NumberClass::join),
            string: join_option(self.string, other.string, StringForm::join),
            object: match (&self.object, &other.object) {
                (Some(left), Some(right)) => Some(left.join(right)),
                (one, other) => one.clone().or_else(|| other.clone()),
            },
            array: match (&self.array, &other.array) {
                (Some(left), Some(right)) => Some(Box::new(left.join(right))),
                (one, other) => one.clone().or_else(|| other.clone()),
            },
        }
    }

    /// The lattice order: `self ⊑ other` exactly when `other` already covers
    /// everything `self` does.
    pub fn le(&self, other: &Self) -> bool {
        &self.join(other) == other
    }

    /// Whether the value is inside the contract: observing it would leave the
    /// shape unchanged.
    pub fn admits(&self, value: &Value) -> bool {
        Self::observe(value).le(self)
    }

    /// The least widening of this shape that admits the value.
    pub fn widening_for(&self, value: &Value) -> Self {
        self.join(&Self::observe(value))
    }

    /// Each way the shape would have to widen to admit the value, located by
    /// JSON Pointer into the value. Empty exactly when the value is admitted.
    pub fn widenings(&self, value: &Value) -> Vec<String> {
        let mut out = vec![];
        self.widen(value, "", &mut out);
        let mut seen = std::collections::HashSet::new();
        out.retain(|line| seen.insert(line.clone()));
        out
    }

    /// [`Self::widenings`] as one sentence, e.g. `/note would have to admit
    /// null`. For an admitted value it says that nothing would widen.
    pub fn describe_widening(&self, value: &Value) -> String {
        let lines = self.widenings(value);
        if lines.is_empty() {
            "nothing would have to widen: the value is inside the observed contract".into()
        } else {
            lines.join("; ")
        }
    }

    pub fn is_bottom(&self) -> bool {
        self == &Self::bottom()
    }

    /// Whether `null` has been observed.
    pub fn null(&self) -> bool {
        self.null
    }

    /// Whether a boolean has been observed.
    pub fn boolean(&self) -> bool {
        self.boolean
    }

    pub fn number(&self) -> Option<NumberClass> {
        self.number
    }

    pub fn string(&self) -> Option<StringForm> {
        self.string
    }

    pub fn object(&self) -> Option<&ObjectShape> {
        self.object.as_ref()
    }

    /// The shape of array elements, once an array has been observed: bottom
    /// when every observed array was empty.
    pub fn items(&self) -> Option<&Shape> {
        self.array.as_deref()
    }

    /// The reldir-dialect JSON Schema 2020-12 subschema stating this contract.
    ///
    /// A single type is that type's subschema, with `null` added to `type` when
    /// observed. Several scalar types merge into one `type` list when at most
    /// one carries keywords besides `type` and none carries a column tag --
    /// every other keyword applies to one instance type only, so the merge is
    /// exact. Otherwise, and whenever an object or array mixes with another
    /// type, the subschema is an `anyOf` of each type's subschema. Types are
    /// listed in a fixed order -- boolean, number, string, array, object, null
    /// -- so the output does not depend on the order values were observed in.
    pub fn to_subschema(&self) -> Value {
        let mut fragments: Vec<Map<String, Value>> = vec![];
        if self.boolean {
            fragments.push(into_object(json!({ "type": "boolean" })));
        }
        if let Some(class) = self.number {
            fragments.push(class.fragment());
        }
        if let Some(form) = self.string {
            fragments.push(form.fragment());
        }
        if let Some(items) = &self.array {
            fragments.push(into_object(
                json!({ "type": "array", "items": items.to_subschema() }),
            ));
        }
        if let Some(object) = &self.object {
            fragments.push(object.fragment());
        }
        let null = || json!({ "type": "null" });
        match fragments.len() {
            0 if self.null => null(),
            0 => Value::Bool(false),
            1 => {
                let mut single = fragments.remove(0);
                if self.null {
                    let name = single["type"].clone();
                    single.insert("type".into(), json!([name, "null"]));
                }
                Value::Object(single)
            }
            _ => {
                let structured = self.array.is_some() || self.object.is_some();
                let rich = fragments.iter().filter(|f| f.len() > 1).count();
                let tagged = fragments.iter().any(|f| f.contains_key(meta::TYPE_TAG));
                if !structured && rich <= 1 && !tagged {
                    let mut merged = Map::new();
                    let mut types: Vec<Value> =
                        fragments.iter().map(|f| f["type"].clone()).collect();
                    if self.null {
                        types.push(json!("null"));
                    }
                    merged.insert("type".into(), Value::Array(types));
                    for fragment in fragments {
                        for (key, value) in fragment {
                            if key != "type" {
                                merged.insert(key, value);
                            }
                        }
                    }
                    Value::Object(merged)
                } else {
                    let mut branches: Vec<Value> =
                        fragments.into_iter().map(Value::Object).collect();
                    if self.null {
                        branches.push(null());
                    }
                    json!({ "anyOf": branches })
                }
            }
        }
    }

    fn widen(&self, value: &Value, pointer: &str, out: &mut Vec<String>) {
        let at = if pointer.is_empty() {
            "the value"
        } else {
            pointer
        };
        match value {
            Value::Null if !self.null => out.push(format!("{at} would have to admit null")),
            Value::Bool(_) if !self.boolean => {
                out.push(format!("{at} would have to admit a boolean"))
            }
            Value::Number(number) => {
                let class = NumberClass::of(number);
                match self.number {
                    None => out.push(format!("{at} would have to admit a number ({number})")),
                    Some(held) if !class.le(held) => out.push(format!(
                        "{at} holds only {}; it would have to admit {number}, one of the {}",
                        held.describe(),
                        class.describe()
                    )),
                    Some(_) => {}
                }
            }
            Value::String(text) => {
                let form = StringForm::of(text);
                match self.string {
                    None => out.push(format!(
                        "{at} would have to admit a string ({})",
                        quote(text)
                    )),
                    Some(held) if !form.le(held) => out.push(format!(
                        "{at} holds only {}; it would have to admit {}",
                        held.describe(),
                        quote(text)
                    )),
                    Some(_) => {}
                }
            }
            Value::Array(items) => match &self.array {
                None => out.push(format!("{at} would have to admit an array")),
                Some(shape) => {
                    for (index, item) in items.iter().enumerate() {
                        shape.widen(item, &format!("{pointer}/{index}"), out);
                    }
                }
            },
            Value::Object(members) => match &self.object {
                None => out.push(format!("{at} would have to admit an object")),
                Some(ObjectShape::Record(record)) => {
                    for (name, member) in members {
                        let child = format!("{pointer}/{}", escape_pointer(name));
                        match record.get(name) {
                            Some(known) => known.shape.widen(member, &child, out),
                            None => out.push(format!(
                                "{child} would have to be admitted: no observed object at {at} \
                                 has this member"
                            )),
                        }
                    }
                    for (name, member) in record {
                        if member.required && !members.contains_key(name) {
                            out.push(format!(
                                "{pointer}/{} would have to become optional: every observed \
                                 object at {at} has it",
                                escape_pointer(name)
                            ));
                        }
                    }
                }
                Some(ObjectShape::Map { names, values }) => {
                    for (name, member) in members {
                        if !StringForm::of(name).le(*names) {
                            out.push(format!(
                                "{at} would have to admit the member name {}: its observed \
                                 names are {}",
                                quote(name),
                                names.describe()
                            ));
                        }
                        values.widen(member, &format!("{pointer}/{}", escape_pointer(name)), out);
                    }
                }
            },
            _ => {}
        }
    }
}

fn quote(text: &str) -> String {
    const LIMIT: usize = 40;
    match text.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{:?}…", &text[..cut]),
        None => format!("{text:?}"),
    }
}

/// The multiset view of observed values: a shape's facts plus the counts that
/// tell a map from a record. Merging adds counts, so it is not a lattice; it is
/// only ever reduced to a [`Shape`].
#[derive(Debug, Clone, Default)]
struct Tally {
    /// Null, boolean, number and string facts; never an object or array.
    scalars: Shape,
    object: Option<ObjectTally>,
    array: Option<Box<Tally>>,
}

#[derive(Debug, Clone, Default)]
struct ObjectTally {
    instances: u64,
    /// For each name: the instances that have it, and their values.
    members: IndexMap<String, (u64, Tally)>,
}

impl Tally {
    fn add(&mut self, value: &Value) {
        match value {
            Value::Array(items) => {
                let tally = self.array.get_or_insert_with(Default::default);
                for item in items {
                    tally.add(item);
                }
            }
            Value::Object(members) => {
                let tally = self.object.get_or_insert_with(Default::default);
                tally.instances += 1;
                for (name, member) in members {
                    let entry = tally.members.entry(name.clone()).or_default();
                    entry.0 += 1;
                    entry.1.add(member);
                }
            }
            scalar => self.scalars = self.scalars.join(&Shape::observe(scalar)),
        }
    }

    fn merge(&mut self, other: &Self) {
        self.scalars = self.scalars.join(&other.scalars);
        if let Some(theirs) = &other.object {
            let ours = self.object.get_or_insert_with(Default::default);
            ours.instances += theirs.instances;
            for (name, (count, tally)) in &theirs.members {
                let entry = ours.members.entry(name.clone()).or_default();
                entry.0 += count;
                entry.1.merge(tally);
            }
        }
        if let Some(theirs) = &other.array {
            self.array
                .get_or_insert_with(Default::default)
                .merge(theirs);
        }
    }

    /// The shape, judging map-likeness at this path when `judge` is set and at
    /// every path below it regardless.
    fn shape(&self, judge: bool) -> Shape {
        let mut shape = self.scalars.clone();
        shape.array = self.array.as_ref().map(|items| Box::new(items.shape(true)));
        shape.object = self.object.as_ref().map(|object| object.shape(judge));
        shape
    }
}

impl ObjectTally {
    fn map_like(&self) -> bool {
        self.members.len() > MAP_MIN_NAMES
            && self
                .members
                .values()
                .all(|(count, _)| count.saturating_mul(2) <= self.instances)
    }

    fn shape(&self, judge: bool) -> ObjectShape {
        if judge && self.map_like() {
            let mut values = Tally::default();
            let mut names: Option<StringForm> = None;
            for (name, (_, tally)) in &self.members {
                values.merge(tally);
                names = join_option(names, Some(StringForm::of(name)), StringForm::join);
            }
            ObjectShape::Map {
                names: names.unwrap_or(StringForm::Plain),
                values: Box::new(values.shape(true)),
            }
        } else {
            ObjectShape::Record(
                self.members
                    .iter()
                    .map(|(name, (count, tally))| {
                        (
                            name.clone(),
                            Member {
                                shape: tally.shape(true),
                                required: *count == self.instances,
                            },
                        )
                    })
                    .collect(),
            )
        }
    }
}

/// A fault in the input to [`TableShape::from_rows`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ShapeError {
    /// Two rows claimed the same path. A directory cannot hold two files of
    /// one name, so the caller has merged two listings.
    #[error("two rows were given for the path {path:?}")]
    DuplicatePath { path: String },
}

/// The observed contract of a table: the shape of its rows, which are always
/// records of columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableShape {
    shape: Shape,
    rows: usize,
}

impl TableShape {
    /// The contract of a table's rows, each given with its path. Rows are
    /// observed in path order, whatever order they are given in, so columns
    /// and nested members take the order they first appear in over the sorted
    /// files. Map-likeness is judged below the row, never of the row itself.
    ///
    /// A table with no rows has no columns: its contract admits only `{}`.
    pub fn from_rows<'a>(
        rows: impl IntoIterator<Item = (&'a str, &'a Map<String, Value>)>,
    ) -> Result<Self, ShapeError> {
        let mut rows: Vec<(&str, &Map<String, Value>)> = rows.into_iter().collect();
        rows.sort_by(|left, right| left.0.cmp(right.0));
        if let Some(pair) = rows.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(ShapeError::DuplicatePath {
                path: pair[0].0.to_string(),
            });
        }
        let mut tally = ObjectTally::default();
        for (_, row) in &rows {
            tally.instances += 1;
            for (name, value) in row.iter() {
                let entry = tally.members.entry(name.clone()).or_default();
                entry.0 += 1;
                entry.1.add(value);
            }
        }
        Ok(Self {
            shape: Shape {
                object: Some(tally.shape(false)),
                ..Shape::bottom()
            },
            rows: rows.len(),
        })
    }

    /// The number of rows observed.
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// The shape of a row.
    pub fn shape(&self) -> &Shape {
        &self.shape
    }

    fn record(&self) -> &IndexMap<String, Member> {
        match &self.shape.object {
            Some(ObjectShape::Record(members)) => members,
            _ => unreachable!("a table shape is built as a record and never widened"),
        }
    }

    /// Column names, in first-observed order over the rows sorted by path.
    pub fn columns(&self) -> Vec<&str> {
        self.record().keys().map(String::as_str).collect()
    }

    /// The columns every row has, in column order.
    pub fn required(&self) -> Vec<&str> {
        self.record()
            .iter()
            .filter(|(_, member)| member.required)
            .map(|(name, _)| name.as_str())
            .collect()
    }

    pub fn column(&self, name: &str) -> Option<&Member> {
        self.record().get(name)
    }

    /// Whether a row is inside the contract.
    pub fn admits_row(&self, row: &Map<String, Value>) -> bool {
        self.shape.admits(&Value::Object(row.clone()))
    }

    /// What would have to widen for the contract to admit a row.
    pub fn describe_row_widening(&self, row: &Map<String, Value>) -> String {
        self.shape.describe_widening(&Value::Object(row.clone()))
    }

    /// The row subschema: a closed object of the observed columns.
    pub fn to_subschema(&self) -> Value {
        self.shape.to_subschema()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    fn infer(texts: &[&str]) -> Shape {
        texts
            .iter()
            .map(|text| Shape::observe(&parse(text)))
            .fold(Shape::bottom(), |shape, next| shape.join(&next))
    }

    /// A JSON Schema validator for an emitted subschema, asserting formats as
    /// the reldir dialect does. `x-reldir-type` is an unknown keyword to a
    /// standard validator and is ignored, which is what makes the lexical
    /// half of the contract `admits`'s to judge.
    fn validator(subschema: &Value) -> jsonschema::Validator {
        jsonschema::options()
            .with_draft(jsonschema::Draft::Draft202012)
            .should_validate_formats(true)
            .build(subschema)
            .unwrap_or_else(|error| panic!("{subschema} does not compile: {error}"))
    }

    fn table(rows: &[(&str, Value)]) -> TableShape {
        let rows: Vec<(String, Map<String, Value>)> = rows
            .iter()
            .map(|(path, row)| (path.to_string(), row.as_object().unwrap().clone()))
            .collect();
        TableShape::from_rows(rows.iter().map(|(path, row)| (path.as_str(), row))).unwrap()
    }

    fn arbitrary_json() -> impl Strategy<Value = Value> {
        let strings = prop_oneof![
            Just("2026-09-14".to_string()),
            Just("2024-02-29".to_string()),
            Just("2026-09-14T12:00:00Z".to_string()),
            Just("2026-09-14T12:00:00.25+02:00".to_string()),
            Just("0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e".to_string()),
            Just("0193B1F4-7C3A-7B1E-9C2D-3F4A5B6C7D8E".to_string()),
            Just("01ARZ3NDEKTSV4RRFFQ69G5FAV".to_string()),
            "[a-z]{0,6}",
        ];
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<i64>().prop_map(|n| Value::Number(n.into())),
            (-1.0e6f64..1.0e6).prop_map(|f| Value::Number(Number::from_f64(f).unwrap())),
            "[1-9][0-9]{19,24}".prop_map(|digits| parse(&digits)),
            strings.prop_map(Value::String),
        ];
        leaf.prop_recursive(4, 48, 5, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::btree_map("[a-e]", inner, 0..4)
                    .prop_map(|members| Value::Object(members.into_iter().collect())),
            ]
        })
    }

    fn arbitrary_shape() -> impl Strategy<Value = Shape> {
        prop::collection::vec(arbitrary_json(), 0..4).prop_map(|values| {
            values.iter().fold(Shape::bottom(), |shape, value| {
                shape.join(&Shape::observe(value))
            })
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, .. ProptestConfig::default() })]

        #[test]
        fn test8600_join_is_commutative(a in arbitrary_shape(), b in arbitrary_shape()) {
            prop_assert_eq!(a.join(&b), b.join(&a));
        }

        #[test]
        fn test8601_join_is_associative(
            a in arbitrary_shape(), b in arbitrary_shape(), c in arbitrary_shape()
        ) {
            prop_assert_eq!(a.join(&b).join(&c), a.join(&b.join(&c)));
        }

        #[test]
        fn test8602_join_is_idempotent_and_bottom_is_its_identity(a in arbitrary_shape()) {
            prop_assert_eq!(a.join(&a), a.clone());
            prop_assert_eq!(a.join(&Shape::bottom()), a.clone());
            prop_assert!(Shape::bottom().le(&a));
        }

        #[test]
        fn test8603_every_observed_value_is_below_the_join_and_admitted(
            values in prop::collection::vec(arbitrary_json(), 1..6)
        ) {
            let joined = values.iter().fold(Shape::bottom(), |s, v| s.join(&Shape::observe(v)));
            for value in &values {
                prop_assert!(Shape::observe(value).le(&joined));
                prop_assert!(joined.admits(value), "{} not admitted", value);
                prop_assert!(joined.widenings(value).is_empty());
            }
        }

        #[test]
        fn test8604_the_emitted_subschema_accepts_every_observed_value(
            values in prop::collection::vec(arbitrary_json(), 1..6)
        ) {
            let joined = values.iter().fold(Shape::bottom(), |s, v| s.join(&Shape::observe(v)));
            let subschema = joined.to_subschema();
            let compiled = validator(&subschema);
            for value in &values {
                prop_assert!(compiled.is_valid(value), "{} rejects {}", subschema, value);
            }
            let inferred = Shape::infer(values.iter());
            let compiled = validator(&inferred.to_subschema());
            for value in &values {
                prop_assert!(inferred.admits(value));
                prop_assert!(compiled.is_valid(value));
            }
        }

        #[test]
        fn test8605_widening_is_described_exactly_when_it_is_needed(
            a in arbitrary_shape(), value in arbitrary_json()
        ) {
            let lines = a.widenings(&value);
            prop_assert_eq!(lines.is_empty(), a.admits(&value), "{:?}", lines);
            let widened = a.widening_for(&value);
            prop_assert!(a.le(&widened));
            prop_assert!(widened.admits(&value));
        }

        #[test]
        fn test8606_order_of_observation_never_changes_the_emitted_schema_of_a_table(
            rows in prop::collection::vec(
                prop::collection::btree_map("[a-f]", arbitrary_json(), 0..5), 1..6
            ),
            seed in any::<u64>()
        ) {
            let named: Vec<(String, Map<String, Value>)> = rows
                .into_iter()
                .enumerate()
                .map(|(index, row)| (format!("t/{index:03}.json"), row.into_iter().collect()))
                .collect();
            let forward = TableShape::from_rows(named.iter().map(|(p, r)| (p.as_str(), r))).unwrap();
            let mut shuffled: Vec<&(String, Map<String, Value>)> = named.iter().collect();
            let len = shuffled.len();
            shuffled.rotate_left((seed as usize) % len);
            if seed % 2 == 0 {
                shuffled.reverse();
            }
            let other = TableShape::from_rows(shuffled.iter().map(|(p, r)| (p.as_str(), r))).unwrap();
            prop_assert_eq!(
                serde_json::to_string(&forward.to_subschema()).unwrap(),
                serde_json::to_string(&other.to_subschema()).unwrap()
            );
            for (_, row) in &named {
                prop_assert!(forward.admits_row(row));
            }
        }

        #[test]
        fn test8607_detected_formats_agree_with_the_validator_that_enforces_them(
            year in 0u32..10000, month in 0u32..14, day in 0u32..33,
            hour in 0u32..26, minute in 0u32..62, second in 0u32..62,
            offset in prop_oneof![Just("Z".to_string()), Just("z".to_string()),
                "[+-][0-2][0-9]:[0-6][0-9]", Just(String::new())],
            separator in prop_oneof![Just('T'), Just('t'), Just(' ')]
        ) {
            let date = format!("{year:04}-{month:02}-{day:02}");
            let stamp = format!("{date}{separator}{hour:02}:{minute:02}:{second:02}{offset}");
            for (text, format) in [(date.as_str(), "date"), (stamp.as_str(), "date-time")] {
                let detected = StringForm::of(text);
                let accepted = validator(&json!({"format": format})).is_valid(&json!(text));
                if matches!(detected, StringForm::Date | StringForm::DateTime) {
                    prop_assert!(accepted, "{} detected as {:?} but the validator refuses it", text, detected);
                }
            }
            if StringForm::of(&stamp) == StringForm::DateTime {
                prop_assert!(chrono::DateTime::parse_from_rfc3339(&stamp).is_ok());
            }
            if StringForm::of(&date) != StringForm::Date {
                let date_format = validator(&json!({"format": "date"}));
                prop_assert!(!date_format.is_valid(&json!(date)), "{} is a date to the validator", date);
            }
        }
    }

    #[test]
    fn test8608_number_spelling_is_a_contract_fact() {
        let ints = infer(&["3", "4"]);
        assert_eq!(ints.number(), Some(NumberClass::Int));
        assert_eq!(
            ints.to_subschema(),
            json!({"type": "integer", "x-reldir-type": "int"})
        );
        let compiled = validator(&ints.to_subschema());
        assert!(!ints.admits(&parse("3.5")));
        assert!(
            !compiled.is_valid(&parse("3.5")),
            "the integer type refuses a fraction"
        );
        assert!(
            !ints.admits(&parse("3.0")),
            "3.0 is a float spelling the int tag refuses"
        );
        assert!(
            compiled.is_valid(&parse("3.0")),
            "JSON Schema cannot see spelling, which is why the tag exists"
        );
        assert!(ints.admits(&parse("-9223372036854775808")));
        assert!(!ints.admits(&parse("9223372036854775808")));

        let floats = infer(&["1.5", "2e3"]);
        assert_eq!(floats.number(), Some(NumberClass::Float));
        assert_eq!(
            floats.to_subschema(),
            json!({"type": "number", "x-reldir-type": "float"})
        );
        assert!(
            !floats.admits(&parse("2")),
            "an integer spelling was never observed"
        );
        assert!(floats.admits(&parse("1E-7")));

        let both = infer(&["1", "1.5"]);
        assert_eq!(both.to_subschema(), json!({"type": "number"}));
        assert!(both.admits(&parse("12345678901234567890123")));

        let big = infer(&["1", "123456789012345678901234567890"]);
        assert_eq!(big.number(), Some(NumberClass::BigInt));
        assert_eq!(big.to_subschema(), json!({"type": "integer"}));
        assert!(big.admits(&parse("-99999999999999999999999")));
        assert!(!big.admits(&parse("0.5")));
        assert!(!validator(&big.to_subschema()).is_valid(&parse("0.5")));
        assert_eq!(big.join(&floats).number(), Some(NumberClass::Number));
    }

    #[test]
    fn test8609_null_types_and_bottom_are_stated_only_as_observed() {
        assert_eq!(Shape::bottom().to_subschema(), json!(false));
        assert!(!Shape::bottom().admits(&json!(null)));
        let only_null = infer(&["null", "null"]);
        assert_eq!(only_null.to_subschema(), json!({"type": "null"}));
        assert!(!only_null.admits(&json!("x")));

        let note = infer(&["\"a\"", "\"b\""]);
        assert!(!note.admits(&json!(null)));
        assert!(!validator(&note.to_subschema()).is_valid(&json!(null)));
        let nullable = note.join(&Shape::observe(&json!(null)));
        assert_eq!(nullable.to_subschema(), json!({"type": ["string", "null"]}));

        let mixed = infer(&["true", "1.5", "\"x\""]);
        assert_eq!(
            mixed.to_subschema(),
            json!({"anyOf": [
                {"type": "boolean"},
                {"type": "number", "x-reldir-type": "float"},
                {"type": "string"}
            ]}),
            "a tagged type keeps its tag in its own branch"
        );
        let merged = infer(&["true", "1", "2.5", "\"2026-01-01\"", "null"]);
        assert_eq!(
            merged.to_subschema(),
            json!({"type": ["boolean", "number", "string", "null"], "format": "date"}),
            "untagged scalars merge, with the one rich type's keywords"
        );
        let structured = infer(&["[1]", "{\"a\": 1}", "null"]);
        assert_eq!(
            structured.to_subschema(),
            json!({"anyOf": [
                {"type": "array", "items": {"type": "integer", "x-reldir-type": "int"}},
                {"type": "object", "properties": {"a": {"type": "integer", "x-reldir-type": "int"}},
                 "required": ["a"], "additionalProperties": false},
                {"type": "null"}
            ]})
        );
        assert_eq!(
            infer(&["\"x\"", "{}"]).to_subschema(),
            json!({"anyOf": [
                {"type": "string"},
                {"type": "object", "additionalProperties": false}
            ]}),
            "an object mixed with a scalar is a union of subschemas, not a type list"
        );
        let compiled = validator(&structured.to_subschema());
        assert!(!compiled.is_valid(&json!("x")));
        assert!(!compiled.is_valid(&json!({"a": 1, "b": 2})));
    }

    #[test]
    fn test8610_objects_are_closed_with_presence_at_every_depth() {
        let shape = infer(&[
            r#"{"id": "a", "meta": {"x": 1, "y": true}}"#,
            r#"{"id": "b", "meta": {"x": 2}, "note": "n"}"#,
        ]);
        assert_eq!(
            shape.to_subschema(),
            json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "meta": {
                        "type": "object",
                        "properties": {
                            "x": {"type": "integer", "x-reldir-type": "int"},
                            "y": {"type": "boolean"}
                        },
                        "required": ["x"],
                        "additionalProperties": false
                    },
                    "note": {"type": "string"}
                },
                "required": ["id", "meta"],
                "additionalProperties": false
            })
        );
        let compiled = validator(&shape.to_subschema());
        let deep_extra = json!({"id": "c", "meta": {"x": 1, "z": 0}});
        assert!(!shape.admits(&deep_extra));
        assert!(!compiled.is_valid(&deep_extra));
        assert_eq!(
            shape.describe_widening(&deep_extra),
            "/meta/z would have to be admitted: no observed object at /meta has this member"
        );
        let missing = json!({"id": "c", "meta": {"y": false}});
        assert!(!compiled.is_valid(&missing));
        assert_eq!(
            shape.widenings(&missing),
            ["/meta/x would have to become optional: every observed object at /meta has it"]
        );
        assert_eq!(
            shape.describe_widening(&json!({"id": "c", "meta": {"x": 1}, "note": null})),
            "/note would have to admit null"
        );
        assert!(shape.admits(&json!({"id": "c", "meta": {"x": 1}})));
    }

    #[test]
    fn test8611_an_always_empty_array_may_stay_empty_but_gains_no_element_type() {
        let shape = infer(&["[]", "[]"]);
        assert_eq!(shape.items(), Some(&Shape::bottom()));
        assert_eq!(
            shape.to_subschema(),
            json!({"type": "array", "items": false})
        );
        let compiled = validator(&shape.to_subschema());
        assert!(compiled.is_valid(&json!([])));
        assert!(!compiled.is_valid(&json!([1])));
        assert_eq!(
            shape.describe_widening(&json!([1])),
            "/0 would have to admit a number (1)"
        );
        let elements = infer(&["[]", "[\"a\"]", "[\"b\", \"c\"]"]);
        assert_eq!(
            elements.to_subschema(),
            json!({"type": "array", "items": {"type": "string"}}),
            "emptiness is not a contract fact: no minItems"
        );
        assert!(elements.admits(&json!([])));
    }

    #[test]
    fn test8612_strong_string_forms_hold_only_when_every_value_has_them() {
        let dates = infer(&["\"2026-01-31\"", "\"2024-02-29\""]);
        assert_eq!(dates.string(), Some(StringForm::Date));
        assert!(!dates.admits(&json!("2026-02-29")), "not a calendar day");
        assert!(!dates.admits(&json!("tomorrow")));
        assert_eq!(
            dates.join(&Shape::observe(&json!("tomorrow"))).string(),
            Some(StringForm::Plain)
        );

        let stamps = infer(&[
            "\"2026-09-14T12:00:00+02:00\"",
            "\"2026-09-14T10:00:00.5Z\"",
        ]);
        assert_eq!(
            stamps.to_subschema(),
            json!({"type": "string", "format": "date-time"})
        );
        assert_eq!(
            StringForm::of("2026-09-14T12:00:00"),
            StringForm::Plain,
            "a local time has no offset and is not a date-time"
        );
        assert_eq!(StringForm::of("2026-09-14 12:00:00Z"), StringForm::Plain);

        let lower = infer(&["\"0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e\""]);
        assert_eq!(
            lower.to_subschema(),
            json!({"type": "string", "format": "uuid"})
        );
        assert!(!lower.admits(&json!("0193B1F4-7C3A-7B1E-9C2D-3F4A5B6C7D8E")));
        let upper = infer(&["\"0193B1F4-7C3A-7B1E-9C2D-3F4A5B6C7D8E\""]);
        assert_eq!(
            upper.to_subschema(),
            json!({"type": "string", "pattern": UUID_UPPER_PATTERN})
        );
        let compiled = validator(&upper.to_subschema());
        assert!(compiled.is_valid(&json!("0193B1F4-7C3A-7B1E-9C2D-3F4A5B6C7D8E")));
        assert!(!compiled.is_valid(&json!("0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e")));
        assert_eq!(
            lower.join(&upper).string(),
            Some(StringForm::Uuid(UuidCase::Mixed))
        );
        assert_eq!(
            StringForm::of("0193b1f4-7C3A-7b1e-9c2d-3f4a5b6c7d8e"),
            StringForm::Uuid(UuidCase::Mixed)
        );
        assert_eq!(
            StringForm::of("0193b1f47c3a7b1e9c2d3f4a5b6c7d8e"),
            StringForm::Plain
        );

        let ulids = infer(&["\"01ARZ3NDEKTSV4RRFFQ69G5FAV\""]);
        assert_eq!(
            ulids.to_subschema(),
            json!({"type": "string", "x-reldir-type": "ulid", "pattern": ULID_PATTERN})
        );
        assert!(!ulids.admits(&json!("01arz3ndektsv4rrffq69g5fav")));
        assert_eq!(
            StringForm::of("81ARZ3NDEKTSV4RRFFQ69G5FAV"),
            StringForm::Plain
        );

        for weak in [
            "a@b.example",
            "https://example.com",
            "example.com",
            "10.0.0.1",
        ] {
            assert_eq!(
                StringForm::of(weak),
                StringForm::Plain,
                "{weak} is not inferred"
            );
        }
        let words = infer(&["\"red\"", "\"red\"", "\"blue\""]);
        assert_eq!(
            words.to_subschema(),
            json!({"type": "string"}),
            "no enum, no length, no pattern"
        );
    }

    #[test]
    fn test8613_objects_keyed_by_data_are_maps_and_records_are_not() {
        // 40 instances, each with its own two names: 80 names, none in more
        // than half the instances.
        let rows: Vec<Value> = (0..40)
            .map(|index| {
                json!({ "counts": {
                    format!("0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c{:04x}", index * 2): index,
                    format!("0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c{:04x}", index * 2 + 1): {"n": 1}
                }})
            })
            .collect();
        let shape = Shape::infer(rows.iter());
        let Some(ObjectShape::Record(top)) = shape.object() else {
            panic!("40 instances with one member each is a record");
        };
        let counts = top["counts"].shape();
        assert!(
            matches!(
                counts.object(),
                Some(ObjectShape::Map {
                    names: StringForm::Uuid(UuidCase::Lower),
                    ..
                })
            ),
            "{counts:?}"
        );
        let subschema = counts.to_subschema();
        assert_eq!(
            subschema["propertyNames"],
            json!({"pattern": UUID_LOWER_PATTERN})
        );
        assert_eq!(
            subschema["additionalProperties"]["anyOf"][0],
            json!({"type": "integer", "x-reldir-type": "int"})
        );
        let compiled = validator(&shape.to_subschema());
        for row in &rows {
            assert!(shape.admits(row));
            assert!(compiled.is_valid(row));
        }
        let new_key = json!({"counts": {"0193b1f4-7c3a-7b1e-9c2d-ffffffffffff": 7}});
        assert!(
            shape.admits(&new_key),
            "a map admits a new key of the observed form"
        );
        assert!(compiled.is_valid(&new_key));
        let bad_key = json!({"counts": {"not-a-uuid": 7}});
        assert!(!shape.admits(&bad_key));
        assert!(!compiled.is_valid(&bad_key));

        // The same names on fewer instances: each name in more than half.
        let dense: Vec<Value> = (0..2)
            .map(|_| Value::Object((0..40).map(|i| (format!("k{i}"), json!(i))).collect()))
            .collect();
        assert!(matches!(
            Shape::infer(dense.iter()).object(),
            Some(ObjectShape::Record(members)) if members.len() == 40
        ));
        // A name in exactly half of the instances is not in more than half.
        let halves: Vec<Value> = (0..2)
            .map(|half| {
                Value::Object(
                    (0..33)
                        .map(|i| (format!("k{half}_{i}"), json!(i)))
                        .collect(),
                )
            })
            .collect();
        assert!(matches!(
            Shape::infer(halves.iter()).object(),
            Some(ObjectShape::Map { .. })
        ));
        // Exactly 32 names is not more than 32.
        let edge: Vec<Value> = (0..32).map(|i| json!({ format!("k{i}"): 1 })).collect();
        assert!(matches!(
            Shape::infer(edge.iter()).object(),
            Some(ObjectShape::Record(_))
        ));
        let over: Vec<Value> = (0..33).map(|i| json!({ format!("k{i}"): 1 })).collect();
        assert!(matches!(
            Shape::infer(over.iter()).object(),
            Some(ObjectShape::Map {
                names: StringForm::Plain,
                ..
            })
        ));
    }

    #[test]
    fn test8614_a_table_is_a_record_of_columns_in_first_observed_file_order() {
        let shape = table(&[
            ("t/b.json", json!({"id": "b", "late": 1, "n": 2})),
            ("t/a.json", json!({"n": 1, "id": "a"})),
        ]);
        assert_eq!(
            shape.columns(),
            ["n", "id", "late"],
            "a.json is observed first"
        );
        assert_eq!(shape.required(), ["n", "id"]);
        assert_eq!(shape.rows(), 2);
        assert!(shape.column("late").is_some_and(|c| !c.required()));
        assert!(!shape.admits_row(json!({"id": "c"}).as_object().unwrap()));
        assert_eq!(
            shape.describe_row_widening(json!({"id": "c", "n": 1.5}).as_object().unwrap()),
            "/n holds only integers within the 64-bit range; it would have to admit 1.5, one of \
             the numbers written with a fraction or exponent"
        );

        let wide: Vec<(String, Value)> = (0..40)
            .map(|i| (format!("t/{i:02}.json"), json!({ format!("c{i}"): i })))
            .collect();
        let refs: Vec<(&str, Value)> = wide.iter().map(|(p, v)| (p.as_str(), v.clone())).collect();
        let sparse = table(&refs);
        assert_eq!(sparse.columns().len(), 40, "a row is never judged a map");

        let empty = TableShape::from_rows(std::iter::empty()).unwrap();
        assert_eq!(
            empty.to_subschema(),
            json!({"type": "object", "additionalProperties": false})
        );

        let row = Map::new();
        assert_eq!(
            TableShape::from_rows([("t/a.json", &row), ("t/a.json", &row)]),
            Err(ShapeError::DuplicatePath {
                path: "t/a.json".into()
            })
        );
    }

    #[test]
    fn test8615_the_dialect_reads_emitted_columns_as_the_intended_relational_types() {
        use crate::schema::{ColumnType, Schema};
        let shape = table(&[
            (
                "t/1.json",
                json!({
                    "id": "0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8e", "n": 1,
                    "at": "2026-09-14T12:00:00Z", "day": "2026-09-14", "ulid": "01ARZ3NDEKTSV4RRFFQ69G5FAV",
                    "tags": ["a"], "meta": {"k": true}, "note": null
                }),
            ),
            (
                "t/2.json",
                json!({
                    "id": "0193b1f4-7c3a-7b1e-9c2d-3f4a5b6c7d8f", "n": 2,
                    "at": "2026-09-14T13:00:00+01:00", "day": "2026-09-15", "ulid": "01ARZ3NDEKTSV4RRFFQ69G5FAW",
                    "tags": [], "meta": {"k": false}, "note": "x"
                }),
            ),
        ]);
        let mut document = shape.to_subschema();
        document["$schema"] = json!(meta::DIALECT_URI);
        document["x-reldir"] = json!({"table": "t", "identity": {"source": "body", "columns": ["id"], "naming": {"rule": "reldir"}}});
        let problems = meta::check_document(&document, None).unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        let schema = Schema::from_document(document, None).unwrap();
        for (column, kind) in [
            ("id", ColumnType::Uuid),
            ("n", ColumnType::Int),
            ("at", ColumnType::Timestamp),
            ("day", ColumnType::Date),
            ("ulid", ColumnType::Ulid),
            ("tags", ColumnType::Array),
            ("meta", ColumnType::Object),
            ("note", ColumnType::String),
        ] {
            assert_eq!(schema.column(column).unwrap().kind(), &kind, "{column}");
        }
        assert!(schema.column("note").unwrap().nullable());
    }
}
