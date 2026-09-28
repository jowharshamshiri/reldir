//! `.db/config`: operational settings a person chooses.
//!
//! Configuration is authoritative -- it is versioned beside the rows -- so an
//! unknown key is a mistake to report, never a field to ignore, and every
//! setting is checked before it is used.

use serde::{Deserialize, Serialize};

pub const BOOTSTRAP_MAX_CONFIG_SIZE: u64 = 64 * 1024 * 1024;

/// Nesting depth allowed before `.db/config` has been read. `load_config`
/// must parse the file that carries `max_nesting_depth`, so the parser needs a
/// bound before the configured one is known: generous for any real document,
/// finite so a hostile file cannot exhaust the stack.
pub const BOOTSTRAP_MAX_NESTING_DEPTH: usize = 1024;

fn default_enum_max() -> usize {
    10
}
fn default_unique_min() -> usize {
    20
}
fn default_file_size() -> u64 {
    BOOTSTRAP_MAX_CONFIG_SIZE
}
fn default_depth() -> usize {
    128
}
fn default_rows() -> usize {
    1_000_000
}
fn default_memory() -> u64 {
    256 * 1024 * 1024
}
fn default_transaction() -> u64 {
    1024 * 1024 * 1024
}
fn default_indentation() -> usize {
    2
}
/// Wait for the writer lock by default: five seconds absorbs any ordinary
/// commit while still failing in bounded time when a writer is stuck.
fn default_wait() -> f64 {
    5.0
}
fn default_reference_min_values() -> usize {
    1
}
/// Column names that announce a reference, with `{table}` standing for a
/// table's name and `{singular}` for its name without a trailing `s`.
fn default_reference_naming() -> Vec<String> {
    [
        "{singular}_id",
        "{table}_id",
        "{singular}_ids",
        "{singular}_ref",
        "{singular}_refs",
        "{table}_ref",
        "{table}_refs",
        "{singular}",
        "{table}",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}
fn default_ignores() -> Vec<String> {
    vec![".DS_Store".into(), "*~".into(), "*.swp".into(), ".gitkeep".into()]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Spaces per indentation level when reldir writes a file.
    #[serde(default = "default_indentation")]
    pub indentation_width: usize,
    /// Inference makes a string column an enum only with at most this many
    /// distinct values.
    #[serde(default = "default_enum_max")]
    pub enum_max_values: usize,
    /// Inference declares a unique constraint only over at least this many rows.
    #[serde(default = "default_unique_min")]
    pub unique_min_rows: usize,
    #[serde(default = "default_file_size")]
    pub max_json_file_size: u64,
    #[serde(default = "default_depth")]
    pub max_nesting_depth: usize,
    #[serde(default = "default_rows")]
    pub max_result_rows: usize,
    /// Bytes a query may allocate, including sorting and temporary storage.
    #[serde(default = "default_memory")]
    pub max_query_memory: u64,
    #[serde(default = "default_transaction")]
    pub max_transaction_size: u64,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    /// Seconds to wait for the writer lock before reporting contention. Zero
    /// means try once. Every wait is bounded.
    #[serde(default = "default_wait")]
    pub wait_seconds: f64,
    #[serde(default = "default_ignores")]
    pub ignore: Vec<String>,
    /// Permit writes on a network or FUSE filesystem, whose locking and
    /// renames reldir cannot verify. A decision about a known filesystem.
    #[serde(default)]
    pub allow_remote_filesystem: bool,
    /// Column-name patterns inference treats as announcing a reference.
    #[serde(default = "default_reference_naming")]
    pub reference_naming: Vec<String>,
    /// Distinct resolving values a column must hold before its references
    /// are proposed.
    #[serde(default = "default_reference_min_values")]
    pub reference_min_values: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            indentation_width: default_indentation(),
            enum_max_values: default_enum_max(),
            unique_min_rows: default_unique_min(),
            max_json_file_size: default_file_size(),
            max_nesting_depth: default_depth(),
            max_result_rows: default_rows(),
            max_query_memory: default_memory(),
            max_transaction_size: default_transaction(),
            timeout_seconds: None,
            wait_seconds: default_wait(),
            ignore: default_ignores(),
            allow_remote_filesystem: false,
            reference_naming: default_reference_naming(),
            reference_min_values: default_reference_min_values(),
        }
    }
}

