//! Controlled offscreen captures of the production Xylo and composer.
//! A real temporary Host/Store loads a synthetic local configuration and empty
//! project. No prompt is sent, no account is read and no device is activated.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
#[path = "../src/assets.rs"]
mod assets;
mod capture_dir;
#[path = "../src/icons.rs"]
mod icons;
#[path = "../src/motion.rs"]
mod motion;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/terminal_view.rs"]
mod terminal_view;
#[path = "../src/terminal_workspace.rs"]
mod terminal_workspace;
#[path = "../src/ui.rs"]
mod ui;
mod xylo {
    include!("../src/xylo.rs");

    #[cfg(test)]
    impl Xylo {
        pub(crate) fn capture_draft_count(&self) -> Option<usize> {
            self.drafts
                .validation_list_request()
                .is_none()
                .then(|| self.drafts.validation_summaries().len())
        }
        pub(crate) fn capture_usage_summary(&mut self, cx: &mut Context<Self>) {
            self.usage_ui.report = Some((
                self.session,
                xylo_runtime::UsageReport {
                    conversation: xylo_runtime::UsageTotals {
                        input_tokens: 12400,
                        output_tokens: 1600,
                        cache_read_tokens: 8100,
                        estimated_usd: 0.0842,
                        ..Default::default()
                    },
                    daily: xylo_runtime::UsageTotals {
                        input_tokens: 784000,
                        output_tokens: 14300,
                        cache_read_tokens: 676000,
                        cache_write_tokens: 108000,
                        estimated_usd: 1.3694,
                        ..Default::default()
                    },
                    ..Default::default()
                },
            ));
            cx.notify();
        }
        pub(crate) fn approved_chat_capture(
            &mut self,
            scene: &str,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.settings_open = false;
            self.status = "".into();
            self.page = Page::Chat;
            self.settings.ui.sidebar_visible = false;
            self.settings.ui.work_panel_open = false;
            self.settings.ui.work_panel = "details".into();
            self.git_panel = Default::default();
            self.settings.permissions = PermissionMode::FullAccess;
            self.turn_queue.items.remove(&self.session);
            self.turns.remove(&self.session);
            self.close_output(cx);
            self.messages.clear();
            for image in self.draft_attachments() {
                self.remove_draft_attachment(image.id, cx);
            }
            self.conversations.retain(|chat| chat.id != self.session);
            self.conversations.push(Conversation {
                id: self.session,
                title: if scene == "empty" {
                    "New chat"
                } else {
                    "A calmer, more useful chat"
                }
                .into(),
                workspace: self.workspace.clone().unwrap(),
                updated_at: now_ms(),
                title_source: Default::default(),
                pinned: false,
                archived: false,
            });
            self.composer
                .update(cx, |input, cx| input.set_value("", window, cx));
            match scene {
                "greeting" => {
                    self.messages = vec![
                        message(self.session, "user", "hi?".into()),
                        message(
                            self.session,
                            "assistant",
                            "Hi! What would you like to work on in Xylo?".into(),
                        ),
                    ]
                }
                "markdown" => {
                    self.messages.push(message(self.session, "assistant", "## Xylo — what it is\n\nA native Rust workspace for conversations and coding (`README.md:25`). The toolchain is pinned in `rust-toolchain.toml`.\n\n## Architecture\n\n- `crates/desktop` owns the interface and uses `gpui-kit`.\n- `xylo-runtime` coordinates the worker and streams `HostEvent` values.\n- Long paths like `crates/desktop/src/xylo/chat/transcript_flow_tests.rs` wrap within the conversation.\n\n```rust\nfn main() {\n    println!(\"Ready\");\n}\n```".into()));
                }
                "edit-preview" => {
                    let turn = self.turns.entry(self.session).or_default();
                    turn.apply(EngineEvent::Started);
                    turn.apply(EngineEvent::Delta {
                        text: "Updated the README with a clearer project overview.".into(),
                    });
                    let id = Uuid::new_v4();
                    turn.apply(EngineEvent::ToolStarted {
                        action_id: id,
                        name: "file_edit".into(),
                        arguments: serde_json::json!({"path":"README.md"}),
                        parent_id: None,
                    });
                    let patch = "--- a/README.md\n+++ b/README.md\n@@ -24,4 +24,7 @@\n ## Why Xylo\n-One workspace for your tasks.\n+A focused workspace for your projects, conversations, and coding tasks. Bring a local model or connect your preferred provider, then keep every useful result close to the work.\n+\n+- Native desktop interface with consistent spacing, readable changes, and compact controls.\n+- Local or cloud models, with your existing tools and project context.\n \n ## Getting started\n";
                    turn.apply(EngineEvent::ToolFinished {
                        action_id: id,
                        name: "file_edit".into(),
                        result: serde_json::json!({"diff":patch}).to_string(),
                        success: true,
                        parent_id: None,
                    });
                    self.expanded_tools.insert(id);
                    self.expanded_tools.insert(Uuid::new_v5(&id, b"tool-group"));
                }
                "tool-alignment" => {
                    let turn = self.turns.entry(self.session).or_default();
                    turn.apply(EngineEvent::Started);
                    for (name, arguments, result, success) in [
                        (
                            "shell_command",
                            serde_json::json!({"command":"cargo test --locked -p xylo --no-fail-fast"}),
                            "Command could not start: the sandbox helper is unavailable.",
                            false,
                        ),
                        (
                            "files_list",
                            serde_json::json!({"path":"docs/modules"}),
                            r#"{"entries":["capabilities.md","core.md","desktop.md","engine.md","helper.md"],"count":5}"#,
                            true,
                        ),
                    ] {
                        let id = Uuid::new_v4();
                        turn.apply(EngineEvent::ToolStarted {
                            action_id: id,
                            name: name.into(),
                            arguments,
                            parent_id: None,
                        });
                        turn.apply(EngineEvent::ToolFinished {
                            action_id: id,
                            name: name.into(),
                            result: result.into(),
                            success,
                            parent_id: None,
                        });
                        self.expanded_tools.insert(id);
                    }
                    self.expanded_tools
                        .insert(Uuid::new_v5(&turn.tools[0].id, b"tool-group"));
                }
                "conversation" => {
                    self.messages.push(message(
                        self.session,
                        "user",
                        "Make the chat simpler to use, without losing the useful features.".into(),
                    ));
                    let mut answer = message(self.session, "assistant", "## The conversation has room to breathe.\n\nI simplified the chat around the parts you use most: the response, the work in progress, and your next message.\n\n- **Quieter activity.** Completed file reads and commands live in one expandable row. Simple replies have no activity control.\n- **A composer that stays ready.** Attach files, choose a model, or write the next message while Xylo works.\n- **Useful details, on demand.** Open command output, inspect changes, and review subtasks without losing your place.\n\nQuestions and errors stay close to the part of the conversation that needs attention.".into());
                    answer.turn_id = Some(Uuid::new_v4());
                    answer.activity_count = 6;
                    answer.activity_summary =
                        Some("Read 3 files · Ran 2 commands · Edited 1 file".into());
                    self.messages.push(answer);
                }
                "activity" => {
                    let turn = self.turns.entry(self.session).or_default();
                    turn.apply(EngineEvent::Started);
                    turn.apply(EngineEvent::Delta {
                        text: "I’ll start by getting an overview of the project.".into(),
                    });
                    turn.apply(EngineEvent::ThinkingDelta { text: " \n".into() });
                    for (name, args) in [
                        ("files_list", serde_json::json!({"path":"."})),
                        ("file_read", serde_json::json!({"path":"README.md"})),
                    ] {
                        let id = Uuid::new_v4();
                        turn.apply(EngineEvent::ToolStarted {
                            action_id: id,
                            name: name.into(),
                            arguments: args,
                            parent_id: None,
                        });
                        turn.apply(EngineEvent::ToolFinished {
                            action_id: id,
                            name: name.into(),
                            result: "{}".into(),
                            success: true,
                            parent_id: None,
                        });
                    }
                    turn.apply(EngineEvent::ThinkingDelta {
                        text:
                            "Check the existing chat components and keep their spacing consistent."
                                .into(),
                    });
                }
                "queue" => self.queued_capture(true, cx),
                "question" => self.local_decision_capture(false, window, cx),
                "plan" => self.local_decision_capture(true, window, cx),
                "empty" => {}
                _ => self.rich_chat_capture(scene, window, cx),
            }
            self.refresh_composer_placeholder(cx);
            gpui_kit::component::WindowExt::clear_notifications(window, cx);
            self.transcript_focus.focus(window, cx);
            self.invalidate_live_history();
            self.sync_history_list(cx);
            if let Some(history) = self.histories.get(&self.session) {
                history.list.scroll_to(gpui_kit::ListOffset {
                    item_ix: 0,
                    offset_in_item: px(0.),
                });
            }
            cx.notify();
        }

