use serde::{Deserialize, Serialize};

fn default_enum_max() -> usize {
    10
}
fn default_unique_min() -> usize {
    20
}
fn default_file_size() -> u64 {
    64 * 1024 * 1024
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
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
