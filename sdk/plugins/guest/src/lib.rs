//! Rust guest bindings for the Xylo component ABI.
//!
//! Build guests for `wasm32-unknown-unknown`, then use the SDK packager to wrap
//! their core module as a component. No WASI adapter is installed in the host.
//! Importing a binding never grants permission; each call checks stored grants.

#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "../wit",
        world: "plugin",
        pub_export_macro: true,
        default_bindings_module: "::xylo_plugin_sdk::bindings",
    });
}

pub use bindings::{Guest, export, http_get, read_text, write_text};
