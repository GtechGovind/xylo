//! App-owned contracts for reviewed, installed native component plugins.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PluginBudgets {
    pub memory_mib: u32,
    pub fuel: u64,
    pub timeout_seconds: u64,
    pub max_input_bytes: u64,
    pub max_output_bytes: u64,
}

impl Default for PluginBudgets {
    fn default() -> Self {
        Self {
            memory_mib: 32,
            fuel: 10_000_000,
            timeout_seconds: 15,
            max_input_bytes: 256 * 1024,
            max_output_bytes: 256 * 1024,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NativePluginManifest {
    pub format: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub entrypoint: String,
    pub sha256: String,
    pub capabilities: Vec<String>,
    pub budgets: PluginBudgets,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct InstalledPlugin {
    pub manifest: NativePluginManifest,
    pub package_sha256: String,
    pub enabled: bool,
    pub grants: Vec<String>,
    pub installed_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginCatalog {
    pub revision: String,
    pub plugins: Vec<InstalledPlugin>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginInspection {
    pub manifest: NativePluginManifest,
    pub module_bytes: u64,
    pub package_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct PluginManagementResult {
    pub operation_id: Uuid,
    pub catalog: PluginCatalog,
    pub changed: bool,
    pub message: String,
}
