//! Inspectable procedural learning. Reviews and rollbacks are explicit product
//! actions; scoped, request-correlated responses cannot leak across projects.
use super::*;
use serde_json::{Value, json};
use workbench_runtime::learning::{LearnedSkill, LearningStatus};

pub(super) struct LearningUi {
    status: LearningStatus,
    skills: Vec<LearnedSkill>,
    versions: Vec<LearnedSkill>,
    selected: Option<(String, i64)>,
    pending: BTreeMap<Uuid, String>,
    review: Entity<InputState>,
    correction: Entity<TextareaState>,
    correction_source: Option<Uuid>,
    notice: String,
    scroll: UniformListScrollHandle,
}
impl LearningUi {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workbench>) -> Self {
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
            correction: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .placeholder("Choose a saved correction from your conversation")
                    .rows(3)
            }),
            correction_source: None,
            notice: String::new(),
            scroll: UniformListScrollHandle::new(),
        }
    }
    pub(super) fn clear(&mut self) {
        self.status = Default::default();
        self.skills.clear();
        self.versions.clear();
        self.selected = None;
        self.correction_source = None;
        self.notice.clear();
    }
}
impl Workbench {
    fn learning_request(&mut self, name: &str, mut args: Value, cx: &mut Context<Self>) {
        let scope = self.scope();
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
    pub(super) fn learning_card(&self, cx: &mut Context<Self>) -> AnyElement {
        let weak = cx.entity().downgrade();
        let rows = uniform_list(
            "learned-procedures",
            self.learning.skills.len(),
            move |range, _, cx| {
                weak.update(cx, |view, cx| {
                    range
                        .filter_map(|i| {
                            let skill = view.learning.skills.get(i)?;
                            let name = skill.skill.name.clone();
                            let version = skill.skill.version;
                            let label = format!("{} · v{} · {}", skill.title, version, skill.state);
                            Some(
                                button(("learned-procedure", i), label)
                                    .h(px(36.))
                                    .w_full()
                                    .justify_start()
                                    .text_xs()
                                    .on_click(cx.listener(move |view, _, _, cx| {
                                        view.learning.selected = Some((name.clone(), version));
                                        view.learning.versions.clear();
                                        view.learning_request(
                                            "learning_versions",
                                            json!({"name":name}),
                                            cx,
                                        );
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
        .h(px(200.))
        .min_h(px(36.));
        let status = &self.learning.status;
        let mut panel=card().child(div().font_weight(FontWeight::MEDIUM).child("Learned procedures"))
            .child(subtitle("Verified work and your corrections can become reusable procedures for this project. You can inspect every version and undo a promotion."))
            .child(subtitle(format!("{} candidates · {} promoted · {} verified reuses · {} waiting",status.candidates,status.promoted,status.verified_reuses,status.pending_jobs)))
            .child(button("refresh-learning","Refresh procedures").on_click(cx.listener(|view,_,_,cx|view.refresh_learning(cx))));
        if self.learning.skills.is_empty() {
            panel = panel.child(subtitle(
                "No procedures yet. Xylo learns from repeated, verified results.",
            ));
        } else {
            panel = panel.child(rows);
        }
        if let Some((name, version)) = &self.learning.selected {
            let selected = self
                .learning
                .versions
                .iter()
                .chain(&self.learning.skills)
                .find(|skill| &skill.skill.name == name && &skill.skill.version == version);
            if let Some(skill) = selected {
                panel = panel.child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child(skill.title.clone()),
                );
                if let Some(evaluation) = &skill.evaluation {
                    panel=panel.child(subtitle(format!("{} · {} evaluated examples",if evaluation.passed{"Evaluation passed"}else{"More evidence needed"},evaluation.cases.len())))
                        .child(subtitle("These checks reflect observed outcomes. Review the procedure before relying on it."));
                }
                if let Some(guidance) = &skill.correction_guidance {
                    panel = panel.child(
                        TextView::markdown("learning-guidance", guidance.clone()).selectable(true),
                    );
                }
                for (i, step) in skill.steps.iter().enumerate() {
                    let label = step.tool.replace('_', " ");
                    panel = panel.child(subtitle(format!(
                        "{}. {}{}",
                        i + 1,
                        label,
                        if step.requires_approval {
                            " · approval required"
                        } else {
                            ""
                        }
                    )));
                }
                let promote_name = name.clone();
                let promote_version = *version;
                panel=panel.child(Input::new(&self.learning.review).aria_label("Procedure review evidence"))
                    .child(button("review-learned-procedure","Review and promote").disabled(skill.state=="promoted")
                        .on_click(cx.listener(move|view,_,_,cx|{
                            let evidence=view.learning.review.read(cx).value().trim().to_string();
                            if evidence.is_empty(){view.learning.notice="Describe what you checked before promoting this procedure.".into();cx.notify();return;}
                            view.learning_request("learning_review",json!({"name":promote_name,"version":promote_version,"evidence":evidence}),cx);
                        })));
                for (i, old) in self.learning.versions.iter().enumerate().take(20) {
                    if old.skill.version >= *version {
                        continue;
                    }
                    let name = old.skill.name.clone();
                    let version = old.skill.version;
                    panel = panel.child(
                        button(
                            ("rollback-procedure", i),
                            format!("Restore version {version}"),
                        )
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.learning_request(
                                "learning_rollback",
                                json!({"name":name,"version":version}),
                                cx,
                            )
                        })),
                    );
                }
            }
        }
        let source = self
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user");
        if let Some(source) = source {
            let id = source.id;
            let content = source.content.clone();
            panel = panel.child(
                button(
                    "use-latest-correction",
                    "Use my latest message as a correction",
                )
                .disabled(self.running())
                .on_click(cx.listener(move |view, _, window, cx| {
                    view.learning.correction_source = Some(id);
                    view.learning
                        .correction
                        .update(cx, |state, cx| state.set_value(content.clone(), window, cx));
                    cx.notify();
                })),
            );
        }
        if let Some(source) = self.learning.correction_source {
            panel=panel.child(subtitle("Select the guidance from your saved message. Keep its wording so the source remains verifiable."))
                .child(Textarea::new(&self.learning.correction))
                .child(button("save-learning-correction","Evaluate this correction").on_click(cx.listener(move|view,_,_,cx|{
                    let guidance=view.learning.correction.read(cx).value().trim().to_string();
                    view.learning_request("learning_correction",json!({"correction":{"user_message_id":source,"guidance":guidance}}),cx);
                })));
        }
        panel
            .child(subtitle(self.learning.notice.clone()))
            .into_any_element()
    }
}
