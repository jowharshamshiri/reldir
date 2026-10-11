//! Revisions: one accepted state, the revisions it followed, and how it came
//! to be.
//!
//! A revision is a JSON document in its RFC 8785 canonical form -- members
//! sorted, no whitespace, strings escaped minimally, integers only -- and its
//! id is the SHA-256 of those bytes:
//!
//! ```text
//! {"binary_version":"…","idempotency_key":null|"…",
//!  "imported":null|{"format":2,"revision":n},
//!  "lineage":null|{"quarantined":"…","reason":"…"},
//!  "origin":"…","parents":["<id>",…],"reldir_revision":3,
//!  "state":"<root identity>","stats":{"added":n,"modified":n,"removed":n},
//!  "timestamp":"YYYY-MM-DDTHH:MM:SS.nnnnnnnnnZ","transaction_id":null|"…",
//!  "tree":"<root object id>"}
//! ```
//!
//! Parents are sorted, so a merge's id does not depend on the order its heads
//! were found in. Parsing accepts the canonical bytes only: a document that
//! means the same but is spelled differently would have another id, so it is
//! refused rather than normalized.

use super::{
    Hash256, Id, Object, ObjectKind, Store,
    codec::{MAX_JSON_INTEGER, push_json_string},
    error::{Corruption, Invalid, Result, StoreError},
};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;

/// The `reldir_revision` this module reads and writes.
pub const REVISION_FORMAT: u64 = 3;
/// The only format an import records its source in.
pub const IMPORTED_FORMAT: u64 = 2;

/// Why a revision was recorded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    Inference,
    User,
    Internal,
    External,
    Recovery,
    Repair,
    Migration,
    Import,
    SnapshotRestore,
    Merge,
    Upgrade,
}

impl Origin {
    pub const ALL: [Origin; 11] = [
        Origin::Inference,
        Origin::User,
        Origin::Internal,
        Origin::External,
        Origin::Recovery,
        Origin::Repair,
        Origin::Migration,
        Origin::Import,
        Origin::SnapshotRestore,
        Origin::Merge,
        Origin::Upgrade,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Inference => "inference",
            Origin::User => "user",
            Origin::Internal => "internal",
            Origin::External => "external",
            Origin::Recovery => "recovery",
            Origin::Repair => "repair",
            Origin::Migration => "migration",
            Origin::Import => "import",
            Origin::SnapshotRestore => "snapshot_restore",
            Origin::Merge => "merge",
            Origin::Upgrade => "upgrade",
        }
    }

    pub fn parse(text: &str) -> Option<Origin> {
        Origin::ALL.into_iter().find(|o| o.as_str() == text)
    }
}

/// How many rows the revision added, modified and removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stats {
    pub added: u64,
    pub modified: u64,
    pub removed: u64,
}

/// Where a lineage began, when it began over a quarantined history.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lineage {
    pub quarantined: String,
    pub reason: String,
}

/// The format 2 revision an imported history's revision was converted from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Imported {
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    /// Kept sorted and unique: the encoding sorts them, and refuses
    /// duplicates.
    pub parents: Vec<Id>,
    /// The identity of the state's root.
    pub state: Hash256,
    /// The state's root node.
    pub tree: Id,
    pub timestamp: DateTime<Utc>,
    pub origin: Origin,
    pub transaction_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub binary_version: String,
    pub stats: Stats,
    pub lineage: Option<Lineage>,
    pub imported: Option<Imported>,
}

/// A timestamp's one rendering: UTC, `Z`, nine fractional digits.
pub fn render_timestamp(at: &DateTime<Utc>) -> Result<String> {
    let text = at.to_rfc3339_opts(SecondsFormat::Nanos, true);
    // Outside years 0..=9999 chrono writes a sign or a fifth digit, which is
    // not RFC 3339 and would not parse back.
    if text.len() != "0000-00-00T00:00:00.000000000Z".len() {
        return Err(StoreError::Invalid(Invalid::Timestamp(text)));
    }
    Ok(text)
}

/// Parse a timestamp, accepting its one rendering only.
pub fn parse_timestamp(text: &str) -> Option<DateTime<Utc>> {
    let at = DateTime::parse_from_rfc3339(text).ok()?.with_timezone(&Utc);
    (at.to_rfc3339_opts(SecondsFormat::Nanos, true) == text).then_some(at)
}

fn integer(value: u64) -> Result<String> {
    if value > MAX_JSON_INTEGER {
        return Err(StoreError::Invalid(Invalid::IntegerTooLarge(value)));
    }
    Ok(value.to_string())
}

fn push_member(out: &mut String, name: &str, value: &str) {
    if !out.ends_with('{') {
        out.push(',');
    }
    push_json_string(out, name);
    out.push(':');
    out.push_str(value);
}

