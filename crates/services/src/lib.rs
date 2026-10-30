//! Durable local application services. Blocking APIs must be called off the UI thread.
//! No service contacts a provider or loads embedding assets during construction.

mod drafts;
pub use drafts::*;
mod document_drafts;
pub use document_drafts::*;
mod steering;
pub use steering::*;
mod memory_facts;
pub use memory_facts::*;
mod memory_forms;
pub use memory_forms::*;
mod store;
pub use store::*;
pub mod action_review;
pub use action_review::*;
mod credentials;
pub use credentials::*;
mod memory_records;
pub use memory_records::*;
mod memory;
pub use memory::*;
mod connectors;
pub use connectors::*;
pub mod attachments;
mod history_search;
pub mod image_generation;
mod learning;
pub mod providers;
pub mod extensions;
pub mod sandbox;
pub use history_search::*;
pub use learning::*;

#[cfg(test)]
mod tests;

mod extension_receipts;

mod turn_queue;
pub use turn_queue::*;
