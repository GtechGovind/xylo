//! Xylo's agent engine, linked only into the task worker: the agent loop,
//! model provider adapters, context budgeting, delegated helper agents and
//! the durable tool broker.
//!
//! The model types are public so an evaluation harness (`xylo-evals`) can
//! script or record a turn's provider through
//! [`AgentEngine::with_provider_layer`]; the worker never installs a layer.

mod agent_loop;
mod attachments;
mod broker_tools;
mod credentials;
mod engine;
mod model;
mod prompts;
mod provider_guard;
mod providers;
mod steering;
mod subagent;
pub mod tools;

pub use credentials::ProviderCredentialResolver;
pub use engine::{AgentEngine, ProviderLayer};
pub use model::{
    Assembler, EventStream, FinishReason, Message, ModelError, ModelProvider, ModelRequest,
    ModelResponse, ModelUsage, NativeContent, Reasoning, Role, StreamEvent, ToolCall, ToolSchema,
};
pub use xylo_runtime::AppPaths;
