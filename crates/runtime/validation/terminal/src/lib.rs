//! Narrow validation of the actual native terminal implementation. Local macOS
//! results do not qualify Linux PTY teardown; the native Linux jobs do that.
//! Existing production tests, deadlines and opt-in tests are imported unchanged.
#[path = "../../../src/terminal.rs"]
pub mod terminal;