        pub(crate) fn capture_model_state(
            &mut self,
            providers: Vec<ProviderConfig>,
            query: &str,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.settings.providers = providers;
            self.model_picker_capture_query(query, window, cx);
            cx.notify();
        }
        pub(crate) fn capture_task_panel(&mut self, scene: &str, cx: &mut Context<Self>) {
            use xylo_runtime::workspace::git::{ChangeGroup, GitChange, GitStatus};
            self.settings.ui.work_panel_open = true;
            self.settings.ui.work_panel_width = if scene == "subagents" { 400. } else { 600. };
            self.settings.ui.work_panel = if scene == "subagents" {
                "details"
            } else {
                "changes"
            }
            .into();
            let root = self.workspace.clone().unwrap();
            self.git_panel = work_panel::GitPanel::prepare(
                root.clone(),
                GitStatus {
                    branch: "main".into(),
                    truncated: false,
                    changes: vec![
                        GitChange {
                            path: "src/chat/composer.rs".into(),
                            original: None,
                            index: ' ',
                            worktree: 'M',
                        },
                        GitChange {
                            path: "src/chat/task_panel.rs".into(),
                            original: None,
                            index: '?',
                            worktree: '?',
                        },
                        GitChange {
                            path: "src/theme/colors.rs".into(),
                            original: None,
                            index: ' ',
                            worktree: 'M',
                        },
                    ],
                },
            );
            if scene == "subagents" {
                let turn = self.turns.entry(self.session).or_default();
                turn.subagents = vec![
                    crate::state::Subagent {
                        id: Uuid::from_u128(100), task: "Review the chat layout and composer spacing".into(), tools: vec![],
                        summary: Some("Aligned the model menu and composer controls. Empty activity rows are hidden.\n\nThe **Context** control stays visible beside Usage.".into()),
                        state: crate::state::SubagentState::Completed, started_at: now_ms() - 48000, finished_at: Some(now_ms() - 4000),
                    },
                    crate::state::Subagent {
                        id: Uuid::from_u128(101), task: "Verify narrow layouts and keyboard navigation".into(), tools: vec![], summary: None,
                        state: crate::state::SubagentState::Running, started_at: now_ms() - 9000, finished_at: None,
                    },
                ];
            }
            if scene == "file-diff" {
                let request = Uuid::new_v4();
                self.git_panel.selected =
                    Some(("src/chat/composer.rs".into(), ChangeGroup::Unstaged));
                self.git_panel.diff_request = Some(request);
                self.git_diff_received(request, root, Ok("diff --git a/src/chat/composer.rs b/src/chat/composer.rs\nindex 123..456 100644\n--- a/src/chat/composer.rs\n+++ b/src/chat/composer.rs\n@@ -12,8 +12,9 @@ fn composer()\n     div()\n-        .gap(px(16.))\n-        .rounded(px(30.))\n+        .gap(px(8.))\n+        .rounded(px(23.))\n         .child(message_input)\n         .child(controls)\n+        .child(context_usage)\n }\n \n fn controls() {\n".into()), cx);
            }
            cx.notify();
        }

        pub(crate) fn capture_context(&mut self, cx: &mut Context<Self>) {
            self.turns.entry(self.session).or_default().context = Some(ContextUsage {
                capacity: 200_000,
                messages: 27_000,
                system: 4_000,
                tools: 3_000,
                tool_count: 12,
                deferred_tool_count: 8,
                autocompact_threshold: 180_000,
                ..Default::default()
            });
            cx.notify();
        }
        pub(crate) fn capture_attachment(&mut self, path: PathBuf, cx: &mut Context<Self>) {
            self.attach_image_paths(vec![path], AttachmentSource::Picker, cx);
        }
        pub(crate) fn capture_attachment_ready(&self) -> bool {
            self.draft_attachments().len() == 1 && !self.attachments.importing_for(self.session)
        }
        pub(crate) fn captured_draft(&self, cx: &App) -> String {
            self.composer.read(cx).value().to_string()
        }
        pub(crate) fn capture_close_output(&mut self, cx: &mut Context<Self>) {
            self.close_output(cx);
        }

        pub(crate) fn composer_capture_ready(&self) -> bool {
            self.settings_loaded
                && self.workspace_ready
                && self
                    .drafts
                    .validation_revision(&xylo_runtime::drafts::DraftKey {
                        session_id: self.session,
                        workspace: self.workspace.clone(),
                    })
                    .is_some()
        }

        pub(crate) fn composer_capture_theme(&self) -> &str {
            &self.settings.theme
        }

        pub(crate) fn composer_capture_prompt(
            &mut self,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.composer.update(cx, |input, cx| {
                input.set_value("Plan the next step for this project.", window, cx);
            });
            self.draft_changed(cx);
            cx.notify();
        }

        pub(crate) fn composer_capture_history(&mut self, populated: bool, cx: &mut Context<Self>) {
            self.messages = if populated {
                (0..300)
                    .map(|index| {
                        message(
                            self.session,
                            if index % 2 == 0 { "user" } else { "assistant" },
                            format!(
                                "History message {index}: {}",
                                "Scrollable conversation text. ".repeat(10)
                            ),
                        )
                    })
                    .collect()
            } else {
                Vec::new()
            };
            self.sync_history_list(cx);
            cx.notify();
        }

        pub(crate) fn provider_capture(
            &mut self,
            custom: bool,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.settings_open = true;
            self.page = Page::Providers;
            self.provider_form_open = true;
            self.choose_provider_kind(if custom { "Custom" } else { "OpenAI" }, window, cx);
            if custom {
                Self::set_input(&self.provider_name, "Custom provider", window, cx);
                Self::set_input(
                    &self.endpoint,
                    "https://provider.example.invalid/v1",
                    window,
                    cx,
                );
                Self::set_input(
                    &self.provider_discovery_url,
                    "https://provider.example.invalid/catalog",
                    window,
                    cx,
                );
                self.provider_headers.update(cx, |input, cx| {
                    input.set_value("{\"X-Project\":\"local-fixture\"}", window, cx)
                });
            }
            cx.notify();
        }