fn string(text: &str) -> String {
    let mut out = String::new();
    push_json_string(&mut out, text);
    out
}

fn optional_string(text: &Option<String>) -> String {
    text.as_deref().map_or_else(|| "null".into(), string)
}

impl Revision {
    /// The canonical bytes. Members are written in sorted order by hand: the
    /// shape is fixed, so each member's place is too.
    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut parents = self.parents.clone();
        parents.sort_unstable();
        if let Some(pair) = parents.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(StoreError::Invalid(Invalid::DuplicateParent(pair[0])));
        }
        let mut out = String::from("{");
        push_member(&mut out, "binary_version", &string(&self.binary_version));
        push_member(
            &mut out,
            "idempotency_key",
            &optional_string(&self.idempotency_key),
        );
        let imported = match &self.imported {
            None => "null".to_string(),
            Some(imported) => format!(
                "{{\"format\":{IMPORTED_FORMAT},\"revision\":{}}}",
                integer(imported.revision)?
            ),
        };
        push_member(&mut out, "imported", &imported);
        let lineage = match &self.lineage {
            None => "null".to_string(),
            Some(lineage) => {
                let mut inner = String::from("{");
                push_member(&mut inner, "quarantined", &string(&lineage.quarantined));
                push_member(&mut inner, "reason", &string(&lineage.reason));
                inner.push('}');
                inner
            }
        };
        push_member(&mut out, "lineage", &lineage);
        push_member(&mut out, "origin", &string(self.origin.as_str()));
        let parents: Vec<String> = parents.iter().map(|p| string(&p.to_hex())).collect();
        push_member(&mut out, "parents", &format!("[{}]", parents.join(",")));
        push_member(&mut out, "reldir_revision", &REVISION_FORMAT.to_string());
        push_member(&mut out, "state", &string(&self.state.to_hex()));
        let stats = format!(
            "{{\"added\":{},\"modified\":{},\"removed\":{}}}",
            integer(self.stats.added)?,
            integer(self.stats.modified)?,
            integer(self.stats.removed)?
        );
        push_member(&mut out, "stats", &stats);
        push_member(
            &mut out,
            "timestamp",
            &string(&render_timestamp(&self.timestamp)?),
        );
        push_member(
            &mut out,
            "transaction_id",
            &optional_string(&self.transaction_id),
        );
        push_member(&mut out, "tree", &string(&self.tree.to_hex()));
        out.push('}');
        Ok(out.into_bytes())
    }

    pub fn id(&self) -> Result<Id> {
        Ok(Id::of(&self.encode()?))
    }

    pub fn object(&self) -> Result<Object> {
        Ok(Object::new(ObjectKind::Revision, self.encode()?))
    }

    /// Parse a revision's canonical bytes; anything else is refused.
    pub fn decode(bytes: &[u8]) -> Result<Revision, Corruption> {
        let raw: Raw =
            serde_json::from_slice(bytes).map_err(|e| Corruption::Revision(e.to_string()))?;
        if raw.reldir_revision != REVISION_FORMAT {
            return Err(Corruption::UnsupportedVersion(
                u32::try_from(raw.reldir_revision).unwrap_or(u32::MAX),
            ));
        }
        let hex = |what: &str, text: &str| {
            Id::from_hex(text)
                .ok_or_else(|| Corruption::Revision(format!("{what} {text:?} is not an id")))
        };
        let mut parents = vec![];
        for parent in &raw.parents {
            parents.push(hex("parent", parent)?);
        }
        if parents.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(Corruption::Revision("a parent is listed twice".into()));
        }
        let state = Hash256::from_hex(&raw.state).ok_or_else(|| {
            Corruption::Revision(format!("state {:?} is not a digest", raw.state))
        })?;
        let tree = hex("tree", &raw.tree)?;
        let timestamp = parse_timestamp(&raw.timestamp)
            .ok_or_else(|| Corruption::Revision(format!("timestamp {:?}", raw.timestamp)))?;
        let origin = Origin::parse(&raw.origin)
            .ok_or_else(|| Corruption::Revision(format!("unknown origin {:?}", raw.origin)))?;
        let imported = match raw.imported {
            None => None,
            Some(RawImported { format, revision }) if format == IMPORTED_FORMAT => {
                Some(Imported { revision })
            }
            Some(RawImported { format, .. }) => {
                return Err(Corruption::Revision(format!(
                    "imported from format {format}"
                )));
            }
        };
        let revision = Revision {
            parents,
            state,
            tree,
            timestamp,
            origin,
            transaction_id: raw.transaction_id,
            idempotency_key: raw.idempotency_key,
            binary_version: raw.binary_version,
            stats: raw.stats,
            lineage: raw.lineage,
            imported,
        };
        // Everything above parsed leniently where JSON allows -- member order,
        // whitespace, escapes, absent nullable members. Only the canonical
        // spelling is a revision.
        match revision.encode() {
            Ok(canonical) if canonical == bytes => Ok(revision),
            _ => Err(Corruption::NonCanonical),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    reldir_revision: u64,
    parents: Vec<String>,
    state: String,
    tree: String,
    timestamp: String,
    origin: String,
    transaction_id: Option<String>,
    idempotency_key: Option<String>,
    binary_version: String,
    stats: Stats,
    lineage: Option<Lineage>,
    imported: Option<RawImported>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawImported {
    format: u64,
    revision: u64,
}

/// Read and parse a revision.
pub fn load(store: &Store<'_>, id: &Id) -> Result<Revision> {
    let object = store.get(id)?;
    Revision::decode(&object.bytes).map_err(|what| StoreError::CorruptObject { id: *id, what })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use proptest::prelude::*;

    pub(crate) fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).unwrap()
    }

    pub(crate) fn sample() -> Revision {
        Revision {
            parents: vec![Id::of(b"p2"), Id::of(b"p1")],
            state: Hash256::of(b"state"),
            tree: Id::of(b"tree"),
            timestamp: DateTime::from_timestamp(1_790_000_000, 123_456_789).unwrap(),
            origin: Origin::SnapshotRestore,
            transaction_id: Some("tx-1".into()),
            idempotency_key: None,
            binary_version: "1.0.0".into(),
            stats: Stats {
                added: 1,
                modified: 2,
                removed: 3,
            },
            lineage: Some(Lineage {
                quarantined: ".db/history/quarantine/x".into(),
                reason: "a \"reason\"\n".into(),
            }),
            imported: Some(Imported { revision: 41 }),
        }
    }

    #[test]
    fn test8260_a_revision_has_one_documented_spelling() {
        let revision = sample();
        let bytes = revision.encode().unwrap();
        let mut parents = [Id::of(b"p1").to_hex(), Id::of(b"p2").to_hex()];
        parents.sort();
        let expected = format!(
            concat!(
                r#"{{"binary_version":"1.0.0","idempotency_key":null,"#,
                r#""imported":{{"format":2,"revision":41}},"#,
                r#""lineage":{{"quarantined":".db/history/quarantine/x","reason":"a \"reason\"\n"}},"#,
                r#""origin":"snapshot_restore","parents":["{}","{}"],"reldir_revision":3,"#,
                r#""state":"{}","stats":{{"added":1,"modified":2,"removed":3}},"#,
                r#""timestamp":"2026-09-21T14:13:20.123456789Z","transaction_id":"tx-1","tree":"{}"}}"#
            ),
            parents[0],
            parents[1],
            Hash256::of(b"state").to_hex(),
            Id::of(b"tree").to_hex()
        );
        assert_eq!(String::from_utf8(bytes.clone()).unwrap(), expected);
        assert_eq!(revision.id().unwrap(), Id::of(&bytes));
        let decoded = Revision::decode(&bytes).unwrap();
        let mut sorted = revision.clone();
        sorted.parents.sort();
        assert_eq!(decoded, sorted);
        // The members are in sorted order: serde_json agrees key by key.
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let keys: Vec<&String> = value.as_object().unwrap().keys().collect();
        let mut sorted_keys = keys.clone();
        sorted_keys.sort();
        assert_eq!(keys, sorted_keys);
    }

    #[test]
    fn test8261_every_origin_round_trips_and_no_other_is_accepted() {
        for origin in Origin::ALL {
            assert_eq!(Origin::parse(origin.as_str()), Some(origin));
            let revision = Revision { origin, ..sample() };
            assert_eq!(
                Revision::decode(&revision.encode().unwrap())
                    .unwrap()
                    .origin,
                origin
            );
        }
        let text = String::from_utf8(sample().encode().unwrap()).unwrap();
        let bogus = text.replace("snapshot_restore", "telepathy");
        assert!(matches!(
            Revision::decode(bogus.as_bytes()).unwrap_err(),
            Corruption::Revision(message) if message.contains("telepathy")
        ));
    }

    #[test]
    fn test8262_any_other_spelling_of_a_revision_is_refused() {
        let text = String::from_utf8(sample().encode().unwrap()).unwrap();
        let tree = Id::of(b"tree").to_hex();
        let refused = |bytes: &str| Revision::decode(bytes.as_bytes()).unwrap_err();
        // Same meaning, other bytes.
        assert_eq!(
            refused(&text.replace(",\"tree\"", ", \"tree\"")),
            Corruption::NonCanonical
        );
        // serde reads trailing whitespace as JSON; the canonical comparison
        // is what refuses it.
        assert_eq!(refused(&format!("{text}\n")), Corruption::NonCanonical);
        assert_eq!(
            refused(&text.replace("tx-1", "tx\\u002d1")),
            Corruption::NonCanonical
        );
        assert_eq!(
            refused(&text.replace("\"idempotency_key\":null,", "")),
            Corruption::NonCanonical
        );
        let mut parents = [Id::of(b"p1").to_hex(), Id::of(b"p2").to_hex()];
        parents.sort();
        let swapped = text.replace(
            &format!("[\"{}\",\"{}\"]", parents[0], parents[1]),
            &format!("[\"{}\",\"{}\"]", parents[1], parents[0]),
        );
        assert_eq!(
            refused(&swapped),
            Corruption::NonCanonical,
            "unsorted parents"
        );
        let doubled = text.replace(&parents[1], &parents[0]);
        assert_eq!(
            refused(&doubled),
            Corruption::Revision("a parent is listed twice".into())
        );
        assert_eq!(
            refused(&text.replace(
                "\"timestamp\":\"2026-09-21T14:13:20.123456789Z\"",
                "\"timestamp\":\"2026-09-21T14:13:20.123456789+00:00\""
            )),
            Corruption::Revision("timestamp \"2026-09-21T14:13:20.123456789+00:00\"".into())
        );
        assert!(matches!(
            refused(&text.replace(&tree, &tree.to_uppercase())),
            Corruption::Revision(_)
        ));
        // Other meanings.
        assert_eq!(
            refused(&text.replace("\"reldir_revision\":3", "\"reldir_revision\":2")),
            Corruption::UnsupportedVersion(2)
        );
        assert!(matches!(
            refused(&text.replace("\"format\":2", "\"format\":3")),
            Corruption::Revision(_)
        ));
        assert!(matches!(
            refused(&text.replace("\"added\":1", "\"added\":1.0")),
            Corruption::Revision(_)
        ));
        assert!(matches!(
            refused(&text.replace("\"added\":1", "\"added\":-1")),
            Corruption::Revision(_)
        ));
        assert!(matches!(
            refused(&text.replace("{\"binary", "{\"surprise\":1,\"binary")),
            Corruption::Revision(_)
        ));
        assert!(matches!(
            refused(&text.replace("\"tree\":", "\"tree\":\"x\",\"tree\":")),
            Corruption::Revision(_)
        ));
        assert_eq!(
            refused(&text.replace(
                "\"added\":1",
                &format!("\"added\":{}", MAX_JSON_INTEGER + 1)
            )),
            Corruption::NonCanonical
        );
        assert!(matches!(refused("[]"), Corruption::Revision(_)));
        assert!(matches!(refused(""), Corruption::Revision(_)));
    }

    #[test]
    fn test8263_what_has_no_canonical_form_is_not_encoded() {
        let doubled = Revision {
            parents: vec![Id::of(b"p"), Id::of(b"p")],
            ..sample()
        };
        assert!(matches!(
            doubled.encode(),
            Err(StoreError::Invalid(Invalid::DuplicateParent(_)))
        ));
        let huge = Revision {
            stats: Stats {
                added: MAX_JSON_INTEGER + 1,
                ..Stats::default()
            },
            ..sample()
        };
        assert!(matches!(
            huge.encode(),
            Err(StoreError::Invalid(Invalid::IntegerTooLarge(_)))
        ));
        let far = Revision {
            timestamp: DateTime::from_timestamp(300_000_000_000, 0).unwrap(),
            ..sample()
        };
        assert!(matches!(
            far.encode(),
            Err(StoreError::Invalid(Invalid::Timestamp(_)))
        ));
    }

    proptest! {
        #[test]
        fn test8264_arbitrary_text_round_trips_through_the_canonical_form(
            transaction in proptest::option::of("\\PC*|[\\x00-\\x1f\"\\\\\\x7f]{0,8}"),
            binary in "\\PC{0,12}",
            reason in "[\\x00-\\x7f]{0,16}",
            nanos in 0u32..1_000_000_000,
            seconds in 0i64..253_402_300_799,
            added in 0u64..=MAX_JSON_INTEGER,
        ) {
            let revision = Revision {
                parents: vec![],
                transaction_id: transaction,
                binary_version: binary,
                lineage: Some(Lineage { quarantined: "q".into(), reason }),
                timestamp: DateTime::from_timestamp(seconds, nanos).unwrap(),
                stats: Stats { added, ..Stats::default() },
                imported: None,
                ..sample()
            };
            let bytes = revision.encode().unwrap();
            prop_assert_eq!(Revision::decode(&bytes).unwrap(), revision.clone());
            // And serde_json reads the same values from the same bytes.
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            prop_assert_eq!(value["binary_version"].as_str(), Some(revision.binary_version.as_str()));
            prop_assert_eq!(value["transaction_id"].as_str(), revision.transaction_id.as_deref());
        }
    }
}
