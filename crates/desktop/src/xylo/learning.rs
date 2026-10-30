//! Inspectable procedural learning. Reviews and rollbacks are explicit product
//! actions; scoped, request-correlated responses cannot leak across projects.
use super::*;
use serde_json::{Value, json};
use xylo_runtime::learning::{LearnedSkill, LearningStatus};

pub(super) struct LearningUi {
    status: LearningStatus,
    skills: Vec<LearnedSkill>,
    versions: Vec<LearnedSkill>,
    selected: Option<(String, i64)>,
    pending: BTreeMap<Uuid, String>,
    review: Entity<InputState>,
    notice: String,
    scroll: UniformListScrollHandle,
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
    pub(super) fn validation_learning_pending_selection(&mut self, name: &str) -> Uuid {
        let id = Uuid::new_v4();
        self.learning.pending.clear();
        self.learning.selected = Some((name.into(), 1));
        self.learning.pending.insert(id, self.scope());
        id
    }
    #[cfg(test)]
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
            self.learning.pending.insert(id, scope);
            self.learning.notice = "Loading procedures…".into();
        }
        self.result(result, cx);
    }
    pub(super) fn refresh_learning(&mut self, cx: &mut Context<Self>) {
        self.learning_request("learning_snapshot", json!({}), cx);
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
    pub(super) fn skills_settings_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let scope = self.scope();
        let loading = self
            .learning
            .pending
            .values()
            .any(|pending| pending == &scope);
        let mut content = div().w_full().flex().flex_col().gap_4().child(
            div().flex().items_center().justify_end().child(
                button("refresh-learning", "Refresh")
                    .disabled(loading)
                    .on_click(cx.listener(|view, _, _, cx| view.refresh_learning(cx))),
            ),
        );
        if loading && !self.learning.skills.is_empty() {
            content = content.child(subtitle("Updating procedures…"));
        }
        if self.learning.skills.is_empty() {
            content = content.child(subtitle(if loading {
                "Loading skills…"
            } else {
                "No saved skills"
            }));
        } else {
            let weak = cx.entity().downgrade();
            let rows = uniform_list(
                "learned-procedures",
                self.learning.skills.len(),
                move |range, _, cx| {
                    weak.update(cx, |view, cx| {
                        range
                            .filter_map(|index| {
                                let skill = view.learning.skills.get(index)?;
                                let name = skill.skill.name.clone();
                                let version = skill.skill.version;
                                Some(
                                    button(("learned-procedure", index), skill.title.clone())
                                        .disabled(loading)
                                        .h(px(52.))
                                        .w_full()
                                        .justify_start()
                                        .bg(rgb(
                                            if view.learning.selected.as_ref()
                                                == Some(&(name.clone(), version))
                                            {
                                                selected()
                                            } else {
                                                panel()
                                            },
                                        ))
                                        .child(div().flex_1())
                                        .child(subtitle(format!(
                                            "v{version} · {}",
                                            skill.state.replace('_', " ")
                                        )))
                                        .on_click(cx.listener(move |view, _, _, cx| {
                                            view.select_learning(name.clone(), version, cx);
                                        }))
                                        .into_any_element(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default()
                },
            )
            .track_scroll(&self.learning.scroll)
            .h(px((self.learning.skills.len() as f32 * 52.).min(320.)));
            content = content.child(
                div()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(border()))
                    .overflow_hidden()
                    .child(rows),
            );
        }
        if let Some((name, version)) = &self.learning.selected {
            if let Some(skill) = self
                .learning
                .versions
                .iter()
                .chain(&self.learning.skills)
                .find(|skill| &skill.skill.name == name && &skill.skill.version == version)
            {
                let mut detail = div()
                    .rounded_lg()
                    .border_1()
                    .border_color(rgb(border()))
                    .p_4()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(skill.title.clone()),
                    );
                if let Some(guidance) = &skill.correction_guidance {
                    detail = detail.child(
                        TextView::markdown("skill-guidance", guidance.clone()).selectable(true),
                    );
                }
                for (index, step) in skill.steps.iter().enumerate() {
                    detail = detail.child(subtitle(format!(
                        "{}. {}{}",
                        index + 1,
                        step.tool.replace('_', " "),
                        if step.requires_approval {
                            " · approval required"
                        } else {
                            ""
                        }
                    )));
                }
                if skill.state != "promoted" {
                    let promote_name = name.clone();
                    let promote_version = *version;
                    detail=detail.child(Input::new(&self.learning.review).aria_label("Procedure review evidence"))
                        .child(button("review-learned-procedure","Review and promote").disabled(loading).on_click(cx.listener(move|view,_,_,cx|{
                            let evidence=view.learning.review.read(cx).value().trim().to_string();
                            if evidence.is_empty(){view.learning.notice="Describe what you verified before promoting this skill.".into();cx.notify();return;}
                            view.learning_request("learning_review",json!({"name":promote_name,"version":promote_version,"evidence":evidence}),cx);
                        })));
                }
                for (index, old) in self
                    .learning
                    .versions
                    .iter()
                    .enumerate()
                    .filter(|(_, old)| old.skill.version < *version)
                    .take(20)
                {
                    let restore_name = old.skill.name.clone();
                    let restore_version = old.skill.version;
                    detail = detail.child(
                        button(
                            ("rollback-procedure", index),
                            format!("Restore version {restore_version}"),
                        )
                        .disabled(loading)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.learning_request(
                                "learning_rollback",
                                json!({"name":restore_name,"version":restore_version}),
                                cx,
                            )
                        })),
                    );
                }
                content = content.child(detail);
            }
        }
        // Keep feedback relevant to the user's current action; omit the legacy
        // always-on tutorial, counters and background-learning explanation.
        if !self.learning.notice.is_empty()
            && self.learning.notice != "Loading procedures…"
            && !self.learning.notice.starts_with("Procedures are evaluated")
            && self.learning.notice != "Version history loaded."
        {
            content = content.child(subtitle(self.learning.notice.clone()));
        }
        content.into_any_element()
    }
}
