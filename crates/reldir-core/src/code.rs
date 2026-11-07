//! Every diagnostic code reldir emits, with the one exit status it implies.
//!
//! Codes are a stable contract: scripts, agents and the drivers dispatch on
//! them. A code therefore means one thing everywhere, and the exit status a
//! failure carries is a property of its code -- never chosen at the call site,
//! where two sites could disagree about the same failure. The documentation's
//! error table, the SARIF rules and the protocol schema are all generated from
//! this one list, and a test holds them to it.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// A process exit status. Each documented status has one meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Exit {
    /// Success; for `check`, a valid database.
    Ok = 0,
    /// A flag, argument, path, setting or input the command cannot accept.
    Usage = 1,
    /// The database is invalid, or a change was refused because it would make
    /// it so.
    Invalid = 2,
    /// Another writer, or the files themselves, moved under the command;
    /// nothing was written and a retry is safe.
    Contention = 3,
    /// A name that does not resolve, or a query that cannot be answered as
    /// asked: outside the SQL surface, or beyond its configured limits.
    Unresolved = 4,
    /// An interrupted transaction cannot be completed without a decision.
    Recovery = 5,
    /// The format, history or metadata cannot be trusted, or I/O failed.
    Untrusted = 6,
    /// Findings under `--strict`.
    Findings = 7,
    /// Inference could not describe the data.
    Inference = 8,
    /// A decision is required: confirm, choose a policy, or authorise.
    Decision = 9,
    /// No database here, and `--no-auto` forbade establishing one.
    Uninitialized = 10,
    /// The filesystem cannot keep the commit protocol's promises.
    UnsafeFilesystem = 11,
    /// The change is committed and visible, but finishing it on disk failed;
    /// recovery completes it. Do not retry the change.
    CommittedRecoveryPending = 12,
    /// Whether the change committed cannot be known; inspect before retrying.
    CommitUnknown = 13,
    /// reldir itself failed: a defect, reported with what it was doing.
    Internal = 70,
}

impl Exit {
    pub fn code(self) -> i32 {
        self as i32
    }

    /// Every status, in numeric order.
    pub const ALL: [Exit; 15] = [
        Exit::Ok,
        Exit::Usage,
        Exit::Invalid,
        Exit::Contention,
        Exit::Unresolved,
        Exit::Recovery,
        Exit::Untrusted,
        Exit::Findings,
        Exit::Inference,
        Exit::Decision,
        Exit::Uninitialized,
        Exit::UnsafeFilesystem,
        Exit::CommittedRecoveryPending,
        Exit::CommitUnknown,
        Exit::Internal,
    ];

    /// What the status means, as documented.
    pub fn meaning(self) -> &'static str {
        match self {
            Exit::Ok => "success; for `check`, a valid database",
            Exit::Usage => "a flag, argument, path, setting or input the command cannot accept",
            Exit::Invalid => {
                "the database is invalid, or a change was refused because it would make it so"
            }
            Exit::Contention => {
                "another writer or an outside edit moved what the command depended on; nothing was \
                 written, and retrying is safe"
            }
            Exit::Unresolved => {
                "a name that does not resolve, or a query that cannot be answered as asked -- \
                 outside the SQL surface, or beyond its configured limits"
            }
            Exit::Recovery => "an interrupted transaction cannot be completed without a decision",
            Exit::Untrusted => "the format, history or metadata cannot be trusted, or I/O failed",
            Exit::Findings => "findings under `--strict`",
            Exit::Inference => "inference could not describe the data",
            Exit::Decision => {
                "a decision is required: confirm with `--yes`, choose a policy, or authorise"
            }
            Exit::Uninitialized => "there is no database here, and `--no-auto` forbade making one",
            Exit::UnsafeFilesystem => "the filesystem cannot keep the commit protocol's promises",
            Exit::CommittedRecoveryPending => {
                "the change is committed and visible, but finishing it on disk failed; recovery \
                 completes it -- do not retry the change"
            }
            Exit::CommitUnknown => {
                "whether the change committed cannot be known; inspect the database before retrying"
            }
            Exit::Internal => "reldir itself failed; please report it",
        }
    }
}

/// The section of the documentation a code belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Group {
    Opening,
    Structure,
    Schema,
    Contract,
    Row,
    Integrity,
    Change,
    Query,
    Transaction,
    History,
    Inference,
    Lint,
    Update,
    Internal,
}