        pub(crate) fn account_capture(
            &mut self,
            ready: bool,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.settings_open = true;
            self.page = Page::Providers;
            self.provider_form_open = true;
            let account = ProviderConfig {
                name: "ChatGPT".into(),
                family: ApiFamily::OpenAiResponses,
                endpoint: "https://api.openai.com/v1".into(),
                credential_ref: Some(CHATGPT_REF.into()),
                ..Default::default()
            };
            self.provider_form(&account, window, cx);
            // A connected, authorized account with no saved ChatGPT connection
            // loads its models as soon as its status arrives, so discovery is
            // captured before the status is delivered.
            self.provider_discovery.test_dispatch = true;
            self.account_result("account_chatgpt_status",&serde_json::json!({"connected":true,"plan_usage_authorized":true,"email":"fixture-account@example.invalid"}),window,cx);
            if ready {
                let (generation, provider) = self
                    .provider_discovery
                    .test_requests
                    .last()
                    .unwrap()
                    .clone();
                self.event(
                    HostEvent::Models {
                        generation,
                        provider,
                        models: vec!["gpt-fixture-model".into(), "gpt-fixture-reasoning".into()],
                        context_limits: Default::default(),
                        reasoning_options: Default::default(),
                        default_reasoning_efforts: Default::default(),
                    },
                    window,
                    cx,
                );
            }
        }

        pub(crate) fn queued_capture(&mut self, active: bool, cx: &mut Context<Self>) {
            self.settings_open = false;
            self.page = Page::Chat;
            self.settings.ui.work_panel_open = false;
            self.messages = vec![
                message(
                    self.session,
                    "user",
                    "Review the project and check the current changes.".into(),
                ),
                message(
                    self.session,
                    "assistant",
                    "I’m reviewing the changed files and checking the affected tests.".into(),
                ),
            ];
            if active {
                self.turns
                    .entry(self.session)
                    .or_default()
                    .apply(EngineEvent::Started);
            } else {
                self.turns.remove(&self.session);
            }
            let items = [
                "Then run the affected tests and report any failures.",
                "Summarize the changes and anything that still needs attention.",
            ]
            .into_iter()
            .map(|prompt| {
                let turn = TurnRequest {
                    session_id: self.session,
                    turn_id: Uuid::new_v4(),
                    workspace: self.workspace.clone().unwrap(),
                    prompt: prompt.into(),
                    provider: self.settings.provider.clone(),
                    permissions: self.settings.permissions,
                    attachments: vec![],
                };
                xylo_runtime::turn_queue::QueuedTurn {
                    id: turn.turn_id,
                    turn,
                    paused: !active,
                    reason: None,
                }
            })
            .collect();
            self.turn_queue.items.insert(self.session, items);
            cx.notify();
        }
        pub(crate) fn reset_queued_capture(&mut self, cx: &mut Context<Self>) {
            self.turns.remove(&self.session);
            self.turn_queue.items.remove(&self.session);
            self.messages.clear();
            cx.notify();
        }
        pub(crate) fn local_decision_capture(
            &mut self,
            plan: bool,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            self.settings_open = false;
            self.page = Page::Chat;
            self.settings.ui.work_panel_open = false;
            self.messages = vec![message(
                self.session,
                "user",
                if plan {
                    "Review a plan before changing the project."
                } else {
                    "Improve this screen and check the result."
                }
                .into(),
            )];
            self.turns.remove(&self.session);
            self.turns
                .entry(self.session)
                .or_default()
                .apply(EngineEvent::Started);
            let id = Uuid::new_v4();
            if plan {
                self.turns.entry(self.session).or_default().apply(EngineEvent::PlanRequired { action_id: id, plan: "1. Inspect the existing screen and its tests.\n2. Improve spacing and keyboard focus.\n3. Verify the changes at compact and wide window sizes.".into() });
                self.event(
                    HostEvent::PlanModes {
                        session: self.session,
                        action_id: id,
                        modes: vec![PermissionMode::AskForApproval, PermissionMode::FullAccess],
                        scheduled: false,
                    },
                    window,
                    cx,
                );
                let input = self.interaction_input(self.session, id, usize::MAX, window, cx);
                input.update(cx, |input, cx| {
                    input.set_value(
                        "Keep the current colors and limit changes to chat.",
                        window,
                        cx,
                    )
                });
            } else {
                self.turns
                    .entry(self.session)
                    .or_default()
                    .apply(EngineEvent::QuestionRequired {
                        action_id: id,
                        questions: vec![xylo_core::UserQuestion {
                            header: "Verification".into(),
                            question: "How should I check this change?".into(),
                            multi_select: false,
                            options: vec![
                                xylo_core::QuestionOption {
                                    label: "Focused tests".into(),
                                    description: "Run the relevant existing tests.".into(),
                                },
                                xylo_core::QuestionOption {
                                    label: "Full suite".into(),
                                    description: "Run all project checks.".into(),
                                },
                            ],
                        }],
                    });
                let input = self.interaction_input(self.session, id, 0, window, cx);
                input.update(cx, |input, cx| {
                    input.set_value(
                        "Run focused tests and inspect the compact layout.",
                        window,
                        cx,
                    )
                });
                self.turns.get_mut(&self.session).unwrap().tools[0].question_answers[0].other =
                    Some("Run focused tests and inspect the compact layout.".into());
            }
            self.composer.update(cx, |input, cx| {
                input.set_value("Keep this separate main draft for later.", window, cx)
            });
            self.draft_changed(cx);
            self.invalidate_live_history();
            self.sync_history_list(cx);
            cx.notify();
        }

