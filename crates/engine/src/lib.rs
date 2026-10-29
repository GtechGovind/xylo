//! Pinned community engine integration, linked only into the task worker.

mod attachments;
mod engine;
mod prompt;
mod provider_guard;
mod providers;
mod steering;
pub mod tools;
pub use engine::HermesEngine;
pub use xylo_runtime::AppPaths;
