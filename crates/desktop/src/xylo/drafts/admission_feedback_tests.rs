use super::{Xylo, draft_admission_failure};
use gpui_kit::{AppContext, Bounds, TestAppContext, WindowBounds, WindowOptions, px, size};

#[test]
fn admission_feedback_is_actionable_and_never_echoes_service_payloads() {
    let authorization = draft_admission_failure(
        "ChatGPT plan usage is not authorized; reconnect and allow plan usage",
    );
    assert!(authorization.contains("Providers") && authorization.contains("allow plan usage"));
    let unknown = draft_admission_failure(
        "unexpected response secret_token=DO_NOT_EXPOSE https://private.example/path?token=DO_NOT_EXPOSE",
    );
    assert!(!unknown.contains("DO_NOT_EXPOSE") && !unknown.contains("private.example"));
    assert!(draft_admission_failure("The selected project is unavailable").contains("Reopen"));
    assert!(
        draft_admission_failure("This chat belongs to another project")
            .contains("original project")
    );
}

#[test]
fn mapped_missing_component_reason_survives_the_actual_banner_formatter() {
    let message = draft_admission_failure(
        "A required application component is missing. Reinstall the complete application.",
    );
    assert_eq!(super::super::product_error(&message), message);
    assert!(message.contains("Rebuild") || message.contains("rebuild"));
}

#[test]
fn numeric_request_or_path_fragments_are_not_http_authorization_statuses() {
    assert!(
        draft_admission_failure("database request failed at /tmp/cache-1401/entry")
            .contains("could not be saved")
    );
    assert!(draft_admission_failure("HTTP 401 Unauthorized").contains("Reconnect"));
}

#[gpui_kit::test]
fn failed_old_send_stays_with_its_draft_during_deferred_chat_switch(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1000.), px(680.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
            },
        )
        .unwrap()
    });
    cx.update_window(window, |_, window, cx| view.update(cx, |view, cx| {
        view.validation_draft_surface(None, true, window, cx);
        let original = view.current_draft_key();
        let request = view.drafts.entries[&original].send.as_ref().unwrap().id;
        view.drafts.entries.get_mut(&original).unwrap().dirty = false;
        view.new_chat(cx);
        assert_ne!(view.current_draft_key(), original);
        assert_eq!(view.drafts.active.as_ref(), Some(&original), "Exercise the deferred-switch interval");
        view.error = None;
        view.drafts.entries.get_mut(&original).unwrap().dirty = false;
        view.draft_admitted(request, Err("A required application component is missing. Reinstall the complete application.".into()), window, cx);
        assert!(view.error.is_none(), "An old chat's send failure must not become the new chat's banner");
        assert!(view.drafts.entries[&original].send_error.as_deref().is_some_and(|error| error.contains("missing")));
        view.activate_draft(view.current_draft_key(), window, cx);
        let save_id = uuid::Uuid::new_v4();
        let snapshot = {
            let entry = view.drafts.entries.get_mut(&original).unwrap();
            entry.text = "Newer unsent revision".into();
            entry.generation += 1;
            entry.revision = Some(1);
            entry.dirty = true;
            entry.save = Some(super::SaveAttempt {
                id: save_id, generation: entry.generation,
                write: super::DraftWrite { key: original.clone(), expected_revision: 1, mutation_id: uuid::Uuid::new_v4(), text: entry.text.clone(), attachments: entry.attachments.clone() },
            });
            super::DraftSnapshot { key: original.clone(), text: entry.text.clone(), attachments: entry.attachments.clone(), revision: 2, updated_at: 1 }
        };
        view.draft_saved(save_id, Ok(super::DraftSave::Saved(snapshot)), window, cx);
        assert_eq!(view.drafts.entries[&original].text, "Newer unsent revision");
        assert!(view.drafts.entries[&original].send_error.as_deref().is_some_and(|error| error.contains("missing")), "Autosave acknowledgement must not consume the originating send failure");
    })).unwrap();
}

