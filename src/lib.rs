// The dialect is one `json!` literal describing the whole schema language, and
// declaring the bound and composition keywords pushed it past the macro
// expander's default depth. The document is more legible as one literal than as
// fragments assembled to suit an expander, so the limit moves rather than the
// dialect.
#![recursion_limit = "256"]

pub mod canonical;
pub mod catalog;
pub mod cli;
pub mod config;
pub mod db;
pub mod diagnostic;
pub mod doctor;
pub mod index;
pub mod infer;
pub mod integrity;
pub mod json;
pub mod lint;
pub mod metadata;
pub mod output;
pub mod schema;
pub mod schema_store;
pub mod sql;
pub mod state;
pub mod transaction;
pub mod value;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const FORMAT_VERSION: u32 = 1;
