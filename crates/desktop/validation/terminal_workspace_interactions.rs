//! Native workspace controls backed by real, temporary local PTYs. The assist
//! routing assertion emits an explicit view event; it does not invoke a model.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
#[path = "../src/terminal_view.rs"]
mod terminal_view;
#[path = "../src/terminal_workspace.rs"]
mod terminal_workspace;
#[path = "../src/ui.rs"]
mod ui;

#[cfg(unix)]
mod tests {
    use super::{terminal_view::TerminalEvent, terminal_workspace::*};
    use gpui_kit::{
        AnyWindowHandle, App, AppContext, Bounds, Entity, Focusable, TestAppContext, Window,
        WindowBounds, WindowOptions, px, size,
        test::{ElementSnapshot, TestQueryExt, TestWindowExt},
    };
    use std::{
        cell::RefCell,
        rc::Rc,
        time::{Duration, Instant},
    };
    use xylo_runtime::terminal::{TerminalSession, TerminalSnapshot};

    type Fixture = (
        tempfile::TempDir,
        AnyWindowHandle,
        Entity<TerminalWorkspace>,
        tokio::sync::watch::Receiver<TerminalSnapshot>,
    );

    fn open(cx: &mut TestAppContext) -> Fixture {
        cx.executor().allow_parking();
        cx.update(gpui_kit::init);
        let cwd = tempfile::tempdir().unwrap();
        let (session, updates) = TerminalSession::spawn(cwd.path()).unwrap();
        let observed = updates.clone();
        let (window, view) = cx.update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(1280.), px(380.)),
                    ))),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    cx.new(|cx| {
                        TerminalWorkspace::new(
                            cwd.path().to_path_buf(),
                            session,
                            updates,
                            window,
                            cx,
                        )
                    })
                },
            )
            .unwrap()
        });
        cx.update_window(window, |_, window, cx| {
            window.activate_window();
            window.render_frame(cx);
        })
        .unwrap();
        cx.run_until_parked();
        (cwd, window, view, observed)
    }

    fn wait(
        cx: &mut TestAppContext,
        view: &Entity<TerminalWorkspace>,
        description: &str,
        condition: impl Fn(&TerminalWorkspace, &App) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            if cx.update(|cx| condition(view.read(cx), cx)) {
                return;
            }
            assert!(Instant::now() < deadline, "Timed out: {description}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn tab_buttons(window: &Window) -> Vec<ElementSnapshot> {
        window
            .elements()
            .into_iter()
            .filter(|element| {
                element
                    .label()
                    .is_some_and(|label| label.starts_with("Select "))
            })
            .collect()
    }

    #[gpui_kit::test]
    fn real_shell_tabs_split_and_focus_preserve_independent_views(cx: &mut TestAppContext) {
        let (_cwd, window, view, _updates) = open(cx);
        let initial = cx.update(|cx| view.read(cx).active_view().unwrap());
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let add = window.find_by_label("New terminal");
            assert!(add.visible());
            window.click("terminal-new", cx);
        })
        .unwrap();
        wait(cx, &view, "second real shell", |view, _| {
            view.session_count() == 2
        });
        let second = cx.update(|cx| view.read(cx).active_view().unwrap());
        assert_ne!(initial.entity_id(), second.entity_id());
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let tabs = tab_buttons(window);
            assert_eq!(tabs.len(), 2);
            assert_eq!(
                tabs.iter()
                    .filter(|tab| tab
                        .label()
                        .is_some_and(|label| label.ends_with(" (active)")))
                    .count(),
                1
            );
            window.click("terminal-split", cx);
        })
        .unwrap();
        wait(cx, &view, "split real shell", |view, _| {
            view.session_count() == 3
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let left = second.read(cx).focus_handle(cx);
            let right = view.read(cx).active_view().unwrap();
            assert_ne!(right.entity_id(), second.entity_id());
            // Focusing an existing left pane must change the selected tab without
            // swapping the two visible terminal entities.
            window.focus(&left, cx);
            window.render_frame(cx);
        })
        .unwrap();
        wait(cx, &view, "focused existing pane", |view, _| {
            view.active_view().unwrap().entity_id() == second.entity_id()
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            assert_eq!(
                view.read(cx).active_view().unwrap().entity_id(),
                second.entity_id()
            );
            assert_eq!(window.find_all_by_label("✦ Ask Xylo").len(), 2);
            window.click("terminal-unsplit", cx);
            window.render_frame(cx);
            assert_eq!(view.read(cx).session_count(), 3);
            assert_eq!(window.find_all_by_label("✦ Ask Xylo").len(), 1);
            // A hidden tab remains selectable after collapsing the split.
            let tabs = tab_buttons(window);
            window.click(tabs[0].path().last().unwrap().clone(), cx);
            assert_eq!(
                view.read(cx).active_view().unwrap().entity_id(),
                initial.entity_id()
            );
            view.update(cx, |view, cx| view.close_all(cx));
        })
        .unwrap();
        println!("terminal workspace flow: tabs, split, focus, collapse, independent views");
    }

    #[gpui_kit::test]
    fn cancelled_close_keeps_shell_and_confirmed_last_close_emits_empty(cx: &mut TestAppContext) {
        let (_cwd, window, view, updates) = open(cx);
        let observed = Rc::new(RefCell::new(Vec::new()));
        let received = observed.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&view, move |_, event: &TerminalWorkspaceEvent, _| {
                received.borrow_mut().push(event.clone());
            })
        });
        cx.update_window(window, |_, window, cx| {
            let session = view.read(cx).active_session().unwrap();
            session
                .input(b"printf 'workspace-scrollmark\\n'\r".to_vec())
                .unwrap();
            window.render_frame(cx);
            let close = window.find_by_label("Close terminal");
            window.click(close.path().last().unwrap().clone(), cx);
        })
        .unwrap();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Keep terminal");
        cx.run_until_parked();
        assert!(observed.borrow().is_empty());
        cx.update(|cx| {
            assert_eq!(view.read(cx).session_count(), 1);
            assert!(view.read(cx).has_live_sessions());
        });
        wait(cx, &view, "retained shell output", |_, _| {
            updates
                .borrow()
                .cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
                .contains("workspace-scrollmark")
        });
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let close = window.find_by_label("Close terminal");
            window.click(close.path().last().unwrap().clone(), cx);
        })
        .unwrap();
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Close terminal");
        wait(cx, &view, "last close", |view, _| view.session_count() == 0);
        assert_eq!(*observed.borrow(), vec![TerminalWorkspaceEvent::Empty]);
        println!("terminal workspace flow: close cancellation, retained output, confirmed Empty");
    }

    #[gpui_kit::test]
    fn close_all_fences_inflight_start_and_late_close_confirmation(cx: &mut TestAppContext) {
        let (_cwd, window, view, _updates) = open(cx);
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let close = window.find_by_label("Close terminal");
            window.click(close.path().last().unwrap().clone(), cx);
            assert!(view.read(cx).has_live_sessions());
            view.update(cx, |view, cx| {
                view.new_tab(window, cx);
                // The response cannot run until this update returns. Explicitly
                // close first, regardless of the background spawn's wall time.
                view.close_all(cx);
                assert_eq!(view.session_count(), 0);
            });
        })
        .unwrap();
        cx.simulate_prompt_answer("Close terminal");
        let deadline = Instant::now() + Duration::from_millis(400);
        while Instant::now() < deadline {
            cx.run_until_parked();
            std::thread::sleep(Duration::from_millis(10));
        }
        cx.update(|cx| {
            assert_eq!(view.read(cx).session_count(), 0);
            assert!(!view.read(cx).has_live_sessions());
        });
        println!(
            "terminal workspace flow: in-flight spawn and stale close reply cannot resurrect terminals"
        );
    }

    #[gpui_kit::test]
    fn every_new_terminal_forwards_explicit_assistance_event(cx: &mut TestAppContext) {
        let (_cwd, window, view, _updates) = open(cx);
        let observed = Rc::new(RefCell::new(Vec::new()));
        let received = observed.clone();
        let _subscription = cx.update(|cx| {
            cx.subscribe(&view, move |_, event: &TerminalWorkspaceEvent, _| {
                received.borrow_mut().push(event.clone());
            })
        });
        for number in 1..=3 {
            if number > 1 {
                cx.update_window(window, |_, window, cx| {
                    view.update(cx, |view, cx| view.new_tab(window, cx));
                })
                .unwrap();
                wait(cx, &view, "next shell for event routing", |view, _| {
                    view.session_count() == number
                });
            }
            cx.update(|cx| {
                let terminal = view.read(cx).active_view().unwrap();
                terminal.update(cx, |_, cx| {
                    cx.emit(TerminalEvent::AssistRequested {
                        prompt: format!("Explain terminal {number}"),
                    });
                });
            });
            cx.run_until_parked();
        }
        assert_eq!(
            *observed.borrow(),
            (1..=3)
                .map(|number| TerminalWorkspaceEvent::AssistRequested {
                    prompt: format!("Explain terminal {number}"),
                })
                .collect::<Vec<_>>()
        );
        cx.update(|cx| view.update(cx, |view, cx| view.close_all(cx)));
        println!(
            "terminal workspace flow: assistance forwarding from every newly created view (event fixture only)"
        );
    }
}
