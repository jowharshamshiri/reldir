use serde::{Deserialize, Serialize};

pub const BOOTSTRAP_MAX_CONFIG_SIZE: u64 = 64 * 1024 * 1024;

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
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        let invalid = self.indentation_width == 0
            || self.max_json_file_size == 0
            || self.max_nesting_depth == 0
            || self.max_result_rows == 0
            || self.max_query_memory == 0
            || self.max_sort_memory == 0
            || self.max_temporary_disk == 0
            || self.max_transaction_size == 0
            || self.timeout_seconds == Some(0);
        if invalid {
            Err("indentation and resource limits must be greater than zero".into())
        } else {
            self.ignore_set().map(|_| ())
        }
    }
}
