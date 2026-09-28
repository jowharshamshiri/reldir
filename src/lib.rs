// The dialect is one `json!` literal describing the whole schema language, and
// declaring the bound and composition keywords pushed it past the macro
// expander's default depth. The document is more legible as one literal than as
// fragments assembled to suit an expander, so the limit moves rather than the
// dialect.
#![recursion_limit = "256"]

pub mod analysis;
pub mod canonical;
pub mod catalog;
pub mod cli;
pub mod command;
pub mod config;
pub mod db;
pub mod diagnostic;
pub mod doctor;
pub mod fs;
pub mod fuzzing;
pub mod infer;
pub mod integrity;
pub mod json;
pub mod lint;
pub mod locate;
pub mod lock;
pub mod mcp;
pub mod metadata;
pub mod migrate;
pub mod mirror;
pub mod output;
pub mod plan;
pub mod probe;
pub mod referential;
pub mod schema;
pub mod schema_store;
pub mod snapshot;
pub mod sql;
pub mod state;
pub mod transaction;
pub mod value;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The on-disk format this binary reads and writes. Any other version is
/// refused, never interpreted.
pub const FORMAT_VERSION: u32 = 2;
