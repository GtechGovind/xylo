//! Inspectable procedural learning. Reviews and rollbacks are explicit product
//! actions; scoped, request-correlated responses cannot leak across projects.
use super::*;
use serde_json::{Value, json};
use xylo_runtime::learning::{LearnedSkill, LearningStatus};

/// The engine's built-in skills, read from the same `SKILL.md` files the
/// engine compiles in. The desktop cannot reach the engine crate, so only the
/// list of folders is repeated here; a test keeps it equal to
/// `crates/engine/skills`.
const BUILT_IN_SKILL_FILES: [&str; 6] = [
    include_str!("../../../engine/skills/code-review/SKILL.md"),
    include_str!("../../../engine/skills/create-skill/SKILL.md"),
    include_str!("../../../engine/skills/debug-failure/SKILL.md"),
    include_str!("../../../engine/skills/git-commit/SKILL.md"),
    include_str!("../../../engine/skills/refactor/SKILL.md"),
    include_str!("../../../engine/skills/write-tests/SKILL.md"),
];

/// A skill that ships with Xylo: always available, nothing to manage.
#[derive(Debug, PartialEq, Eq)]
struct BuiltInSkill {
    name: String,
    description: String,
}

/// The `name` and `description` of a `SKILL.md` frontmatter block; `None`
/// when either is missing.
fn built_in_skill(source: &str) -> Option<BuiltInSkill> {
    let mut lines = source.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }
    let (mut name, mut description) = (None, None);
    for line in lines.take_while(|line| line.trim() != "---") {
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim().trim_matches('"').to_owned();
            match key.trim() {
                "name" => name = Some(value),
                "description" => description = Some(value),
                _ => {}
            }
        }
    }
    Some(BuiltInSkill {
        name: name?,
        description: description?,
    })
}
fn built_in_skills() -> Vec<BuiltInSkill> {
    BUILT_IN_SKILL_FILES
        .iter()
        .filter_map(|source| built_in_skill(source))
        .collect()
}

pub(super) struct LearningUi {
    status: LearningStatus,
    skills: Vec<LearnedSkill>,
    versions: Vec<LearnedSkill>,
    selected: Option<(String, i64)>,
    pending: BTreeMap<Uuid, String>,
    review: Entity<InputState>,
    notice: String,
    scroll: UniformListScrollHandle,
    /// The latest request and when it began, for busy buttons and the slow
    /// caption.
    request: String,
    since: Option<std::time::Instant>,
}
impl LearningUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Xylo>) -> Self {
        Self {
            status: Default::default(),
            skills: Vec::new(),
            versions: Vec::new(),
            selected: None,
            pending: BTreeMap::new(),
            review: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("What did you verify before promoting this procedure?")
            }),
            notice: String::new(),
            scroll: UniformListScrollHandle::new(),
            request: String::new(),
            since: None,
        }
    }
    pub(super) fn clear(&mut self) {
        self.status = Default::default();
        self.skills.clear();
        self.versions.clear();
        self.selected = None;
        self.notice.clear();
    }
}
impl Xylo {
    pub(super) fn select_learning(&mut self, name: String, version: i64, cx: &mut Context<Self>) {
        let scope = self.scope();
        if self
            .learning
            .pending
            .values()
            .any(|pending| pending == &scope)
        {
            return;
        }
        self.learning.selected = Some((name.clone(), version));
        self.learning.versions.clear();
        self.learning_request("learning_versions", json!({"name":name}), cx);
    }
    #[cfg(test)]
    #[allow(dead_code)] // Exercised by the separate native validation test target.
    pub(super) fn validation_learning_pending_selection(&mut self, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        self.learning.pending.clear();
        self.learning.selected = Some((name.into(), 1));
        self.learning.pending.insert(id, self.scope());
        id
    }
    #[cfg(test)]
    #[allow(dead_code)] // Exercised by the separate native validation test target.
    pub(super) fn validation_learning_selection(&self) -> Option<String> {
        self.learning
            .selected
            .as_ref()
            .map(|(name, _)| name.clone())
    }