impl Group {
    pub fn title(self) -> &'static str {
        match self {
            Group::Opening => "Finding and opening a database",
            Group::Structure => "Structure",
            Group::Schema => "Schemas",
            Group::Contract => "The observed contract",
            Group::Row => "Rows",
            Group::Integrity => "Keys, references and rules",
            Group::Change => "Changes",
            Group::Query => "Queries and names",
            Group::Transaction => "Transactions and concurrency",
            Group::History => "History and snapshots",
            Group::Inference => "Inference",
            Group::Lint => "Lint",
            Group::Update => "Updating reldir",
            Group::Internal => "reldir itself",
        }
    }
}

macro_rules! codes {
    ($( $name:ident => $group:ident, $exit:ident, $summary:literal; )*) => {
        /// A diagnostic code. Spelled in the protocol exactly as the variant.
        #[allow(non_camel_case_types)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Code { $( $name, )* }

        impl Code {
            /// Every code, in declaration order.
            pub const ALL: &'static [Code] = &[ $( Code::$name, )* ];

            pub fn as_str(self) -> &'static str {
                match self { $( Code::$name => stringify!($name), )* }
            }

            /// The exit status a failure with this code carries.
            pub fn exit(self) -> Exit {
                match self { $( Code::$name => Exit::$exit, )* }
            }

            pub fn group(self) -> Group {
                match self { $( Code::$name => Group::$group, )* }
            }

            /// One line saying what the code means.
            pub fn summary(self) -> &'static str {
                match self { $( Code::$name => $summary, )* }
            }

            pub fn parse(text: &str) -> Option<Code> {
                match text { $( stringify!($name) => Some(Code::$name), )* _ => None }
            }
        }
    };
}

