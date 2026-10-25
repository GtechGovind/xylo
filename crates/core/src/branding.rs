//! Product identity and deliberately stable credential compatibility.

pub const APPLICATION_NAME: &str = "Xylo";
pub const APPLICATION_ID: &str = "com.qurkos.xylo";

/// Existing account references remain in their original OS credential service.
/// Changing the bundle identity must not silently disconnect saved accounts.
/// This identifier is storage compatibility, not the displayed application name.
pub const CREDENTIAL_SERVICE: &str = "org.agentworkbench";