    fn learning_request(&mut self, name: &str, mut args: Value, cx: &mut Context<Self>) {
        let scope = self.scope();
        if self
            .learning
            .pending
            .values()
            .any(|pending| pending == &scope)
        {
            return;
        }
        args["scope"] = json!(scope);
        let id = Uuid::new_v4();
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Application service unavailable"))
            .and_then(|host| host.capability(id, name.into(), args));
        if result.is_ok() {
            self.learning.since = Some(self.arm_slow_hint(cx));
            self.learning.request = name.to_owned();
            self.learning.pending.insert(id, scope);
            self.learning.notice = "Loading procedures…".into();
        }
        self.result(result, cx);
    }
    /// Loads this project's procedures, and installed plugins for the
    /// Installed skills section when they are not known yet.
    pub(super) fn refresh_learning(&mut self, cx: &mut Context<Self>) {
        self.learning_request("learning_snapshot", json!({}), cx);
        self.ensure_extensions_loaded(cx);
    }
    #[cfg(test)]
    #[allow(dead_code)] // Exercised by the separate native validation test target.
    pub(crate) fn validation_learning_idle(&self) -> bool {
        !self.learning_loading()
    }
    pub(super) fn learning_error(
        &mut self,
        id: Uuid,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(scope) = self.learning.pending.remove(&id) else {
            return false;
        };
        if scope == self.scope() {
            self.learning.notice = message.into();
            cx.notify();
        }
        true
    }
    pub(super) fn learning_result(
        &mut self,
        id: Uuid,
        name: &str,
        value: &Value,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(scope) = self.learning.pending.remove(&id) else {
            return false;
        };
        if scope != self.scope() {
            return true;
        }
        let result = (|| -> anyhow::Result<()> {
            match name {
                "learning_snapshot" => {
                    self.learning.status = serde_json::from_value(value["status"].clone())?;
                    self.learning.skills = serde_json::from_value(value["skills"].clone())?;
                    self.learning.notice =
                        "Procedures are evaluated in the background. Tool permissions still apply."
                            .into();
                }
                "learning_versions" => {
                    if self
                        .learning
                        .selected
                        .as_ref()
                        .is_some_and(|(name, _)| value["name"].as_str() == Some(name))
                    {
                        self.learning.versions = serde_json::from_value(value["versions"].clone())?;
                    }
                    self.learning.notice = "Version history loaded.".into();
                }
                "learning_review" | "learning_rollback" | "learning_correction" => {
                    self.learning.versions.clear();
                    self.refresh_learning(cx);
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(%error,"Invalid learning response");
            self.learning.notice = "Procedures could not be loaded. Try again.".into();
        }
        cx.notify();
        true
    }
    /// Skills: portable skills from installed plugins, then the procedures
    /// Xylo learned in this project. Both load when the page opens.
    pub(super) fn skills_settings_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut page = settings_ui::settings_page("skills-settings", "Skills")
            .description("Instructions Xylo loads only when a task needs them.");
        // One "Browse plugins" at a time: the empty state carries it as the
        // primary action, otherwise it sits in the header.
        if !self.no_skills_installed() {
            page = page.action(
                settings_ui::secondary_button("skills-browse-plugins", "Browse plugins").on_click(
                    cx.listener(|view, _, window, cx| view.open_plugins_discover(window, cx)),
                ),
            );
        }
        page = page
            .child(settings_ui::section(
                "Built-in",
                Some("Included with Xylo and always available.".into()),
                built_in_skills_list(),
            ))
            .child(settings_ui::section(
                "From plugins",
                Some("Skills in disabled plugins are not loaded.".into()),
                self.installed_skills_list(cx),
            ))
            .child(settings_ui::section(
                "Learned procedures",
                Some(
                    "Learned from completed work in this project. Tool permissions still apply."
                        .into(),
                ),
                self.learned_procedures_list(cx),
            ));
        if let Some(detail) = self.learned_procedure_detail(cx) {
            page = page.child(detail);
        }
        // Keep feedback relevant to the person's current action; loading and
        // background-evaluation messages are shown by the lists themselves.
        if !self.learning.notice.is_empty()
            && self.learning.notice != "Loading procedures…"
            && !self.learning.notice.starts_with("Procedures are evaluated")
            && self.learning.notice != "Version history loaded."
        {
            page = page.child(notice(BadgeTone::Neutral, self.learning.notice.clone()));
        }
        page.into_any_element()
    }
    fn learning_loading(&self) -> bool {
        let scope = self.scope();
        self.learning
            .pending
            .values()
            .any(|pending| pending == &scope)
    }
    /// Installed skills are loaded and there are none, so the list shows its
    /// empty state.
    fn no_skills_installed(&self) -> bool {
        self.installed_skills().is_empty()
            && !(self.extensions.active() && !self.extensions_loaded())
    }
    fn installed_skills_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let skills = self.installed_skills();
        if skills.is_empty() {
            if !self.no_skills_installed() {
                return settings_ui::loading_list(2, self.extensions_pending_since())
                    .into_any_element();
            }
            return settings_ui::empty_state(
                AppIcon::Skills,
                "No plugin skills yet",
                "Plugins can add more skills alongside the built-in ones.",
                Some(
                    settings_ui::primary_button("skills-browse-plugins", "Browse plugins")
                        .on_click(
                            cx.listener(|view, _, window, cx| {
                                view.open_plugins_discover(window, cx)
                            }),
                        )
                        .into_any_element(),
                ),
            )
            .into_any_element();
        }
        let height = skills.len().min(9) as f32 * settings_ui::LIST_ITEM_HEIGHT;
        let skills = std::rc::Rc::new(skills);
        settings_ui::list_frame()
            .child(
                uniform_list("installed-skills", skills.len(), move |range, _, _| {
                    range
                        .filter_map(|index| {
                            let skill = skills.get(index)?;
                            let name = super::extensions::display_name(&skill.name);
                            let mut item = settings_ui::list_item(
                                ("installed-skill", index),
                                Some(
                                    settings_ui::monogram(
                                        &name,
                                        &format!("{}/{}", skill.package_id, skill.name),
                                    )
                                    .into_any_element(),
                                ),
                                name,
                                (!skill.description.is_empty())
                                    .then(|| skill.description.clone().into()),
                            )
                            .hover_detail(skill.name.clone())
                            .tags([skill.package.clone().into()])
                            .divider(index > 0);
                            if !skill.enabled {
                                item = item.trailing(settings_ui::status_pill(
                                    settings_ui::Status::Disabled,
                                ));
                            }
                            Some(item.into_any_element())
                        })
                        .collect()
                })
                .h(px(height)),
            )
            .into_any_element()
    }
    fn learned_procedures_list(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.learning.skills.is_empty() {
            if self.learning_loading() {
                return settings_ui::loading_list(2, self.learning.since).into_any_element();
            }
            return settings_ui::empty_state(
                AppIcon::Skills,
                "No learned procedures yet",
                "Xylo saves a procedure when a verified sequence of steps succeeds, and suggests it next time.",
                None,
            )
            .into_any_element();
        }
        let height = self.learning.skills.len().min(9) as f32 * settings_ui::LIST_ITEM_HEIGHT;
        let weak = cx.entity().downgrade();
        settings_ui::list_frame()
            .child(
                uniform_list(
                    "learned-procedures",
                    self.learning.skills.len(),
                    move |range, _, cx| {
                        weak.update(cx, |view, cx| {
                            range
                                .filter_map(|index| view.learned_procedure_row(index, cx))
                                .collect()
                        })
                        .unwrap_or_default()
                    },
                )
                .track_scroll(&self.learning.scroll)
                .h(px(height)),
            )
            .into_any_element()
    }
    fn learned_procedure_row(&self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let skill = self.learning.skills.get(index)?;
        let name = skill.skill.name.clone();
        let version = skill.skill.version;
        let selected = self.learning.selected.as_ref() == Some(&(name.clone(), version));
        Some(
            settings_ui::list_item(
                ("learned-procedure", index),
                Some(settings_ui::monogram(&skill.title, &name).into_any_element()),
                skill.title.clone(),
                Some(format!("Version {version}").into()),
            )
            .hover_detail(name.clone())
            .divider(index > 0)
            .selected(selected)
            .trailing(procedure_pill(&skill.state))
            .on_click(cx.listener(move |view, _, _, cx| {
                view.select_learning(name.clone(), version, cx);
            }))
            .into_any_element(),
        )
    }
    /// The selected procedure: guidance, steps, review and earlier versions.
    fn learned_procedure_detail(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (name, version) = self.learning.selected.as_ref()?;
        let skill = self
            .learning
            .versions
            .iter()
            .chain(&self.learning.skills)
            .find(|skill| &skill.skill.name == name && &skill.skill.version == version)?;
        let loading = self.learning_loading();
        let mut body = div().flex().flex_col().gap(px(12.));
        if let Some(guidance) = &skill.correction_guidance {
            body =
                body.child(TextView::markdown("skill-guidance", guidance.clone()).selectable(true));
        }
        body = body.child(settings_ui::rows(skill.steps.iter().enumerate().map(
            |(index, step)| {
                settings_ui::setting_row(
                    format!("{}. {}", index + 1, step.tool.replace('_', " ")),
                    step.requires_approval.then(|| "Approval required".into()),
                    None,
                )
                .into_any_element()
            },
        )));
        if skill.state != "promoted" {
            let promote_name = name.clone();
            let promote_version = *version;
            body = body
                .child(settings_ui::field(
                    "Review evidence",
                    Input::new(&self.learning.review).aria_label("Procedure review evidence"),
                    Some("Promoting lets Xylo suggest this procedure in future tasks.".into()),
                ))
                .child(settings_ui::form_actions([settings_ui::busy(
                    settings_ui::primary_button("review-learned-procedure", "Review and promote")
                        .disabled(loading),
                    loading && self.learning.request == "learning_review",
                )
                .on_click(cx.listener(move |view, _, _, cx| {
                    let evidence = view.learning.review.read(cx).value().trim().to_string();
                    if evidence.is_empty() {
                        view.learning.notice =
                            "Describe what you verified before promoting this skill.".into();
                        cx.notify();
                        return;
                    }
                    view.learning_request(
                        "learning_review",
                        json!({"name": promote_name, "version": promote_version, "evidence": evidence}),
                        cx,
                    );
                }))
                .into_any_element()]));
        }
        let earlier: Vec<AnyElement> = self
            .learning
            .versions
            .iter()
            .enumerate()
            .filter(|(_, old)| old.skill.version < *version)
            .take(20)
            .map(|(index, old)| {
                let restore_name = old.skill.name.clone();
                let restore_version = old.skill.version;
                settings_ui::setting_row(
                    format!("Version {restore_version}"),
                    None,
                    Some(
                        settings_ui::text_button(("rollback-procedure", index), "Restore")
                            .disabled(loading)
                            .on_click(cx.listener(move |view, _, _, cx| {
                                view.learning_request(
                                    "learning_rollback",
                                    json!({"name": restore_name, "version": restore_version}),
                                    cx,
                                )
                            }))
                            .into_any_element(),
                    ),
                )
                .into_any_element()
            })
            .collect();
        if !earlier.is_empty() {
            body = body.child(settings_ui::section(
                "Earlier versions",
                None,
                settings_ui::rows(earlier),
            ));
        }
        Some(
            settings_ui::section(
                skill.title.clone(),
                Some(format!("Version {version} · {}", procedure_state(&skill.state)).into()),
                body,
            )
            .into_any_element(),
        )
    }
}
/// The built-in skills: read-only rows (the section heading says where they come from).
fn built_in_skills_list() -> Div {
    settings_ui::list_container(
        built_in_skills()
            .into_iter()
            .enumerate()
            .map(|(index, skill)| {
                let name = super::extensions::display_name(&skill.name);
                settings_ui::list_item(
                    ("built-in-skill", index),
                    Some(settings_ui::monogram(&name, &skill.name).into_any_element()),
                    name,
                    Some(skill.description.into()),
                )
                .hover_detail(skill.name)
                .into_any_element()
            }),
    )
}
/// Readable state of a learned procedure.
fn procedure_state(state: &str) -> String {
    match state {
        "candidate" => "Candidate".into(),
        "promoted" => "Promoted".into(),
        "suspended" => "Suspended".into(),
        "superseded" => "Superseded".into(),
        other => {
            let mut chars = other.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect::<String>())
                .unwrap_or_default()
                .replace('_', " ")
        }
    }
}
fn procedure_pill(state: &str) -> Div {
    settings_ui::pill(
        procedure_state(state),
        match state {
            "promoted" => settings_ui::Tone::Success,
            "candidate" => settings_ui::Tone::Accent,
            "suspended" => settings_ui::Tone::Warning,
            _ => settings_ui::Tone::Neutral,
        },
    )
}

#[cfg(test)]
#[path = "learning/tests.rs"]
mod tests;