codes! {
    // Finding and opening a database.
    PATH_NOT_FOUND => Opening, Usage, "`--db` or `RELDIR_DB` names something that does not exist";
    PATH_NOT_DIRECTORY => Opening, Usage, "`--db` or `RELDIR_DB` names a file";
    ROOT_AMBIGUOUS => Opening, Usage, "a folder of pins without `.db/` sits inside another database; name the one you mean";
    ROOT_JSON_AMBIGUOUS => Opening, Usage, "JSON files at the root belong to no table";
    UNINITIALIZED => Opening, Uninitialized, "there is no `.db/`, and nothing may create one";
    ALREADY_INITIALIZED => Opening, Usage, "`init` where `.db/` exists";
    FORMAT_UNSUPPORTED => Opening, Untrusted, "`.db/` or a revision is in a format this binary does not read";
    FORMAT_MISSING => Opening, Untrusted, "`.db/` has no format marker and holds history or a transaction; `--rebuild-metadata` discards the history";
    FORMAT_UPGRADE_REQUIRED => Opening, Untrusted, "`.db/` is in format 2; `reldir upgrade` converts it";
    CONFIG_INVALID => Opening, Usage, "`.db/config` is unreadable, too large, or holds an unknown or unusable setting";
    UNSAFE_FILESYSTEM => Opening, UnsafeFilesystem, "a write on a network, cluster, FUSE, FAT, shared-folder or unrecognised filesystem without `allow_remote_filesystem`";
    READ_ONLY => Opening, Usage, "a change through a database opened read-only";
    USAGE => Opening, Usage, "an argument the command cannot accept";
    INPUT_INVALID => Opening, Usage, "input given to a command is not what it accepts (malformed JSON, CSV or JSON Lines)";
    IO_ERROR => Opening, Untrusted, "a file operation failed; the path is named";

    // Structure.
    UNEXPECTED_FILE => Structure, Invalid, "a file in a table or schema directory that is not `.json`";
    UNGOVERNED_DIRECTORY => Structure, Invalid, "a top-level directory no schema governs";
    NON_REGULAR_FILE => Structure, Invalid, "a symlink, socket, FIFO, device or hard-linked file where a row or schema belongs";
    PATH_COLLISION => Structure, Invalid, "two schema names equal under case or Unicode folding";
    PORTABILITY_COLLISION => Structure, Invalid, "two row files whose names are equal under case or Unicode folding, which one filesystem keeps apart and another merges";
    PATH_VIOLATION => Structure, Usage, "a path that escapes the root or reaches into reldir's metadata, or an unusable name";
    FILENAME_TOO_LONG => Structure, Invalid, "a row's identity renders to a file name longer than the filesystem allows";
    FILENAME_RULE_UNDETERMINED => Structure, Invalid, "a key needs a character the table's file names have never used, so no name can be chosen without inventing a convention";
    CROSS_DEVICE => Structure, UnsafeFilesystem, "a table directory is on another device than `.db/`, so a change cannot be renamed into place atomically";

    // Schemas.
    SCHEMA_INVALID_JSON => Schema, Invalid, "the schema file is not JSON";
    SCHEMA_MISSING_REQUIRED => Schema, Invalid, "a required member of the schema is absent";
    SCHEMA_UNKNOWN_KEY => Schema, Invalid, "a member that is no keyword, at any depth; the message names the nearest one";
    SCHEMA_UNKNOWN_FORMAT => Schema, Invalid, "a `format` that is no format JSON Schema defines";
    SCHEMA_GRAMMAR => Schema, Invalid, "the document breaks the dialect's grammar, or does not compile";
    SCHEMA_REF_EXTERNAL => Schema, Invalid, "a `$ref` leaves the document";
    SCHEMA_TABLE_NAME_MISMATCH => Schema, Invalid, "`x-reldir.table` is not the file's name";
    SCHEMA_INVALID_TABLE_NAME => Schema, Invalid, "a table name breaks the naming rule";
    SCHEMA_TYPE_UNKNOWN => Schema, Invalid, "an unknown column type";
    SCHEMA_TYPE_TAG_CONFLICT => Schema, Invalid, "an `x-reldir-type` tag contradicts the subschema's `type`";
    SCHEMA_COLUMN_UNKNOWN => Schema, Invalid, "a constraint names a column that does not exist";
    SCHEMA_COLUMN_EXISTS => Schema, Invalid, "a migration adds a column that exists";
    SCHEMA_TABLE_EXISTS => Schema, Invalid, "a migration adds a table that exists";
    SCHEMA_CONSTRAINT_EXISTS => Schema, Invalid, "a migration adds a constraint that exists";
    SCHEMA_IDENTITY_INVALID => Schema, Invalid, "`x-reldir.identity` names unknown, repeated, nullable, optional or non-scalar columns";
    SCHEMA_DEFAULT_TYPE_MISMATCH => Schema, Invalid, "a default or generator does not fit its column, or a column has both";
    SCHEMA_CONSTRAINT_NAME_DUPLICATE => Schema, Invalid, "two constraints share a name";
    SCHEMA_REFERENCE_PATH_INVALID => Schema, Invalid, "a reference path does not parse, or cannot reach a scalar through the schema";
    SCHEMA_FK_TARGET_MISSING => Schema, Invalid, "a foreign key's target table or domain has no schema";
    SCHEMA_FK_TARGET_INVALID => Schema, Invalid, "a foreign key's target is malformed";
    SCHEMA_FK_TARGET_NOT_UNIQUE => Schema, Invalid, "a foreign key's target columns are no key of the target";
    SCHEMA_TARGET_KEYS_OVERLAP => Schema, Invalid, "the tables a reference may name do not share one key namespace";
    SCHEMA_FK_TYPE_MISMATCH => Schema, Invalid, "a reference cannot equal the key it names";
    SCHEMA_FK_ACTION_INVALID => Schema, Invalid, "a referential action could write a value the schema or contract forbids";
    SCHEMA_KEY_NOT_SCALAR => Schema, Invalid, "a key column is an array, object or json";
    SCHEMA_DOMAIN_KEY_INVALID => Schema, Invalid, "the tables of an identity domain do not key on one comparable column";
    SCHEMA_ACYCLIC_INVALID => Schema, Invalid, "an acyclic graph's edges do not lead from a row to keys of its own table";
    SCHEMA_CHECK_INVALID => Schema, Invalid, "a check is not a deterministic expression over the row's own columns";
    SCHEMA_ASSERTION_INVALID => Schema, Invalid, "an assertion is not a deterministic query returning the keys of rows that break it";
    SCHEMA_CONFLICT => Schema, Invalid, "inference disagrees with an existing schema, and the policy is to fail";

    // The observed contract.
    CONTRACT_WIDENING_REQUIRED => Contract, Invalid, "a change would write a value the table's contract has never admitted; widening it is a decision";
    CONTRACT_EXCEEDED => Contract, Invalid, "a file edited outside reldir holds a value the table's inferred contract never admitted";
    CONTRACT_REDERIVED => Contract, Ok, "an inferred contract's file was missing and was restored, byte for byte, from recorded history";

    // Rows.
    INVALID_JSON => Row, Invalid, "the file is not one JSON document";
    MERGE_CONFLICT => Row, Invalid, "the file holds a merge's conflict markers: both sides of a merge changed a member differently";
    ROW_ROOT_NOT_OBJECT => Row, Invalid, "a row file holds something other than one JSON object";
    SCHEMA_VIOLATION => Row, Invalid, "a value breaks its subschema";
    TYPE_MISMATCH => Row, Invalid, "a value is not of its column's type";
    ROW_MISSING_FIELD => Row, Invalid, "a required member is absent";
    ROW_UNKNOWN_FIELD => Row, Invalid, "a member the schema does not declare";
    NOT_NULL_VIOLATION => Row, Invalid, "a null where the column admits none";
    IDENTITY_MISMATCH => Row, Invalid, "a file is not named by its row's identity";
    KEY_COLLISION => Row, Invalid, "two rows' identities name one file";
    FILE_TOO_LARGE => Row, Invalid, "a file is larger than `max_json_file_size`, so it is not read";
    NESTING_TOO_DEEP => Row, Invalid, "a document nests deeper than `max_nesting_depth`, so it is not read";

    // Keys, references and rules.
    PRIMARY_KEY_VIOLATION => Integrity, Invalid, "two rows share a primary key";
    UNIQUE_VIOLATION => Integrity, Invalid, "two rows share a unique key";
    DOMAIN_KEY_VIOLATION => Integrity, Invalid, "two tables of one identity domain hold the same key";
    FOREIGN_KEY_VIOLATION => Integrity, Invalid, "a reference names no row, or a change would leave one that does not";
    CYCLE_VIOLATION => Integrity, Invalid, "a graph declared acyclic holds a cycle";
    CHECK_VIOLATION => Integrity, Invalid, "a row fails one of its table's checks";
    ASSERTION_VIOLATION => Integrity, Invalid, "a row is named by an assertion over many rows";

    // Changes.
    MUTATION_CONFLICT => Change, Invalid, "a change names one file twice, or two changes to one row disagree";
    MIGRATION_REFUSED => Change, Decision, "a migration cannot be carried out exactly as the schema is written; the message says what to edit by hand";
    RESOURCE_LIMIT => Query, Unresolved, "a configured limit was exceeded; the answer is refused, never truncated";
    DECISION_REQUIRED => Change, Decision, "the command needs a confirmation, a policy or an authorisation it was not given";

    // Queries and names.
    QUERY_UNSUPPORTED => Query, Unresolved, "a statement outside the SQL surface, or one that does not parse";
    QUERY_TYPE_ERROR => Query, Unresolved, "a value SQL produced cannot be stored in its column";
    UNKNOWN_TABLE => Query, Unresolved, "no such table";
    UNKNOWN_COLUMN => Query, Unresolved, "no such column";
    UNKNOWN_ROW => Query, Unresolved, "no row has that key";
    UNKNOWN_CONSTRAINT => Query, Unresolved, "no constraint has that name";
    UNKNOWN_REVISION => Query, Unresolved, "no revision has that number or id";
    UNKNOWN_SNAPSHOT => Query, Unresolved, "no snapshot has that name";
    UNKNOWN_RESOURCE => Query, Unresolved, "no MCP resource has that URI";
    CURSOR_STALE => Query, Contention, "a page cursor belongs to a state the database has since left";

    // Transactions and concurrency.
    LOCK_CONTENDED => Transaction, Contention, "another writer held the lock past the wait";
    CONCURRENT_MODIFICATION => Transaction, Contention, "a file a change was planned against moved before it was written";
    PATH_INTERFERENCE => Transaction, Contention, "a path a change needs is not what it must be (a directory where a file goes, a symlink ancestor)";
    PLAN_CHANGED => Transaction, Contention, "a planned change no longer matches the state it would be committed to";
    TRANSACTION_INCOMPLETE => Transaction, Recovery, "an interrupted transaction's journal cannot be read";
    RECOVERY_BLOCKED => Transaction, Recovery, "recovery could not place a committed file; the path and the failure are named";
    COMMITTED_RECOVERY_PENDING => Transaction, CommittedRecoveryPending, "the change is committed and visible, and finishing it on disk failed; do not retry it";
    COMMIT_UNKNOWN => Transaction, CommitUnknown, "making the commit durable failed and could not be retracted";
    TRANSACTION_EXPIRED => Transaction, Contention, "a transaction held the writer lock longer than `transaction_timeout_seconds` and was rolled back";
    TRANSACTION_STATEMENT => Transaction, Usage, "a statement that cannot run inside a transaction, or transaction control out of place";
    SNAPSHOT_READ => Transaction, Ok, "a writer was committing, so a read answered from the database as of its last commit";

    // History and snapshots.
    INTERNAL_METADATA_CORRUPT => History, Untrusted, "history, an object, the mirror or other metadata fails verification";
    HISTORY_DIVERGED => History, Ok, "history has more than one head; the next recording merges them";
    SNAPSHOT_EXISTS => History, Usage, "a snapshot with that name exists";

    // Inference.
    INFER_NO_ROWS => Inference, Inference, "a table has no rows to infer from";
    INFER_INVALID_JSON => Inference, Inference, "a file inference must read is not JSON";
    INFER_ROOT_NOT_OBJECT => Inference, Inference, "a file inference must read is not one JSON object";
    INFER_NON_JSON_FILE => Inference, Inference, "a table directory holds a file that is not `.json`";
    INFER_NESTED_DIRECTORY => Inference, Inference, "a table directory holds a directory";
    INFER_NON_REGULAR_FILE => Inference, Inference, "a table directory holds a symlink, device or hard-linked file";
    INFER_KEY_INVALID => Inference, Inference, "a column chosen with `--pk` is absent or null in a row, so it cannot identify every row";
    INFER_FILENAME_INCONSISTENT => Inference, Inference, "the key chosen with `--pk` does not name the table's files, and adopting a folder never renames them";

    // Lint.
    LINT_SCHEMA_UNPINNED => Lint, Findings, "the table is governed by an inferred contract nobody pinned";
    LINT_NULLABLE_NEVER_NULL => Lint, Findings, "a column admits null, and no row is null";
    LINT_WIDER_TYPE => Lint, Findings, "a column's values all have a narrower type than it declares";
    LINT_ENUM_CANDIDATE => Lint, Findings, "a string column with few distinct values";
    LINT_UNIQUE_CANDIDATE => Lint, Findings, "every row holds a different value";
    LINT_CHECK_CANDIDATE => Lint, Findings, "an int that is never negative, a string that is never empty";
    LINT_MIN_ITEMS_CANDIDATE => Lint, Findings, "an array that is never empty";
    LINT_FK_CANDIDATE => Lint, Findings, "values at some path all name rows of one table, and no foreign key says so";
    LINT_DOMAIN_CANDIDATE => Lint, Findings, "values at some path each name a row of one of several tables whose keys never collide";
    LINT_FK_ACTION_DEFAULTED => Lint, Findings, "a foreign key does not say what happens when its target is deleted or re-keyed";
    LINT_PK_NOT_GENERATED => Lint, Findings, "a uuid or ulid key every insert must supply";
    LINT_COLUMN_NEVER_POPULATED => Lint, Findings, "no row holds a value in a column";
    LINT_INCONSISTENT_PRESENCE => Lint, Findings, "some rows have a column and others do not";
    LINT_ADDITIONAL_FIELDS_ALLOWED => Lint, Findings, "the schema accepts members it does not declare";
    LINT_NON_CANONICAL_FORMATTING => Lint, Findings, "rows not in their table's recorded style";
    LINT_NO_DESCRIPTION => Lint, Findings, "a table or column without a description";

    // Updating reldir.
    RELEASE_UNREACHABLE => Update, Untrusted, "the releases could not be read or downloaded: the network, a proxy or GitHub refused";
    RELEASE_NOT_FOUND => Update, Unresolved, "no release has that version, or the release holds no build for this platform";
    RELEASE_UNVERIFIED => Update, Untrusted, "a downloaded release does not match its published checksum, or does not run as reldir";

    // reldir itself.
    INTERNAL_ERROR => Internal, Internal, "reldir failed; the message says what it was doing";
}