        pub(crate) fn composer_popovers_capture(&mut self, cx: &mut Context<Self>) {
            self.settings_open = false;
            self.page = Page::Chat;
            self.settings.ui.work_panel_open = false;
            self.settings.providers = (0..32)
                .map(|index| ProviderConfig {
                    name: if index < 20 { "Alpha" } else { "Beta" }.into(),
                    endpoint: format!(
                        "https://{}.example.invalid/v1",
                        if index < 20 { "alpha" } else { "beta" }
                    ),
                    model: format!("fixture-model-{index}"),
                    context_tokens: 200_000,
                    reasoning_options: Some(vec!["low".into(), "high".into()]),
                    default_reasoning_effort: Some("high".into()),
                    ..Default::default()
                })
                .collect();
            self.settings.provider = self.settings.providers[0].clone();
            self.settings.ui.recent_models = [0, 20]
                .map(|index| RecentModel {
                    endpoint: self.settings.providers[index].endpoint.clone(),
                    model: self.settings.providers[index].model.clone(),
                })
                .into();
            let turn = self.turns.entry(self.session).or_default();
            turn.context = Some(ContextUsage {
                capacity: 200_000,
                system: 6_000,
                tools: 9_000,
                tool_count: 18,
                deferred_tool_count: 40,
                messages: 30_000,
                summary: 5_000,
                compacted: true,
                autocompact_threshold: 160_000,
                ..Default::default()
            });
            turn.rate_limits = Some((
                "fixture-model-0".into(),
                xylo_core::RateLimits {
                    requests: Some(3_999),
                    tokens: Some(2_397_000),
                    input_tokens: Some(1_997_000),
                    output_tokens: Some(400_000),
                    observed_at_ms: now_ms(),
                },
            ));
            self.usage_ui.report = Some((self.session, xylo_runtime::UsageReport::default()));
            cx.notify();
        }
        pub(crate) fn rich_chat_capture(
            &mut self,
            scene: &str,
            _window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            use crate::state::{Subagent, SubagentState};
            self.settings_open = false;
            self.page = Page::Chat;
            self.settings.ui.work_panel_open = false;
            self.settings.permissions = PermissionMode::FullAccess;
            self.turn_queue.items.remove(&self.session);
            self.close_output(cx);
            self.messages = vec![message(
                self.session,
                "user",
                "Review the changes and show me the result.".into(),
            )];
            self.turns.remove(&self.session);
            let turn_id = Uuid::new_v4();
            let turn = self.turns.entry(self.session).or_default();
            turn.apply(EngineEvent::Started);
            turn.request = Some(TurnRequest {
                session_id: self.session,
                turn_id,
                workspace: self.workspace.clone().unwrap(),
                prompt: "Review changes".into(),
                provider: self.settings.provider.clone(),
                permissions: PermissionMode::FullAccess,
                attachments: vec![],
            });
            if scene == "answer" {
                turn.apply(EngineEvent::Completed { text: "The chat now keeps the answer easy to read.\n\n| Check | Result |\n| --- | --- |\n| Keyboard navigation | Passed |\n| Draft preservation | Passed |\n\n```rust\nfn send_next(message: Message) {\n    queue.push(message);\n}\n```\n\nLong answers keep their reading position while new work arrives.\n\n".repeat(5), usage: Default::default() });
            } else if scene == "helpers" {
                for n in 0..3 {
                    let id = Uuid::new_v4();
                    turn.apply(EngineEvent::ToolStarted {
                        action_id: id,
                        name: "agent_delegate".into(),
                        arguments: serde_json::json!({"task":format!("Read-only code review of {}", ["the shell command lifecycle and cancellation", "chat rendering and subagent result presentation", "the current changes and their test coverage"][n])}),
                        parent_id: None,
                    });
                    turn.apply(EngineEvent::ToolFinished { action_id: id, name: "agent_delegate".into(), result: serde_json::json!({"summary":"Checked the relevant files. The detailed findings stay behind this disclosure.\n\n".repeat(20)}).to_string(), success: true, parent_id: None });
                }
                turn.apply(EngineEvent::Delta { text: "The three checks finished. The summary stays visible while detailed reports can be opened.".into() });
            } else if scene == "output" {
                self.show_action_output(
                    self.session,
                    Uuid::new_v4(),
                    "Complete command output".into(),
                    format!(
                        "{}\n{}\nfinal error: expected status 200, received 503",
                        "Compiling xylo\nRunning focused checks\ntest composer_preserves_draft ... ok\ntest queued_message_edit ... ok",
                        "Checking source and dependencies\n".repeat(1000)
                    ),
                    cx,
                );
            } else {
                turn.apply(EngineEvent::Delta { text: "I found the affected files and started the relevant checks. This partial result remains available if the task stops.".into() });
                let action = Uuid::new_v4();
                let job_id = Uuid::new_v4();
                turn.apply(EngineEvent::ToolStarted {
                    action_id: action,
                    name: "shell_command".into(),
                    arguments: serde_json::json!({"command":"cargo test -p xylo"}),
                    parent_id: None,
                });
                turn.apply(EngineEvent::ShellJobUpdated { job: ShellJob { action_id: Some(action), background: false, id: job_id, turn_id, command: "cargo test -p xylo".into(), cwd: self.workspace.clone().unwrap(), log_path: self.workspace.clone().unwrap().join("fixture.log"), running: scene == "command", exit_code: if scene == "command" { None } else { Some(1) }, output: "Compiling xylo\nRunning 25 relevant checks\ntest composer_preserves_draft ... ok\ntest queued_message_edit ... ok\nerror: expected enabled control".into(), error: None } });
                if scene == "failed" {
                    turn.apply(EngineEvent::ToolFinished { action_id: action, name: "shell_command".into(), result: serde_json::json!({"status":"failed","summary":{"job_id":job_id,"exit_code":1,"stdout":"24 checks passed","stderr":"expected enabled control"}}).to_string(), success: false, parent_id: None });
                    turn.apply(EngineEvent::Error {
                        message:
                            "One check failed. The command output includes the failing assertion."
                                .into(),
                        retryable: false,
                        kind: ErrorKind::Internal,
                    });
                } else if scene == "stopped" {
                    turn.apply(EngineEvent::Cancelled);
                }
            }
            self.invalidate_live_history();
            self.sync_history_list(cx);
            if scene == "answer" {
                if let Some(history) = self.histories.get(&self.session) {
                    history.list.scroll_to(gpui_kit::ListOffset {
                        item_ix: 1,
                        offset_in_item: px(0.),
                    });
                }
            }
            cx.notify();
        }

        pub(crate) fn composer_capture_healthy(&self) {
            assert!(self.error.is_none(), "Actual Host error: {:?}", self.error);
            assert!(self.host.is_some(), "A real temporary Host is required");
            assert!(
                !self.running(),
                "The fixture must never execute a model task"
            );
            assert!(
                !self.speech.busy(),
                "The fixture must not activate a microphone"
            );
        }
    }
}

