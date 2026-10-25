
#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolBatchInterrupt {
    None,
    Stop,
    Steer(String),
}

// ---------------------------------------------------------------------------
// ToolRegistry
// ---------------------------------------------------------------------------

/// A single tool entry in the registry.
#[derive(Clone)]
pub struct ToolEntry {
    /// Application integration: stable identity for durable tool execution.
    pub handler_with_call_id: Option<Arc<dyn Fn(&str, Value) -> Result<String, ToolError> + Send + Sync>>,
    /// The tool's JSON Schema descriptor.
    pub schema: ToolSchema,
    /// A handler function: takes a JSON Value and returns the tool output string.
    pub handler: Arc<dyn Fn(Value) -> Result<String, ToolError> + Send + Sync>,
}

impl std::fmt::Debug for ToolEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolEntry")
            .field("schema", &self.schema)
            .field("handler", &"<function>")
            .finish()
    }
}

/// A simple registry mapping tool names to their schemas and handlers.
///
/// The full-featured implementation lives in `hermes-tools`; this minimal
/// version exists so the agent loop can be tested and used independently.
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, ToolEntry>,
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool.
    pub fn register(
        &mut self,
        name: impl Into<String>,
        schema: ToolSchema,
        handler: Arc<dyn Fn(Value) -> Result<String, ToolError> + Send + Sync>,
    ) {
        self.tools
            .insert(name.into(), ToolEntry { schema, handler, handler_with_call_id: None });
    }

    /// Register an application tool that requires the provider's call identity.
    pub fn register_with_call_id(
        &mut self,
        name: impl Into<String>,
        schema: ToolSchema,
        handler: Arc<dyn Fn(&str, Value) -> Result<String, ToolError> + Send + Sync>,
    ) {
        self.tools.insert(name.into(), ToolEntry {
            schema,
            handler: Arc::new(|_| Err(ToolError::ExecutionFailed("Tool call identity is required".into()))),
            handler_with_call_id: Some(handler),
        });
    }

    /// Look up a tool by name.
    pub fn get(&self, name: &str) -> Option<&ToolEntry> {
        self.tools.get(name)
    }

    /// Return all registered tool schemas.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|e| e.schema.clone()).collect()
    }

    /// Return all registered tool names.
    pub fn names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

