//! Per-broker caches for what dispatch and `search_tools` look up on every
//! call: compiled argument schemas, configured connectors' discovered tools
//! and the BM25 index over everything searchable.
//!
//! Invariants:
//! - A cached value is only ever a function of its key, so a cache hit gives
//!   exactly what recomputing would. Built-in schemas are keyed by tool name
//!   (fixed for a broker's life); connector schemas belong to the connector
//!   snapshot they were discovered with and are replaced along with it.
//! - The connector snapshot is read from the store once per broker and again
//!   after this broker rediscovers a connector (`connector_tools`). Execution
//!   never relies on it: it re-reads the connector, so a connector removed
//!   mid-turn is still refused there.
//! - The search index is keyed by the tool names search may offer and
//!   whether writes are allowed; a mode change or a new connector snapshot
//!   rebuilds it.
use super::catalog;
use parking_lot::Mutex;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};
use xylo_core::{ConnectorConfig, ToolDefinition};

/// A tool's argument schema with its validator, compiled on first use.
pub(crate) struct CompiledSchema {
    schema: Value,
    validator: OnceLock<Option<jsonschema::Validator>>,
}

impl CompiledSchema {
    /// Wraps `schema`; nothing is compiled until [`Self::validator`].
    pub(crate) fn new(schema: Value) -> Self {
        Self {
            schema,
            validator: OnceLock::new(),
        }
    }

    /// The schema as declared.
    pub(crate) fn schema(&self) -> &Value {
        &self.schema
    }

    /// The compiled validator; `None` when the schema does not compile (a
    /// third-party server's mistake), which leaves arguments unchecked.
    pub(crate) fn validator(&self) -> Option<&jsonschema::Validator> {
        self.validator
            .get_or_init(|| {
                jsonschema::options()
                    .should_validate_formats(false)
                    .build(&self.schema)
                    .inspect_err(|error| tracing::debug!("Tool schema not enforced: {error}"))
                    .ok()
            })
            .as_ref()
    }
}

/// Compiled built-in schemas by tool name, shared by a broker and its
/// children (their definitions are the same or a subset).
#[derive(Clone, Default)]
pub(crate) struct SchemaCache(Arc<Mutex<HashMap<String, Arc<CompiledSchema>>>>);

impl SchemaCache {
    /// The compiled schema of `name`, made from `schema` the first time.
    pub(crate) fn get(&self, name: &str, schema: impl FnOnce() -> Value) -> Arc<CompiledSchema> {
        self.0
            .lock()
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(CompiledSchema::new(schema())))
            .clone()
    }
}

/// One discovered connector tool with its compiled schema.
pub(crate) struct ConnectorTool {
    pub definition: ToolDefinition,
    pub schema: Arc<CompiledSchema>,
}

/// A configured connector and its tools; `tools` is `None` for an MCP server
/// never discovered.
pub(crate) struct ConnectorEntry {
    pub config: ConnectorConfig,
    pub tools: Option<Vec<ConnectorTool>>,
}

impl ConnectorEntry {
    /// Pairs `config` with its discovered `tools`.
    pub(crate) fn new(config: ConnectorConfig, tools: Option<Vec<ToolDefinition>>) -> Self {
        let tools = tools.map(|tools| {
            tools
                .into_iter()
                .map(|definition| ConnectorTool {
                    schema: Arc::new(CompiledSchema::new(definition.parameters.clone())),
                    definition,
                })
                .collect()
        });
        Self { config, tools }
    }
}

/// One searchable tool, owned so the index can outlive a single call.
pub(crate) struct SearchEntry {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    /// The connector that serves it; `None` for built-in tools.
    pub connector: Option<ConnectorConfig>,
}

/// What a search index was built for.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SearchKey {
    /// Built-in tools search may offer, in catalog order.
    pub builtin: Vec<String>,
    /// Whether connector tools that write are searchable.
    pub writes: bool,
}

/// A built search index with the entries its ranks refer to.
pub(crate) struct SearchIndex {
    pub key: SearchKey,
    pub entries: Vec<SearchEntry>,
    pub index: catalog::Index,
    /// MCP connectors never discovered, listed with every result.
    pub undiscovered: Vec<ConnectorConfig>,
}

impl SearchIndex {
    /// Indexes `entries` for `key`.
    pub(crate) fn new(
        key: SearchKey,
        entries: Vec<SearchEntry>,
        undiscovered: Vec<ConnectorConfig>,
    ) -> Self {
        let index = catalog::Index::new(
            &entries
                .iter()
                .map(|entry| catalog::Entry {
                    name: &entry.name,
                    description: &entry.description,
                    parameters: &entry.parameters,
                    server: entry.connector.as_ref().map(|config| config.name.as_str()),
                })
                .collect::<Vec<_>>(),
        );
        Self {
            key,
            entries,
            index,
            undiscovered,
        }
    }
}

/// The caches one broker owns.
#[derive(Default)]
pub(crate) struct Lookup {
    pub schemas: SchemaCache,
    /// Configured connectors and their tools; `None` until first read or
    /// after a rediscovery.
    pub connectors: Mutex<Option<Arc<Vec<ConnectorEntry>>>>,
    pub search: Mutex<Option<Arc<SearchIndex>>>,
}

impl Lookup {
    /// A child's caches: the compiled built-in schemas are shared; connector
    /// and search state are its own.
    pub(crate) fn child(&self) -> Self {
        Self {
            schemas: self.schemas.clone(),
            ..Self::default()
        }
    }

    /// Forgets the connector snapshot and the index built from it.
    pub(crate) fn invalidate_connectors(&self) {
        *self.connectors.lock() = None;
        *self.search.lock() = None;
    }
}

#[cfg(test)]
mod tests;