/// Settings a command line overrides for one invocation.
#[derive(Debug, Clone, Default)]
pub struct ResourceOverrides {
    pub max_json_file_size: Option<u64>,
    pub max_nesting_depth: Option<usize>,
    pub max_result_rows: Option<usize>,
    pub max_query_memory: Option<u64>,
    pub max_transaction_size: Option<u64>,
    pub timeout_seconds: Option<u64>,
    pub wait_seconds: Option<f64>,
}

impl Config {
    pub fn ignore_set(&self) -> std::result::Result<globset::GlobSet, String> {
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in &self.ignore {
            builder.add(
                globset::Glob::new(pattern).map_err(|error| format!("invalid ignore glob {pattern:?}: {error}"))?,
            );
        }
        builder.build().map_err(|error| error.to_string())
    }

    pub fn apply_overrides(&mut self, overrides: &ResourceOverrides) {
        if let Some(value) = overrides.max_json_file_size {
            self.max_json_file_size = value;
        }
        if let Some(value) = overrides.max_nesting_depth {
            self.max_nesting_depth = value;
        }
        if let Some(value) = overrides.max_result_rows {
            self.max_result_rows = value;
        }
        if let Some(value) = overrides.max_query_memory {
            self.max_query_memory = value;
        }
        if let Some(value) = overrides.max_transaction_size {
            self.max_transaction_size = value;
        }
        if overrides.timeout_seconds.is_some() {
            self.timeout_seconds = overrides.timeout_seconds;
        }
        if let Some(value) = overrides.wait_seconds {
            self.wait_seconds = value;
        }
    }

