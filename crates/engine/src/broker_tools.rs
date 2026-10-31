//! The agent loop's view of a [`ToolBroker`].
//!
//! Scheduling follows each call's effective classification: a tool's static
//! `writes` flag, a `call_tool`'s target, and for `connector_call` the target
//! tool's classification when Xylo can vouch for it
//! ([`ToolBroker::connector_call_reads`]). Anything unknown is a write.
use crate::{
    agent_loop::{Access, ToolExecutor},
    model::ToolSchema,
    tools::{ToolBroker, lookup::CompiledSchema},
};
use serde_json::Value;
use std::sync::Arc;

/// A root broker, or a delegated child's broker with its journal namespace.
pub(crate) struct BrokerTools {
    broker: Arc<ToolBroker>,
    /// The parent `agent_delegate` call key; `None` for the root agent.
    namespace: Option<String>,
}

impl BrokerTools {
    pub(crate) fn root(broker: Arc<ToolBroker>) -> Self {
        Self {
            broker,
            namespace: None,
        }
    }

    pub(crate) fn child(broker: Arc<ToolBroker>, namespace: String) -> Self {
        Self {
            broker,
            namespace: Some(namespace),
        }
    }

    /// Children cannot delegate further.
    fn offered(&self, name: &str) -> bool {
        self.namespace.is_none() || name != "agent_delegate"
    }
}

impl ToolExecutor for BrokerTools {
    fn schemas(&self) -> Vec<ToolSchema> {
        self.broker
            .initial_definitions()
            .into_iter()
            .filter(|definition| self.offered(&definition.name))
            .map(|definition| ToolSchema {
                name: definition.name,
                description: definition.description,
                parameters: definition.parameters,
            })
            .collect()
    }

    fn deferred_count(&self) -> usize {
        let offered = self.broker.initial_definitions().len();
        self.broker.definitions().len().saturating_sub(offered)
    }

    fn access(&self, name: &str, arguments: &Value) -> Option<Access> {
        if !self.offered(name) {
            return None;
        }
        if name == "call_tool" {
            // An unknown or recursive target is refused by the broker; until
            // then it is treated as writing so it never runs concurrently.
            let target = arguments
                .get("name")
                .and_then(Value::as_str)
                .filter(|target| *target != "call_tool");
            let nested = arguments.get("arguments").unwrap_or(&Value::Null);
            return Some(
                target
                    .and_then(|target| self.access(target, nested))
                    .unwrap_or(Access::Writes),
            );
        }
        self.broker
            .definitions()
            .iter()
            .find(|definition| definition.name == name)
            .map(|definition| {
                let reads = !definition.writes
                    || (name == "connector_call" && self.broker.connector_call_reads(arguments));
                if reads {
                    Access::ReadOnly
                } else {
                    Access::Writes
                }
            })
    }

    fn parameters(&self, name: &str) -> Option<Arc<CompiledSchema>> {
        if !self.offered(name) {
            return None;
        }
        self.broker.compiled_parameters(name)
    }

    fn names(&self) -> Vec<String> {
        self.broker
            .definitions()
            .iter()
            .filter(|definition| self.offered(&definition.name))
            .map(|definition| definition.name.clone())
            .collect()
    }

    fn connector_tools(&self, arguments: &Value) -> Option<Vec<(String, Arc<CompiledSchema>)>> {
        self.broker.connector_catalog(arguments)
    }

    fn execute(&self, call_id: &str, name: &str, arguments: Value) -> anyhow::Result<String> {
        match &self.namespace {
            Some(namespace) => self
                .broker
                .execute_child(namespace, call_id, name, arguments),
            None => self.broker.execute(call_id, name, arguments),
        }
    }
}
