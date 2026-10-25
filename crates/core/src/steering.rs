//! Application-owned identities for durable mid-turn user guidance.
use serde::{Deserialize, Serialize};
use uuid::Uuid;
pub const MAX_STEERING_BYTES: usize = 16 * 1024;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringRequest {
    pub id: Uuid,
    pub session_id: Uuid,
    /// Trusted target supplied by the host's currently active worker identity.
    pub turn_id: Uuid,
    pub text: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringState {
    /// Persisted for this turn; not yet added to its agent checkpoint.
    Queued,
    /// Added to durable agent context. This does not claim model compliance.
    Applied,
    /// The target ended before checkpoint application. Draft remains available.
    Cancelled,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringReceipt {
    pub id: Uuid,
    pub session_id: Uuid,
    pub turn_id: Uuid,
    pub state: SteeringState,
    pub reason: Option<String>,
}
