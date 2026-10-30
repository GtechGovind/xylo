//! Pinned community engine integration, linked only into the task worker.

mod attachments;
mod credentials;
mod engine;
mod prompt;
mod provider_guard;
mod providers;
mod steering;
pub mod tools;

pub use engine::HermesEngine;
pub use credentials::ProviderCredentialResolver;
pub use hermes_core::AgentError as ProviderCredentialError;
pub use xylo_runtime::AppPaths;
