use serde::{Deserialize, Serialize};

pub const BOOTSTRAP_MAX_CONFIG_SIZE: u64 = 64 * 1024 * 1024;

/// Nesting depth allowed before `.db/config` has been read.
///
/// `load_config` must parse the very file that carries `max_nesting_depth`, so
/// the parser needs a bound before the configured one is known. This is that
/// bound: generous enough for any real metadata document, finite so that a
/// hostile file cannot exhaust the stack during bootstrap (Section 57).
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
fn default_temp_disk() -> u64 {
    4 * 1024 * 1024 * 1024
}
fn default_transaction() -> u64 {
    1024 * 1024 * 1024
}
fn default_indentation() -> usize {
    2
}
/// Wait for the writer lock by default.
///
/// The alternative -- failing the instant another writer holds the lock --
/// makes every concurrent caller implement the same retry loop, and a caller
/// that forgets gets spurious failures under load that look like corruption.
/// Five seconds absorbs any ordinary commit while still failing in bounded time
/// when a writer is genuinely stuck.
fn default_wait() -> f64 {
    5.0
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_indentation")]
    pub indentation_width: usize,
    #[serde(default = "default_enum_max")]
    pub enum_max_values: usize,
    #[serde(default = "default_unique_min")]
    pub unique_min_rows: usize,
    #[serde(default = "default_file_size")]
    pub max_json_file_size: u64,
    #[serde(default = "default_depth")]
    pub max_nesting_depth: usize,
    #[serde(default = "default_rows")]
    pub max_result_rows: usize,
    #[serde(default = "default_memory")]
    pub max_query_memory: u64,
    #[serde(default = "default_memory")]
    pub max_sort_memory: u64,
    #[serde(default = "default_temp_disk")]
    pub max_temporary_disk: u64,
    #[serde(default = "default_transaction")]
    pub max_transaction_size: u64,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    /// Seconds to wait for the writer lock before reporting contention.
    ///
    /// Distinct from `timeout_seconds`, which bounds a query. This bounds only
    /// the wait for another writer to finish. Every wait is bounded: there is
    /// no "wait forever", because a stuck writer must not become a hung caller.
    /// Zero means try once and report contention immediately.
    #[serde(default = "default_wait")]
    pub wait_seconds: f64,
    #[serde(default = "default_ignores")]
    pub ignore: Vec<String>,
}