    /// How long to wait for the writer lock. `validate` has refused a negative
    /// or non-finite wait, so this cannot panic.
    pub fn lock_budget(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.wait_seconds)
    }

    /// The column names `reference_naming` produces for one table.
    pub fn reference_names(&self, table: &str) -> Vec<String> {
        let singular = table.strip_suffix('s').unwrap_or(table);
        self.reference_naming
            .iter()
            .map(|pattern| pattern.replace("{singular}", singular).replace("{table}", table))
            .collect()
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        let zero: Option<&str> = if self.indentation_width == 0 {
            Some("indentation_width")
        } else if self.max_json_file_size == 0 {
            Some("max_json_file_size")
        } else if self.max_nesting_depth == 0 {
            Some("max_nesting_depth")
        } else if self.max_result_rows == 0 {
            Some("max_result_rows")
        } else if self.max_query_memory == 0 {
            Some("max_query_memory")
        } else if self.max_transaction_size == 0 {
            Some("max_transaction_size")
        } else if self.reference_min_values == 0 {
            Some("reference_min_values")
        } else if self.timeout_seconds == Some(0) {
            Some("timeout_seconds")
        } else {
            None
        };
        if !self.wait_seconds.is_finite() || self.wait_seconds < 0.0 {
            return Err(format!(
                "wait_seconds must be a finite number of seconds, zero or greater, not {}",
                self.wait_seconds
            ));
        }
        for pattern in &self.reference_naming {
            let placeholders = pattern.matches('{').count();
            let known = pattern.matches("{table}").count() + pattern.matches("{singular}").count();
            if placeholders != known || known == 0 {
                return Err(format!(
                    "reference_naming pattern {pattern:?} must name the table with {{table}} or \
                     {{singular}}, and no other placeholder"
                ));
            }
        }
        match zero {
            Some("timeout_seconds") => {
                Err("timeout_seconds must be greater than zero; use null for no timeout".into())
            }
            Some(key) => Err(format!("{key} must be greater than zero")),
            None => self.ignore_set().map(|_| ()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test1015_zero_limits_are_rejected_and_named() {
        for (key, mutate) in [
            ("indentation_width", (|c: &mut Config| c.indentation_width = 0) as fn(&mut Config)),
            ("max_json_file_size", |c: &mut Config| c.max_json_file_size = 0),
            ("max_nesting_depth", |c: &mut Config| c.max_nesting_depth = 0),
            ("max_result_rows", |c: &mut Config| c.max_result_rows = 0),
            ("max_query_memory", |c: &mut Config| c.max_query_memory = 0),
            ("max_transaction_size", |c: &mut Config| c.max_transaction_size = 0),
            ("reference_min_values", |c: &mut Config| c.reference_min_values = 0),
            ("timeout_seconds", |c: &mut Config| c.timeout_seconds = Some(0)),
        ] {
            let mut config = Config::default();
            mutate(&mut config);
            let message = config.validate().expect_err("a zero limit is rejected");
            assert!(message.contains(key), "{key}: {message}");
        }
        assert!(Config::default().validate().is_ok());
        assert!(Config { wait_seconds: 0.0, ..Default::default() }.validate().is_ok(), "zero is a meaningful wait");
    }

    #[test]
    fn test1016_invalid_ignore_globs_are_rejected() {
        let mut config = Config { ignore: vec!["[unclosed".into()], ..Default::default() };
        assert!(config.validate().is_err());
        config.ignore = vec!["*.tmp".into(), "build/**".into()];
        let set = config.ignore_set().expect("valid globs compile");
        assert!(set.is_match("scratch.tmp"));
        assert!(set.is_match("build/artifact.json"));
        assert!(!set.is_match("users/u1.json"));
    }

    #[test]
    fn test1206_an_unusable_wait_is_rejected_before_it_reaches_a_duration() {
        for unusable in [-1.0, -0.001, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let config = Config { wait_seconds: unusable, ..Default::default() };
            assert!(config.validate().expect_err("unusable").contains("wait_seconds"));
        }
        for usable in [0.0, 0.001, 5.0, 3600.0] {
            let config = Config { wait_seconds: usable, ..Default::default() };
            config.validate().unwrap();
            assert_eq!(config.lock_budget().as_secs_f64(), usable);
        }
    }

    #[test]
    fn test1018_overrides_replace_only_what_they_specify() {
        let mut config = Config { max_result_rows: 500, timeout_seconds: Some(30), ..Default::default() };
        config.apply_overrides(&ResourceOverrides { max_json_file_size: Some(4096), ..Default::default() });
        assert_eq!(config.max_json_file_size, 4096);
        assert_eq!(config.max_result_rows, 500);
        assert_eq!(config.timeout_seconds, Some(30));
        config.apply_overrides(&ResourceOverrides { wait_seconds: Some(0.0), ..Default::default() });
        assert_eq!(config.lock_budget(), std::time::Duration::ZERO, "`--wait 0` is honoured, not read as unset");
    }

    #[test]
    fn test1019_unknown_configuration_keys_are_rejected() {
        assert!(serde_json::from_str::<Config>(r#"{"indentation_width":2,"typo_key":1}"#).is_err());
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.indentation_width, 2);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test2150_reference_names_expand_per_table_and_are_validated() {
        let config = Config::default();
        let names = config.reference_names("subjects");
        for expected in ["subject_id", "subjects_id", "subject_refs", "subject_ref", "subjects"] {
            assert!(names.contains(&expected.to_string()), "{expected} in {names:?}");
        }
        let bad = Config { reference_naming: vec!["{tabel}_refs".into()], ..Default::default() };
        assert!(bad.validate().unwrap_err().contains("{tabel}_refs"));
        let bare = Config { reference_naming: vec!["refs".into()], ..Default::default() };
        assert!(bare.validate().is_err(), "a pattern that names no table matches every table");
    }
}