impl fmt::Display for Code {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for Code {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Code {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Code::parse(&text)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown diagnostic code {text:?}")))
    }
}

impl PartialEq<&str> for Code {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test8900_every_code_round_trips_through_its_spelling() {
        for code in Code::ALL {
            assert_eq!(Code::parse(code.as_str()), Some(*code));
            let json = serde_json::to_value(code).unwrap();
            assert_eq!(json, serde_json::Value::String(code.as_str().into()));
            assert_eq!(serde_json::from_value::<Code>(json).unwrap(), *code);
        }
        assert_eq!(Code::parse("NOT_A_CODE"), None);
    }

    #[test]
    fn test8901_every_code_has_a_summary_and_codes_are_unique() {
        let mut seen = std::collections::BTreeSet::new();
        for code in Code::ALL {
            assert!(seen.insert(code.as_str()), "{code} is declared twice");
            assert!(!code.summary().is_empty());
            assert!(
                code.as_str()
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_'),
                "{code} is not SCREAMING_SNAKE_CASE"
            );
        }
    }

    #[test]
    fn test8902_exit_statuses_are_distinct_and_documented() {
        let numbers: std::collections::BTreeSet<i32> = Exit::ALL.iter().map(|e| e.code()).collect();
        assert_eq!(numbers.len(), Exit::ALL.len());
        for exit in Exit::ALL {
            assert!(!exit.meaning().is_empty());
        }
    }
}
