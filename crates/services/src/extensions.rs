//! Portable extension packages. Discovery never executes package code.
//! Installed resources are immutable snapshots; readiness is per component.
mod catalog;
mod manifest;
mod sources;
pub use catalog::*;
pub use manifest::*;
pub use sources::*;

mod runtime;
pub use runtime::*;

#[cfg(test)]
mod tests;
