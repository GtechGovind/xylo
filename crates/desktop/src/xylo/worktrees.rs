//! New-chat isolation, selected checkout labels and retained-worktree settings.
use super::*;

pub(super) fn branch_label(path: &Path) -> Option<String> {
    (path.parent()?.file_name()? == "worktrees" && path.parent()?.parent()?.file_name()? == ".xylo")
        .then(|| format!("xylo/{}", path.file_name().unwrap().to_string_lossy()))
}
impl Xylo {
    pub(super) fn start_worktree_chat(&mut self, cx: &mut Context<Self>) {
        if self.worktree_pending.is_some()
            || self.chat_busy(self.session)
            || self
                .conversations
                .iter()
                .any(|chat| chat.id == self.session)
        {
            return;
        }
        let Some(root) = self.workspace.clone() else {
            return;
        };
        if self.editor_dirty {
            self.error = Some("Save your editor changes before creating a worktree.".into());
            return;
        }
        if let Some(host) = &self.host {
            match host.create_chat_worktree(self.session, root) {
                Ok(()) => self.worktree_pending = Some(self.session),
                Err(error) => self.error = Some(error.to_string()),
            }
        }
        cx.notify();
    }
    pub(super) fn chat_worktree_changed(
        &mut self,
        session: Uuid,
        workspace: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(chat) = self
            .conversations
            .iter_mut()
            .find(|chat| chat.id == session)
        {
            chat.workspace = workspace.clone();
        }
        self.sidebar_state.dirty = true;
        if self.worktree_pending == Some(session) {
            self.worktree_pending = None;
        }
        if let Some(request) = self
            .turns
            .get_mut(&session)
            .and_then(|turn| turn.request.as_mut())
        {
            request.workspace = workspace.clone();
        }
        if session != self.session {
            return;
        }
        if self.workspace.as_ref() == Some(&workspace) {
            return;
        }
        let previous_draft = self.current_draft_key();
        if !self.draft_before_switch(cx) {
            self.error = Some("The chat changed checkout. Save or shorten the open draft, then reopen the chat to follow it.".into());
            cx.notify();
            return;
        }
        // Preserve unsaved editor content if the model switched while editing.
        // It stays bound to the original absolute editor path until saved.
        let editor = self
            .editor_dirty
            .then(|| self.editor_path.clone())
            .flatten();
        self.park_workspace(cx);
        self.workspace = Some(workspace.clone());
        self.carry_draft_to_checkout(&previous_draft, cx);
        if let Some(path) = editor {
            self.editor_path = Some(path);
            self.editor_dirty = true;
        }
        self.settings.last_workspace = Some(workspace.clone());
        self.after_workspace_change(window, cx);
        if let Some(host) = &self.host {
            let result = host.open_workspace(workspace);
            self.result(result, cx);
        }
        self.persist_ui(cx);
        cx.notify();
    }
    pub(super) fn worktree_settings(&self, cx: &mut Context<Self>) -> Div {
        let mut view = div().flex().flex_col().gap(px(10.)).child(
            secondary("refresh-worktrees", "Refresh worktrees").on_click(cx.listener(
                |view, _, _, cx| {
                    if let Some(host) = &view.host {
                        let result = host.list_worktrees(view.workspace.clone());
                        view.result(result, cx);
                    }
                },
            )),
        );
        if self.worktrees.is_empty() {
            view = view.child("No retained worktrees.");
        }
        for (index, tree) in self.worktrees.iter().enumerate() {
            let path = tree.path.clone();
            let force_path = path.clone();
            view = view.child(div().flex().items_center().gap(px(8.)).child(tree.branch.clone())
                .child(secondary(("remove-worktree", index), "Remove if clean").on_click(cx.listener(move |view, _, _, cx| {
                    if view.workspace.as_ref() == Some(&path) || view.terminal_cache.contains_key(&path) { view.error = Some("Close this worktree’s terminals and open another project before removing it.".into()); cx.notify(); return; }
                    if let Some(host) = &view.host { let result = host.remove_worktree(path.clone(), false); view.result(result, cx); }
                })))
                .child(secondary(("force-remove-worktree", index), "Discard and remove…").on_click(cx.listener(move |view, _, window, cx| {
                    if view.workspace.as_ref() == Some(&force_path) || view.terminal_cache.contains_key(&force_path) { view.error = Some("Close this worktree’s terminals and open another project before removing it.".into()); cx.notify(); return; }
                    let path = force_path.clone();
                    cx.spawn_in(window, async move |this, cx| {
                        let decision = rfd::AsyncMessageDialog::new().set_title("Discard worktree changes?")
                            .set_description(format!("Delete {} and all uncommitted or ignored files in it? Git branches and commits are retained.", path.display()))
                            .set_buttons(rfd::MessageButtons::OkCancel).show().await;
                        if decision == rfd::MessageDialogResult::Ok {
                            let _ = this.update_in(cx, |view, _, cx| { if let Some(host) = &view.host { let result = host.remove_worktree(path, true); view.result(result, cx); } });
                        }
                    }).detach();
                }))));
        }
        view
    }
}
