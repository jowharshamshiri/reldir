//! reldir: a folder of JSON files, governed as a relational database.
//!
//! Each table is a directory, each row a JSON file, each table's rules a JSON
//! Schema. reldir validates every row and every reference, answers SQL over
//! the rows, and writes every change as one validated, recorded transaction
//! -- changing only the bytes the change changes.
//!
//! Applications start at [`Connection`]:
//!
//! ```no_run
//! use reldir_core::{Connection, Options};
//! use serde_json::json;
//!
//! let db = Connection::open("data", Options::default())?;
//! let adults = db.query("SELECT name FROM users WHERE age >= ?", &[json!(18)])?;
//! db.transaction(|tx| {
//!     tx.execute("UPDATE accounts SET balance = balance - 5 WHERE id = 'a'", &[])?;
//!     tx.execute("UPDATE accounts SET balance = balance + 5 WHERE id = 'b'", &[])?;
//!     Ok(())
//! })?;
//! # Ok::<(), reldir_core::Error>(())
//! ```
//!
//! Every failure is an [`Error`]: a registered [`Code`] -- stable, documented,
//! with one exit status -- and the diagnostics behind it, each with its file,
//! line, column and JSON Pointer.
//!
//! The modules below the API are the engine itself: [`db::Database`] observes
//! a folder and commits changes through [`engine::journal`]; [`catalog`] and
//! [`mirror`] hold what was observed; [`schema`], [`integrity`] and
//! [`referential`] judge it; [`store`] and [`history`] record it.

// The dialect is one `json!` literal describing the whole schema language, and
// declaring the bound and composition keywords pushed it past the macro
// expander's default depth. The document is more legible as one literal than as
// fragments assembled to suit an expander, so the limit moves rather than the
// dialect.
#![recursion_limit = "256"]

pub mod analysis;
pub mod api;
pub mod canonical;
pub mod catalog;
pub mod code;
pub mod config;
pub mod contract;
pub mod db;
pub mod diagnostic;
pub mod doctor;
pub mod engine;
#[cfg(any(test, feature = "testing"))]
pub mod fuzzing;
pub mod history;
pub mod index;
pub mod infer;
pub mod integrity;
pub mod json;
pub mod json_edit;
pub mod lint;
pub mod locate;
pub mod lock;
pub mod merge;
pub mod migrate;
pub mod mirror;
pub mod monitor;
pub mod naming;
pub mod plan;
pub mod progress;
pub mod referential;
pub mod rows;
pub mod schema;
pub mod schema_store;
pub mod shape;
pub mod snapshot;
pub mod sql;
pub mod store;

pub mod state;
pub mod transaction;
pub mod upgrade;

pub mod value;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The on-disk format this binary reads and writes. Any other version is
/// refused, never interpreted.
pub const FORMAT_VERSION: u32 = 3;

pub use api::{Changed, Connection, Options, Report, Revision, Rows, Table, Transaction};
pub use code::Code;
/// Every failure: a registered [`Code`] and the diagnostics behind it.
pub use diagnostic::DbError as Error;
