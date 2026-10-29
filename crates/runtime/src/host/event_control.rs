//! Pure correlation gate for worker events before they reach selected UI state.
use uuid::Uuid;
use xylo_core::{EngineEvent, Envelope};

pub(super) fn accepts(active_request: Option<Uuid>, event: &Envelope<EngineEvent>) -> bool {
    active_request == Some(event.request_id)
        || (active_request.is_none() && matches!(event.payload, EngineEvent::Ready { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stale_worker_progress_and_approval_cannot_mutate_a_new_run() {
        let old = Uuid::new_v4();
        let active = Uuid::new_v4();
        for event in [
            EngineEvent::Started,
            EngineEvent::Delta {
                text: "old response".into(),
            },
            EngineEvent::Status {
                text: "old tool".into(),
            },
            EngineEvent::ApprovalRequired {
                action_id: Uuid::new_v4(),
                name: "file_write".into(),
                arguments: json!({}),
                rule: None,
            },
            EngineEvent::Cancelled,
        ] {
            let envelope = Envelope::new(old, None, 0, event);
            assert!(!accepts(Some(active), &envelope));
            assert!(accepts(Some(old), &envelope));
        }
    }

    #[test]
    fn no_active_request_accepts_setup_ready_but_never_execution() {
        let request = Uuid::new_v4();
        let ready = Envelope::new(
            request,
            None,
            0,
            EngineEvent::Ready {
                engine: "fixture".into(),
                revision: "fixture".into(),
            },
        );
        assert!(accepts(None, &ready));
        assert!(!accepts(Some(Uuid::new_v4()), &ready));
        assert!(!accepts(
            None,
            &Envelope::new(
                request,
                None,
                0,
                EngineEvent::Delta {
                    text: "orphan".into()
                }
            )
        ));
    }
}