#[gpui_kit::test]
fn canonical_queue_receipt_clears_only_its_draft_and_transcript_waits_for_start(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    });
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.workspace = Some(std::path::PathBuf::from("/fixture"));
            view.validation_draft_surface(None, true, window, cx);
            let key = view.current_draft_key();
            let entry = view.drafts.entries.get_mut(&key).unwrap();
            entry.revision = Some(7);
            entry.dirty = false;
            let send = entry.send.as_mut().unwrap();
            send.queued = true;
            let id = send.id;
            let mut turn = send.turn.clone();
            turn.turn_id = uuid::Uuid::new_v4();
            let item = xylo_runtime::turn_queue::QueuedTurn {
                id: turn.turn_id,
                turn,
                paused: false,
                reason: None,
            };
            let receipt = xylo_runtime::turn_queue::QueueReceipt {
                item: item.clone(),
                draft: super::DraftSnapshot {
                    key: key.clone(),
                    text: String::new(),
                    attachments: vec![],
                    revision: 8,
                    updated_at: 1,
                },
                cleared: true,
            };
            view.queue_added(uuid::Uuid::new_v4(), Ok(receipt.clone()), window, cx);
            assert!(
                view.turn_queue.items.is_empty(),
                "Unowned receipts must not create ghost queue rows"
            );
            view.queue_added(id, Ok(receipt), window, cx);
            assert!(
                view.messages.is_empty(),
                "Queued messages are not transcript messages yet"
            );
            assert!(view.composer.read(cx).value().is_empty());
            assert_eq!(view.turn_queue.items[&view.session].len(), 1);
            view.composer.update(cx, |input, cx| {
                input.set_value("A newer unsent draft", window, cx)
            });
            view.queued_turn_started(view.session, item.clone(), cx);
            view.queued_turn_started(view.session, item, cx);
            assert_eq!(
                view.messages.len(),
                1,
                "Duplicate start receipts must not duplicate transcript content"
            );
            assert_eq!(
                view.composer.read(cx).value().as_str(),
                "A newer unsent draft"
            );
            assert!(view.turn_queue.items[&view.session].is_empty());
        })
    })
    .unwrap();
}

#[gpui_kit::test]
fn missing_worker_feedback_explains_setup_without_discarding_the_draft(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    Default::default(),
                    size(px(1000.), px(680.)),
                ))),
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
            },
        )
        .unwrap()
    });
    cx.update_window(window, |_, window, cx| view.update(cx, |view, cx| {
        view.validation_draft_surface(None, true, window, cx);
        let key = view.current_draft_key();
        let entry = &view.drafts.entries[&key];
        let request = entry.send.as_ref().unwrap().id;
        let original = entry.text.clone();
        view.draft_admitted(request, Err("A required application component is missing. Reinstall the complete application.".into()), window, cx);
        assert!(view.error.as_deref().unwrap().contains("missing required task components"), "The missing component needs actionable feedback, not a generic send failure");
        assert_eq!(view.drafts.entries[&key].text, original);
        assert_eq!(view.composer.read(cx).value().as_str(), original);
        assert!(!view.draft_send_pending());
        assert!(view.messages.is_empty());
    })).unwrap();
}

#[gpui_kit::test]
fn overdue_admission_unlocks_send_and_a_late_receipt_still_lands_once(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    });
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.workspace = Some(std::path::PathBuf::from("/fixture"));
            view.validation_draft_surface(None, true, window, cx);
            let key = view.current_draft_key();
            let entry = view.drafts.entries.get_mut(&key).unwrap();
            entry.revision = Some(3);
            entry.dirty = false;
            let send = entry.send.as_ref().unwrap();
            let (id, turn_id) = (send.id, send.turn.turn_id);
            assert!(view.draft_send_pending());
            // Admission announced itself, then never confirmed.
            view.turns
                .entry(key.session_id)
                .or_default()
                .apply(xylo_core::EngineEvent::Phase {
                    phase: xylo_core::TurnPhase::Preparing,
                });
            assert!(view.running());
            view.admission_overdue(&key, id, window, cx);
            assert!(
                !view.running(),
                "The activity line clears with the deadline"
            );
            assert!(
                !view.draft_send_pending(),
                "Send unlocks after the deadline"
            );
            assert!(view.drafts.entries[&key].send_error.is_some());
            assert!(view.draft_needs_attention(), "The timeout is shown inline");
            view.draft_admitted(
                id,
                Ok(super::DraftAdmission {
                    turn_id,
                    draft: super::DraftSnapshot {
                        key: key.clone(),
                        text: String::new(),
                        attachments: vec![],
                        revision: 4,
                        updated_at: 1,
                    },
                    cleared: true,
                }),
                window,
                cx,
            );
            let entry = &view.drafts.entries[&key];
            assert!(entry.late.is_none() && entry.send_error.is_none());
            assert!(entry.text.is_empty(), "The admitted draft is cleared");
            assert_eq!(
                view.messages
                    .iter()
                    .filter(|message| message.role == "user")
                    .count(),
                1
            );
        })
    })
    .unwrap();
}

#[gpui_kit::test]
fn enter_explains_why_it_cannot_send(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    });
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.drafts.flush = Some(super::DraftFlushAction::Quit);
            view.submit(window, cx);
            assert!(
                view.composer_hint()
                    .is_some_and(|hint| hint.contains("Keep working"))
            );
            view.drafts.flush = None;
            view.composer
                .update(cx, |input, cx| input.set_value("typing", window, cx));
            view.draft_changed(cx);
            assert!(view.composer_hint().is_none(), "Typing clears the hint");
        })
    })
    .unwrap();
}
