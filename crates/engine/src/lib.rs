//! Xylo's agent engine, linked only into the task worker: the agent loop,
//! model provider adapters, context budgeting, delegated helper agents and
//! the durable tool broker.

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
pub use engine::AgentEngine;
pub use model::ModelError;
pub use xylo_runtime::AppPaths;