fn default_ignores() -> Vec<String> {
    vec![
        ".DS_Store".into(),
        "*~".into(),
        "*.swp".into(),
        ".gitkeep".into(),
    ]
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
            max_sort_memory: default_memory(),
            max_temporary_disk: default_temp_disk(),
            max_transaction_size: default_transaction(),
            timeout_seconds: None,
            wait_seconds: default_wait(),
            ignore: default_ignores(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ResourceOverrides {
    pub max_json_file_size: Option<u64>,
    pub max_nesting_depth: Option<usize>,
    pub max_result_rows: Option<usize>,
    pub max_query_memory: Option<u64>,
    pub max_sort_memory: Option<u64>,
    pub max_temporary_disk: Option<u64>,
    pub max_transaction_size: Option<u64>,
    pub timeout_seconds: Option<u64>,
    pub wait_seconds: Option<f64>,
}

impl Config {
    pub fn ignore_set(&self) -> std::result::Result<globset::GlobSet, String> {
        let mut builder = globset::GlobSetBuilder::new();
        for pattern in &self.ignore {
            builder.add(
                globset::Glob::new(pattern)
                    .map_err(|error| format!("invalid ignore glob {pattern:?}: {error}"))?,
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
        if let Some(value) = overrides.max_sort_memory {
            self.max_sort_memory = value;
        }
        if let Some(value) = overrides.max_temporary_disk {
            self.max_temporary_disk = value;
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

    /// How long to wait for the writer lock.
    ///
    /// `validate` rejects a negative or non-finite setting, so this conversion
    /// cannot produce a nonsense duration or panic.
    pub fn lock_budget(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.wait_seconds)
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        // Naming the offending key matters more here than anywhere else in the
        // configuration: nine settings share this rule, and a message that
        // listed them all left a reader to guess which one they had set to
        // zero -- and told someone who passed `--timeout 0` about indentation.
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
        } else if self.max_sort_memory == 0 {
            Some("max_sort_memory")
        } else if self.max_temporary_disk == 0 {
            Some("max_temporary_disk")
        } else if self.max_transaction_size == 0 {
            Some("max_transaction_size")
        } else if self.timeout_seconds == Some(0) {
            // Zero is not "no timeout" -- that is `null` -- so it would mean a
            // query that must finish in no time at all.
            Some("timeout_seconds")
        } else {
            None
        };
        // `wait_seconds` is the one setting with a meaningful zero -- try once,
        // report contention -- so it is deliberately not in the check above.
        // What it cannot be is negative or non-finite: `Duration::from_secs_f64`
        // panics on both, and a wait that ran backwards has no interpretation.
        if !self.wait_seconds.is_finite() || self.wait_seconds < 0.0 {
            return Err(format!(
                "wait_seconds must be a finite number of seconds, zero or greater, not {}",
                self.wait_seconds
            ));
        }
        match zero {
            // Only the timeout has a "none" spelling, so only it gets told
            // about one. The others simply have no valid zero.
            Some("timeout_seconds") => Err(
                "timeout_seconds must be greater than zero; use null for no timeout".into(),
            ),
            Some(key) => Err(format!("{key} must be greater than zero")),
            None => self.ignore_set().map(|_| ()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Section 61: resource limits bound real work, so a zero limit is a
    /// nonsensical state that must be rejected rather than silently treated as
    /// "unlimited" or as an immediate failure of every operation.
    #[test]
    fn test1015_zero_limits_are_rejected() {
        for mutate in [
            (|c: &mut Config| c.indentation_width = 0) as fn(&mut Config),
            |c: &mut Config| c.max_json_file_size = 0,
            |c: &mut Config| c.max_nesting_depth = 0,
            |c: &mut Config| c.max_result_rows = 0,
            |c: &mut Config| c.max_query_memory = 0,
            |c: &mut Config| c.max_sort_memory = 0,
            |c: &mut Config| c.max_temporary_disk = 0,
            |c: &mut Config| c.max_transaction_size = 0,
            |c: &mut Config| c.timeout_seconds = Some(0),
        ] {
            let mut config = Config::default();
            mutate(&mut config);
            assert!(
                config.validate().is_err(),
                "a zero limit must be rejected: {config:?}"
            );
        }
        assert!(Config::default().validate().is_ok());

        // `wait_seconds` is deliberately outside that rule: zero means "try
        // once and report contention", which is a real posture a caller asks
        // for, not a nonsensical limit.
        let instant = Config {
            wait_seconds: 0.0,
            ..Default::default()
        };
        assert!(
            instant.validate().is_ok(),
            "zero is a meaningful wait, not a rejected limit"
        );

        // Nine settings share this rule, so the message has to say which one
        // was set to zero. It used to name them collectively -- and told
        // someone who passed `--timeout 0` about indentation width, which is
        // the one thing they had not touched.
        for (key, mutate) in [
            ("indentation_width", (|c: &mut Config| c.indentation_width = 0) as fn(&mut Config)),
            ("max_json_file_size", |c: &mut Config| c.max_json_file_size = 0),
            ("max_nesting_depth", |c: &mut Config| c.max_nesting_depth = 0),
            ("max_result_rows", |c: &mut Config| c.max_result_rows = 0),
            ("max_query_memory", |c: &mut Config| c.max_query_memory = 0),
            ("max_sort_memory", |c: &mut Config| c.max_sort_memory = 0),
            ("max_temporary_disk", |c: &mut Config| c.max_temporary_disk = 0),
            ("max_transaction_size", |c: &mut Config| c.max_transaction_size = 0),
            ("timeout_seconds", |c: &mut Config| c.timeout_seconds = Some(0)),
        ] {
            let mut config = Config::default();
            mutate(&mut config);
            let message = config.validate().expect_err("a zero limit is rejected");
            assert!(
                message.contains(key),
                "the message must name the setting at fault; {key} produced {message:?}"
            );
        }
    }

    /// Section 54: the ignore list is a glob set; an unparsable pattern is an
    /// invalid configuration rather than a pattern that silently matches
    /// nothing.
    #[test]
    fn test1016_invalid_ignore_globs_are_rejected() {
        let mut config = Config {
            ignore: vec!["[unclosed".into()],
            ..Default::default()
        };
        assert!(config.validate().is_err());
        assert!(config.ignore_set().is_err());

        config.ignore = vec!["*.tmp".into(), "build/**".into()];
        let set = config.ignore_set().expect("valid globs compile");
        assert!(set.is_match("scratch.tmp"));
        assert!(set.is_match("build/artifact.json"));
        assert!(!set.is_match("users/u1.json"));
    }

    /// The default ignore list covers the editor artefacts named in Section 54.
    #[test]
    fn test1017_default_ignores_cover_editor_artefacts() {
        let set = Config::default().ignore_set().unwrap();
        for ignored in [".DS_Store", "row.json~", "row.swp", ".gitkeep"] {
            assert!(set.is_match(ignored), "{ignored} should be ignored");
        }
        assert!(!set.is_match("users/u1.json"));
    }

    /// A wait that cannot be turned into a duration must be refused by
    /// `validate`, not by a panic inside `Duration::from_secs_f64` at the
    /// moment a writer tries to take the lock.
    #[test]
    fn test1206_an_unusable_wait_is_rejected_before_it_reaches_a_duration() {
        for unusable in [-1.0, -0.001, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let config = Config {
                wait_seconds: unusable,
                ..Default::default()
            };
            let error = config
                .validate()
                .expect_err("an unusable wait must be rejected: {unusable}");
            assert!(
                error.contains("wait_seconds"),
                "the message must name the setting: {error}"
            );
        }

        // Every accepted wait must convert without panicking, which is the
        // property `validate` exists to guarantee for `lock_budget`.
        for usable in [0.0, 0.001, 5.0, 3600.0] {
            let config = Config {
                wait_seconds: usable,
                ..Default::default()
            };
            config
                .validate()
                .expect("a finite, non-negative wait is usable");
            assert_eq!(config.lock_budget().as_secs_f64(), usable);
        }
    }

    /// A command-line `--wait` must override the stored setting, including
    /// overriding a non-zero stored wait with zero.
    #[test]
    fn test1207_the_wait_override_replaces_the_stored_setting() {
        let mut config = Config::default();
        assert_eq!(config.wait_seconds, 5.0, "the default waits");

        config.apply_overrides(&ResourceOverrides {
            wait_seconds: Some(0.0),
            ..Default::default()
        });
        assert_eq!(
            config.wait_seconds, 0.0,
            "`--wait 0` must reach the lock rather than being read as unset"
        );
        assert_eq!(config.lock_budget(), std::time::Duration::ZERO);

        config.apply_overrides(&ResourceOverrides::default());
        assert_eq!(
            config.wait_seconds, 0.0,
            "an absent override must not restore the default"
        );
    }

    /// Section 61: a command-line limit overrides the stored configuration, and
    /// an unset override leaves the stored value untouched.
    #[test]
    fn test1018_overrides_replace_only_what_they_specify() {
        let mut config = Config {
            max_result_rows: 500,
            timeout_seconds: Some(30),
            ..Default::default()
        };

        config.apply_overrides(&ResourceOverrides {
            max_json_file_size: Some(4096),
            ..Default::default()
        });
        assert_eq!(config.max_json_file_size, 4096);
        assert_eq!(config.max_result_rows, 500, "untouched by the override");
        assert_eq!(config.timeout_seconds, Some(30), "untouched");

        config.apply_overrides(&ResourceOverrides {
            max_result_rows: Some(7),
            timeout_seconds: Some(1),
            ..Default::default()
        });
        assert_eq!(config.max_result_rows, 7);
        assert_eq!(config.timeout_seconds, Some(1));
    }

    /// Configuration is authoritative state (Section 7): an unknown key is a
    /// mistake to surface, not a field to ignore.
    #[test]
    fn test1019_unknown_configuration_keys_are_rejected() {
        let parsed: std::result::Result<Config, _> =
            serde_json::from_str(r#"{"indentation_width":2,"typo_key":1}"#);
        assert!(parsed.is_err());
    }

    /// Omitted keys fall back to documented defaults so an existing database
    /// keeps working when a new limit is introduced.
    #[test]
    fn test1020_omitted_keys_take_documented_defaults() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert_eq!(config.indentation_width, 2);
        assert_eq!(config.enum_max_values, 10);
        assert_eq!(config.unique_min_rows, 20);
        assert_eq!(config.timeout_seconds, None);
        assert!(config.validate().is_ok());
    }
}