fn main() {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    render();
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn render() {
    use gpui_kit::{
        component::{Theme, ThemeMode},
        prelude::*,
        test::{TestQueryExt, TestWindowExt},
        *,
    };
    use sha2::{Digest, Sha256};
    use std::{
        path::PathBuf,
        sync::Arc,
        time::{Duration, Instant},
    };
    use xylo_core::{AppSettings, PermissionMode};
    use xylo_runtime::{AppPaths, host::HostClient};
    use xylo_services::Store;

    let temporary = tempfile::tempdir().unwrap();
    let workspace = temporary
        .path()
        .join(if std::env::var_os("XYLO_APPROVED_CHAT").is_some() {
            "xylo"
        } else {
            "Empty project"
        });
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let sample_image = workspace.join("screen.png");
    const PNG: &[u8] = b"\x89\x50\x4e\x47\x0d\x0a\x1a\x0a\x00\x00\x00\x0d\x49\x48\x44\x52\x00\x00\x00\x02\x00\x00\x00\x02\x08\x06\x00\x00\x00\x72\xb6\x0d\x24\x00\x00\x00\x11\x49\x44\x41\x54\x78\x9c\x63\x50\x70\x48\xf8\x0f\xc2\x0c\x30\x06\x00\x39\x74\x06\xfd\x20\x19\xfd\xfb\x00\x00\x00\x00\x49\x45\x4e\x44\xae\x42\x60\x82";
    std::fs::write(&sample_image, PNG).unwrap();
    let paths = AppPaths::at(temporary.path().join("profile")).unwrap();
    let store = Store::open(&paths.database).unwrap();
    let mut settings = AppSettings {
        last_workspace: Some(workspace.clone()),
        permissions: PermissionMode::AskForApproval,
        theme: "light".into(),
        ..Default::default()
    };
    // This is a persisted display configuration, not an available/live model.
    settings.provider = xylo_core::ProviderConfig {
        model: if std::env::var_os("XYLO_APPROVED_CHAT").is_some() {
            "GPT-6 Astra".into()
        } else {
            "fixture-local-coder-model".into()
        },
        ..Default::default()
    };
    settings.provider.reasoning_options = Some(vec![
        "low".into(),
        "medium".into(),
        "high".into(),
        "xhigh".into(),
    ]);
    settings.provider.reasoning_effort = Some("high".into());
    settings.remember_model(settings.provider.clone());
    let mut capture_drafts = Vec::new();
    for text in [
        "Explain this project using read-only inspection. Summarize its architecture and key entry points.",
        "Refine the composer spacing and keyboard shortcuts.",
    ] {
        let key = xylo_runtime::drafts::DraftKey {
            session_id: uuid::Uuid::new_v4(),
            workspace: Some(workspace.clone()),
        };
        capture_drafts.push((key.clone(), text));
        store
            .save_draft(&xylo_runtime::drafts::DraftWrite {
                key,
                expected_revision: 0,
                mutation_id: uuid::Uuid::new_v4(),
                text: text.into(),
                attachments: vec![],
            })
            .unwrap();
    }
    store.save_settings(&settings).unwrap();
    let host = HostClient::spawn_at(paths).unwrap();
    let client = host.0.clone();
    let mut cx = HeadlessAppContext::with_platform(
        platform::current_platform(true).text_system(),
        Arc::new(crate::assets::AppAssets),
        platform::current_headless_renderer,
    );
    cx.allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Light, None, cx);
    });
    let (handle, view) = cx
        .update(|cx| {
            gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        Default::default(),
                        size(px(1380.), px(880.)),
                    ))),
                    focus: false,
                    show: false,
                    ..Default::default()
                },
                cx,
                |window, cx| cx.new(|cx| xylo::Xylo::new(Ok(host), window, cx)),
            )
        })
        .unwrap();

    let wait = |cx: &mut HeadlessAppContext, theme: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            cx.advance_clock(Duration::from_millis(25));
            cx.run_until_parked();
            let ready = cx.update(|cx| {
                let view = view.read(cx);
                view.composer_capture_healthy();
                view.composer_capture_ready() && view.composer_capture_theme() == theme
            });
            if ready {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "Temporary composer Host did not become ready"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait(&mut cx, "light");
    cx.update_window(handle, |_, window, cx| {
        view.update(cx, |view, cx| view.composer_capture_prompt(window, cx));
    })
    .unwrap();
    cx.run_until_parked();

    let output = capture_dir::fresh("composer-rendering");
    if std::env::var_os("XYLO_APPROVED_CHAT").is_some() {
        cx.update(|cx| cx.set_reduce_motion(true));
        let mut manifest = Vec::new();
        for theme in ["dark", "light"] {
            cx.update_window(handle, |_, _, cx| {
                view.update(cx, |view, cx| view.reset_queued_capture(cx))
            })
            .unwrap();
            settings.theme = theme.into();
            client.save_settings(settings.clone()).unwrap();
            wait(&mut cx, theme);
            for (width, height) in [(420, 740), (1000, 680), (1380, 880)] {
                for scene in [
                    "empty",
                    "greeting",
                    "conversation",
                    "markdown",
                    "tool-alignment",
                    "edit-preview",
                    "helpers",
                    "command",
                    "queue",
                    "question",
                    "plan",
                    "failed",
                    "stopped",
                    "output",
                    "model-menu",
                    "model-empty",
                    "model-search",
                    "activity",
                    "permission-menu",
                    "add-menu",
                    "chat-menu",
                    "details",
                    "subagents",
                    "file-changes",
                    "file-diff",
                    "usage",
                    "context",
                    "drafts",
                    "queue-preview",
                    "attachments",
                ] {
                    if std::env::var("XYLO_CAPTURE_SCENES")
                        .is_ok_and(|only| !only.split(',').any(|candidate| candidate == scene))
                    {
                        continue;
                    }
                    if scene == "drafts" {
                        for (key, text) in &capture_drafts {
                            store
                                .save_draft(&xylo_runtime::drafts::DraftWrite {
                                    key: key.clone(),
                                    expected_revision: store.draft(key).unwrap().revision,
                                    mutation_id: uuid::Uuid::new_v4(),
                                    text: (*text).into(),
                                    attachments: vec![],
                                })
                                .unwrap();
                        }
                    }
                    cx.update_window(handle, |_, window, cx| {
                        gpui_kit::component::WindowExt::close_all_dialogs(window, cx);
                        window.resize(size(px(width as f32), px(height as f32)));
                        window.bounds_changed(cx);
                        let base = match scene {
                            "model-menu" | "model-empty" | "model-search" | "permission-menu"
                            | "add-menu" | "chat-menu" | "details" | "usage" | "context"
                            | "drafts" | "attachments" => "greeting",
                            "subagents" | "file-changes" | "file-diff" => "conversation",
                            "queue-preview" => "queue",
                            _ => scene,
                        };
                        view.update(cx, |view, cx| {
                            view.capture_model_state(
                                if scene == "model-empty" {
                                    Vec::new()
                                } else {
                                    settings.providers.clone()
                                },
                                "",
                                window,
                                cx,
                            );
                            view.approved_chat_capture(base, window, cx);
                            if matches!(scene, "subagents" | "file-changes" | "file-diff") {
                                view.capture_task_panel(scene, cx);
                            }
                            if scene == "context" {
                                view.capture_context(cx);
                            }
                            if scene == "attachments" {
                                view.capture_attachment(sample_image.clone(), cx);
                            }
                        });
                        window.render_frame(cx);
                        let control = match scene {
                            "model-menu" | "model-empty" | "model-search" => Some("composer-model"),
                            "permission-menu" => Some("composer-permissions"),
                            "add-menu" => Some("composer-context-actions"),
                            "chat-menu" => Some("conversation-actions"),
                            "details" => Some("toggle-work-panel"),
                            "usage" => Some("usage-totals-button"),
                            "context" => Some("composer-context"),
                            "drafts" => Some("chat-saved-drafts"),
                            _ => None,
                        };
                        if let Some(control) = control {
                            window.click(control, cx);
                        }
                        if scene == "model-search" {
                            view.update(cx, |view, cx| {
                                view.model_picker_capture_query("no-match", window, cx)
                            });
                        }
                        if scene == "queue-preview" {
                            window.click(("preview-queued", 0usize), cx);
                        }
                        window.render_frame(cx);
                    })
                    .unwrap();
                    cx.run_until_parked();
                    cx.advance_clock(Duration::from_millis(150));
                    cx.run_until_parked();
                    if scene == "attachments" {
                        let deadline = Instant::now() + Duration::from_secs(5);
                        while !cx.update(|cx| view.read(cx).capture_attachment_ready()) {
                            assert!(Instant::now() < deadline, "Image import did not finish");
                            cx.advance_clock(Duration::from_millis(25));
                            cx.run_until_parked();
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                            .unwrap();
                    }
                    if scene == "usage" {
                        cx.update_window(handle, |_, window, cx| {
                            view.update(cx, |view, cx| {
                                view.capture_usage_summary(cx);
                            });
                            window.render_frame(cx);
                        })
                        .unwrap();
                    }
                    let filename = format!("{scene}-{theme}-{width}x{height}.png");
                    cx.capture_screenshot(handle)
                        .unwrap()
                        .save(output.join(&filename))
                        .unwrap();
                    if scene == "drafts" {
                        for (action, count, state) in [
                            ("delete-saved-draft-0", 1, "after-delete"),
                            ("delete-saved-draft-0", 0, "empty-after-delete"),
                            ("undo-delete-draft", 1, "after-undo"),
                        ] {
                            cx.update_window(handle, |_, window, cx| window.click(action, cx))
                                .unwrap();
                            let deadline = Instant::now() + Duration::from_secs(5);
                            while cx.update(|cx| view.read(cx).capture_draft_count()) != Some(count)
                            {
                                assert!(
                                    Instant::now() < deadline,
                                    "Draft action did not finish: {state}"
                                );
                                cx.advance_clock(Duration::from_millis(10));
                                cx.run_until_parked();
                                std::thread::sleep(Duration::from_millis(5));
                            }
                            cx.update_window(handle, |_, window, cx| window.render_frame(cx))
                                .unwrap();
                            cx.capture_screenshot(handle)
                                .unwrap()
                                .save(
                                    output.join(format!(
                                        "drafts-{state}-{theme}-{width}x{height}.png"
                                    )),
                                )
                                .unwrap();
                        }
                    }
                    let geometry=cx.update_window(handle,|_,window,_| {
                        ["composer", "send-prompt", "composer-model", "composer-permissions", "chat-toolbar"].into_iter().filter_map(|id| window.try_find(id).map(|node| {let b=node.bounds();serde_json::json!({"id":id,"bounds":[f32::from(b.left()),f32::from(b.top()),f32::from(b.size.width),f32::from(b.size.height)]})})).collect::<Vec<_>>()
                    }).unwrap();
                    manifest.push(serde_json::json!({"image":filename,"theme":theme,"scene":scene,"viewport":[width,height],"geometry":geometry}));
                    if matches!(
                        scene,
                        "model-menu"
                            | "model-empty"
                            | "model-search"
                            | "permission-menu"
                            | "add-menu"
                            | "chat-menu"
                    ) {
                        cx.update_window(handle, |_, window, cx| window.press("escape", cx))
                            .unwrap();
                    }
                }
            }
        }
        std::fs::write(
            output.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        println!(
            "Approved chat capture: {} scenes at {}",
            manifest.len(),
            output.display()
        );
        return;
    }
    let mut cases = Vec::new();
    for theme in ["light", "dark"] {
        settings.theme = theme.into();
        client.save_settings(settings.clone()).unwrap();
        wait(&mut cx, theme);
        for width in [1380_u32, 1000] {
            cx.update_window(handle, |_, window, cx| {
                window.resize(size(px(width as f32), px(880.)));
                // The controlled test platform updates native bounds directly;
                // deliver the same public callback used by real OS resizing.
                window.bounds_changed(cx);
            })
            .unwrap();
            cx.run_until_parked();
            let geometry = cx
                .update_window(handle, |_, window, cx| {
                    view.read(cx).composer_capture_healthy();
                    window.render_frame(cx);
                    assert_eq!(window.viewport_size(), size(px(width as f32), px(880.)));
                    // The context ring stays hidden until a chat uses 5% of the window.
                    let ids = [
                        "composer-context-actions",
                        "composer-permissions",
                        "composer-model",
                        "record-voice-composer",
                        "send-prompt",
                    ];
                    assert_eq!(window.find("composer-context-actions").label(), Some("Add files or audio"));
                    assert_eq!(window.find("composer-permissions").label(), Some("Ask for approval"));
                    assert!(window.try_find("composer-context").is_none(), "A fresh chat has no context ring");
                    let mut controls = Vec::new();
                    let mut previous_right = None;
                    let mut center_y = None::<Pixels>;
                    let mut toolbar_bottom = px(0.);
                    for id in ids {
                        let node = window.find(id);
                        assert!(node.visible(), "{theme}/{width}: {id} is clipped or hidden");
                        let bounds = node.bounds();
                        assert!(bounds.size.width > px(0.) && bounds.size.height > px(0.));
                        assert!(
                            bounds.left() >= px(0.) && bounds.right() <= px(width as f32)
                                && bounds.top() >= px(0.) && bounds.bottom() <= px(880.),
                            "{theme}/{width}: {id} leaves the actual viewport"
                        );
                        let center = bounds.top() + bounds.size.height / 2.;
                        if let Some(expected) = center_y {
                            assert!(
                                (center - expected).abs() <= px(1.),
                                "{theme}/{width}: {id} wraps outside the toolbar row"
                            );
                        } else {
                            center_y = Some(center);
                        }
                        if let Some(right) = previous_right {
                            assert!(bounds.left() >= right, "{id} overlaps a previous control");
                        }
                        previous_right = Some(bounds.right());
                        toolbar_bottom = toolbar_bottom.max(bounds.bottom());
                        controls.push(serde_json::json!({
                            "id":id,"label":node.label(),
                            "bounds":[f32::from(bounds.left()),f32::from(bounds.top()),
                                f32::from(bounds.size.width),f32::from(bounds.size.height)],
                        }));
                    }
                    assert!(window.try_find("composer-project").is_none(), "Project management belongs in the sidebar");
                    assert!(window.try_find("permission-mode").is_none(), "The toolbar must contain one permission selector");
                    assert!(window.try_find(("assistant", 0usize)).is_none(), "The empty chat must not contain showcase cards");
                    let message = window.find_by_label("Message");
                    assert!(message.visible(), "The real composer input must remain visible");
                    let bounds = message.bounds();
                    let envelope_height = toolbar_bottom - bounds.top();
                    assert!(bounds.bottom() <= toolbar_bottom);
                    assert!(
                        envelope_height > px(32.) && envelope_height <= px(160.),
                        "{theme}/{width}: one-line input and toolbar are not compact: {envelope_height:?}"
                    );
                    serde_json::json!({
                        "theme":theme,"logical_viewport":[width,880],"controls":controls,
                        "input_bounds":[f32::from(bounds.left()),f32::from(bounds.top()),
                            f32::from(bounds.size.width),f32::from(bounds.size.height)],
                        "input_to_toolbar_height":f32::from(envelope_height),
                        "same_row_within_viewport":true,
                    })
                })
                .unwrap();
            let screenshot = cx
                .capture_screenshot(handle)
                .expect("Native renderer is required");
            let filename = format!("composer-{theme}-{width}.png");
            let destination = output.join(&filename);
            assert!(
                !destination.exists(),
                "Capture evidence must not be overwritten"
            );
            screenshot.save(&destination).unwrap();
            cx.update_window(handle, |_, window, cx| {
                window.click("composer-model", cx);
                let panel = window.find("composer-model-panel").bounds();
                let effort = window.find("composer-effort").bounds();
                assert!(
                    panel.size.width <= px(340.) && effort.bottom() <= panel.bottom(),
                    "Model and effort controls must stay compact inside the picker"
                );
            })
            .unwrap();
            let popup_image = format!("composer-{theme}-{width}-model-popover.png");
            cx.capture_screenshot(handle)
                .unwrap()
                .save(output.join(&popup_image))
                .unwrap();
            cx.update_window(handle, |_, window, cx| window.click("composer-model", cx))
                .unwrap();
            // A long, virtualized conversation scrolls above the composer;
            // the existing-chat composer must stay at the viewport bottom.
            let anchor = cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| view.composer_capture_history(true, cx));
                window.render_frame(cx);
                let send = window.find("send-prompt").bounds();
                let input = window.find_by_label("Message").bounds();
                assert!(send.bottom() <= px(880.) && send.bottom() >= px(820.), "Existing-chat composer must stay at the bottom: {send:?}");
                assert!(input.top() > px(680.), "History must not displace the composer: {input:?}");
                assert!(window.find_all_by_label("Copy").len() < 50, "Long history must remain virtualized");
                let anchor = serde_json::json!({"send_bottom":f32::from(send.bottom()),"input_top":f32::from(input.top()),"history_messages":300});
                view.update(cx, |view, cx| view.composer_capture_history(false, cx));
                window.render_frame(cx);
                anchor
            }).unwrap();
            cases.push(serde_json::json!({
                "geometry":geometry,"existing_chat_anchor":anchor,"image":filename,"model_popover_image":popup_image,
                "pixels":[screenshot.width(),screenshot.height()],
                "sha256":hex::encode(Sha256::digest(std::fs::read(destination).unwrap())),
            }));
        }
    }
    let mut queue_cases = Vec::new();
    for theme in ["light", "dark"] {
        settings.theme = theme.into();
        client.save_settings(settings.clone()).unwrap();
        wait(&mut cx, theme);
        for (width, height) in [(1000_u32, 680_u32), (1380, 880), (1720, 1040)] {
            for active in [true, false] {
                cx.update_window(handle, |_, window, cx| {
                    window.resize(size(px(width as f32), px(height as f32)));
                    window.bounds_changed(cx);
                    view.update(cx, |view, cx| view.queued_capture(active, cx));
                    window.render_frame(cx);
                    let pending = window.find("composer-pending").bounds();
                    let composer = window.find("image-drop-target").bounds();
                    let status = window.find("composer-status-line").bounds();
                    assert!(pending.bottom() <= composer.top());
                    assert!(
                        composer.bottom() <= status.top(),
                        "The status row belongs below the composer"
                    );
                    assert!(
                        status.bottom() >= px(height as f32 - 35.)
                            && status.bottom() <= px(height as f32)
                    );
                    assert_eq!(window.try_find("steer-prompt").is_some(), active);
                    assert_eq!(window.try_find(("send-queued", 0usize)).is_some(), !active);
                    assert!(window.find(("remove-queued", 0usize)).visible());
                })
                .unwrap();
                let filename = format!(
                    "queue-{theme}-{width}-{}.png",
                    if active { "active" } else { "paused" }
                );
                cx.capture_screenshot(handle)
                    .unwrap()
                    .save(output.join(&filename))
                    .unwrap();
                queue_cases.push(serde_json::json!({"theme":theme,"viewport":[width,height],"active":active,"image":filename}));
                cx.update_window(handle, |_, _, cx| {
                    view.update(cx, |view, cx| view.reset_queued_capture(cx))
                })
                .unwrap();
            }
        }
    }
    let mut local_decision_cases = Vec::new();
    for theme in ["light", "dark"] {
        settings.theme = theme.into();
        client.save_settings(settings.clone()).unwrap();
        wait(&mut cx, theme);
        for (width, height) in [(1000_u32, 680_u32), (1380, 880), (1720, 1040)] {
            for plan in [false, true] {
                cx.update_window(handle, |_, window, cx| {
                    window.resize(size(px(width as f32), px(height as f32)));
                    window.bounds_changed(cx);
                    view.update(cx, |view, cx| view.local_decision_capture(plan, window, cx));
                    window.render_frame(cx);
                    let local =
                        window.find_by_label(if plan { "Plan feedback" } else { "Answer 1" });
                    assert!(local.visible(), "The action owns a visible local field");
                    assert!(
                        window
                            .find(if plan {
                                ("start-plan", 0usize)
                            } else {
                                ("answer-questions", 0usize)
                            })
                            .visible()
                    );
                    assert_eq!(
                        view.read(cx).captured_draft(cx).as_str(),
                        "Keep this separate main draft for later."
                    );
                })
                .unwrap();
                let filename = format!(
                    "{}-{theme}-{width}x{height}.png",
                    if plan {
                        "plan-feedback"
                    } else {
                        "question-answer"
                    }
                );
                cx.capture_screenshot(handle)
                    .unwrap()
                    .save(output.join(&filename))
                    .unwrap();
                local_decision_cases.push(serde_json::json!({"theme":theme,"viewport":[width,height],"kind":if plan {"plan"} else {"question"},"image":filename,"local_field_visible":true,"main_draft_unchanged":true,"answer_submitted":false}));
                cx.update_window(handle, |_, _, cx| {
                    view.update(cx, |view, cx| view.reset_queued_capture(cx))
                })
                .unwrap();
            }
        }
    }
    let mut provider_cases = Vec::new();
    for theme in ["light", "dark"] {
        settings.theme = theme.into();
        client.save_settings(settings.clone()).unwrap();
        wait(&mut cx, theme);
        for custom in [true, false] {
            cx.update_window(handle, |_, window, cx| {
                window.resize(size(px(1380.), px(1100.)));
                window.bounds_changed(cx);
                view.update(cx, |view, cx| view.provider_capture(custom, window, cx));
                window.render_frame(cx);
                assert!(window.try_find("provider-save").is_some());
                assert_eq!(
                    window
                        .try_find_by_label("Extra headers (JSON object)")
                        .is_some(),
                    custom
                );
                assert_eq!(
                    window
                        .try_find_by_label("Model discovery URL (optional)")
                        .is_some(),
                    custom
                );
                assert_eq!(window.try_find_by_label("Base URL").is_some(), custom);
                assert!(window.try_find("preset-azure").is_none());
                assert!(window.try_find("preset-bedrock").is_none());
                assert!(window.try_find("provider-discover").is_none());
                assert!(
                    window.try_find("preset-local").is_none(),
                    "Selected provider forms must not keep a provider showcase"
                );
            })
            .unwrap();
            cx.run_until_parked();
            let screenshot = cx.capture_screenshot(handle).unwrap();
            let filename = format!(
                "provider-{theme}-{}.png",
                if custom { "custom" } else { "openai" }
            );
            screenshot.save(output.join(&filename)).unwrap();
            provider_cases.push(serde_json::json!({"theme":theme,"provider":if custom {"custom"} else {"openai"},"image":filename,"viewport":[1380,1100],"no_request_submitted":true}));
        }
    }
    let mut account_cases = Vec::new();
    for theme in ["light", "dark"] {
        settings.theme = theme.into();
        client.save_settings(settings.clone()).unwrap();
        wait(&mut cx, theme);
        for ready in [false, true] {
            cx.update_window(handle, |_, window, cx| {
                view.update(cx, |view, cx| view.account_capture(ready, window, cx));
                window.render_frame(cx);
                assert!(window.try_find("chatgpt-refresh-models").is_some());
                assert_eq!(
                    window.try_find("provider-cancel-discovery").is_some(),
                    !ready
                );
                assert_eq!(window.try_find(("model", 0usize)).is_some(), ready);
            })
            .unwrap();
            let screenshot = cx.capture_screenshot(handle).unwrap();
            let filename = format!(
                "account-{theme}-{}.png",
                if ready { "models" } else { "loading" }
            );
            screenshot.save(output.join(&filename)).unwrap();
            account_cases.push(serde_json::json!({"theme":theme,"state":if ready {"models"} else {"loading"},"image":filename,"controlled_account_status_and_catalog":true,"live_account_qualified":false,"no_auth_or_catalog_request_submitted":true}));
        }
    }
    let mut rich_chat_cases = Vec::new();
    cx.update(|cx| cx.set_reduce_motion(true));
    for theme in ["light", "dark"] {
        settings.theme = theme.into();
        client.save_settings(settings.clone()).unwrap();
        wait(&mut cx, theme);
        for (width, height) in [(1000_u32, 680_u32), (1380, 880), (1720, 1040)] {
            for scene in [
                "answer", "command", "failed", "stopped", "helpers", "output",
            ] {
                cx.update_window(handle, |_, window, cx| {
                    window.resize(size(px(width as f32), px(height as f32)));
                    window.bounds_changed(cx);
                    view.update(cx, |view, cx| view.rich_chat_capture(scene, window, cx));
                    window.render_frame(cx);
                })
                .unwrap();
                cx.run_until_parked();
                let filename = format!("chat-{scene}-{theme}-{width}x{height}.png");
                cx.capture_screenshot(handle)
                    .unwrap()
                    .save(output.join(&filename))
                    .unwrap();
                rich_chat_cases.push(serde_json::json!({"theme":theme,"viewport":[width,height],"scene":scene,"image":filename,"reduce_motion":true}));
                cx.update_window(handle, |_, _, cx| {
                    view.update(cx, |view, cx| {
                        view.capture_close_output(cx);
                        view.reset_queued_capture(cx);
                    });
                })
                .unwrap();
            }
        }
    }
    let mut popover_cases = Vec::new();
    // Capture settled geometry; these cases validate layout, not motion.
    cx.update(|cx| cx.set_reduce_motion(true));
    for theme in ["light", "dark"] {
        settings.theme = theme.into();
        client.save_settings(settings.clone()).unwrap();
        wait(&mut cx, theme);
        for height in [880_u32, 480] {
            cx.update_window(handle, |_, window, cx| {
                window.resize(size(px(1000.), px(height as f32)));
                window.bounds_changed(cx);
                view.update(cx, |view, cx| view.composer_popovers_capture(cx));
                window.render_frame(cx);
                window.click("composer-model", cx);
                window.render_frame(cx);
                let panel = window.find("composer-model-panel").bounds();
                let row = window.find(("composer-model-recent", 0usize)).bounds();
                assert!(panel.top() >= px(0.) && panel.bottom() <= px(height as f32));
                assert!(
                    row.size.height >= px(28.),
                    "The catalog must scroll without compressing rows"
                );
            })
            .unwrap();
            let model_image = format!("model-picker-{theme}-{height}.png");
            cx.capture_screenshot(handle)
                .unwrap()
                .save(output.join(&model_image))
                .unwrap();
            cx.update_window(handle, |_, window, cx| {
                window.press("up", cx);
                window.render_frame(cx);
                let list = window.find("composer-model-list").bounds();
                let last = window.find(("composer-model-choice", 31usize)).bounds();
                assert!(
                    last.top() >= list.top() && last.bottom() <= list.bottom(),
                    "Keyboard choice is clipped: {last:?}, list={list:?}"
                );
            })
            .unwrap();
            let scrolled_image = format!("model-picker-scrolled-{theme}-{height}.png");
            cx.capture_screenshot(handle)
                .unwrap()
                .save(output.join(&scrolled_image))
                .unwrap();
            cx.update_window(handle, |_, window, cx| {
                window.press("escape", cx);
                window.click("usage-totals-button", cx);
                window.render_frame(cx);
                let panel = window.find("composer-context-panel").bounds();
                assert!(panel.top() >= px(0.) && panel.bottom() <= px(height as f32));
            })
            .unwrap();
            let context_image = format!("context-panel-{theme}-{height}.png");
            cx.capture_screenshot(handle)
                .unwrap()
                .save(output.join(&context_image))
                .unwrap();
            cx.update_window(handle, |_, window, cx| {
                window.click("usage-totals-button", cx);
            })
            .unwrap();
            popover_cases.push(serde_json::json!({
                "theme":theme,"viewport":[1000,height],"model_image":model_image,
                "scrolled_model_image":scrolled_image,"context_image":context_image,
                "synthetic_catalog_context_and_quota":true,"no_request_submitted":true
            }));
        }
    }
    let compiled_sources = [
        (
            "accounts.rs",
            include_bytes!("../src/xylo/accounts.rs").as_slice(),
        ),
        ("xylo.rs", include_bytes!("../src/xylo.rs").as_slice()),
        ("chat.rs", include_bytes!("../src/xylo/chat.rs").as_slice()),
        (
            "question_card.rs",
            include_bytes!("../src/xylo/chat/question_card.rs").as_slice(),
        ),
        (
            "plan_card.rs",
            include_bytes!("../src/xylo/chat/plan_card.rs").as_slice(),
        ),
        (
            "providers.rs",
            include_bytes!("../src/xylo/providers.rs").as_slice(),
        ),
        (
            "settings.rs",
            include_bytes!("../src/xylo/settings.rs").as_slice(),
        ),
        (
            "attachments.rs",
            include_bytes!("../src/xylo/attachments.rs").as_slice(),
        ),
        (
            "speech.rs",
            include_bytes!("../src/xylo/speech.rs").as_slice(),
        ),
        ("ui.rs", include_bytes!("../src/ui.rs").as_slice()),
        (
            "model_picker.rs",
            include_bytes!("../src/xylo/model_picker.rs").as_slice(),
        ),
        (
            "context_panel.rs",
            include_bytes!("../src/xylo/context_panel.rs").as_slice(),
        ),
        (
            "composer_rendering.rs",
            include_bytes!("composer_rendering.rs").as_slice(),
        ),
        ("native-Cargo.lock", include_bytes!("Cargo.lock").as_slice()),
    ]
    .into_iter()
    .map(|(path, bytes)| (path, hex::encode(Sha256::digest(bytes))))
    .collect::<std::collections::BTreeMap<_, _>>();
    let metadata = serde_json::json!({
        "schema":1,
        "scope":"Controlled offscreen production Xylo capture with real temporary Host/Store, native platform fonts and GPU. Synthetic persisted model label, no inference/accounts/microphone or physical desktop acceptance.",
        "rich_chat_cases":rich_chat_cases,"queue_cases":queue_cases,"local_decision_cases":local_decision_cases,"cases":cases,"provider_cases":provider_cases,"account_cases":account_cases,"popover_cases":popover_cases,"compiled_sources":compiled_sources,
        "source_errors_suppressed":false,
        "controls_clicked":true,"execution_actions_submitted":false,
    });
    let metadata_path = output.join("composer-rendering.json");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(metadata_path)
        .unwrap();
    std::io::Write::write_all(
        &mut file,
        serde_json::to_string_pretty(&metadata).unwrap().as_bytes(),
    )
    .unwrap();
    client.shutdown().unwrap();
    cx.run_until_parked();
    println!(
        "Controlled native composer rendering: {}\n{metadata}",
        output.display()
    );
}
