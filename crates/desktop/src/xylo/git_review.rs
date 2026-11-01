//! Hunk actions and line comments in the Changes pane.
use super::*;
use xylo_runtime::workspace::git::{ChangeGroup, GitChange, hunks};
impl Xylo {
    pub(super) fn comment_git_line(
        &mut self,
        line: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((path, _)) = &self.git_panel.selected else {
            return;
        };
        let source = self.git_panel.diff_lines.join("\n");
        let Ok(hunks) = hunks::parse(&source) else {
            return;
        };
        let Some(hunk) = hunks.iter().rev().find(|hunk| hunk.header_line < line) else {
            return;
        };
        let Some(target) = self.git_panel.diff_lines.get(line) else {
            return;
        };
        if !target.starts_with([' ', '+', '-']) {
            return;
        }
        let mut old = hunk.old_start;
        let mut new = hunk.new_start;
        for text in &self.git_panel.diff_lines[hunk.header_line + 1..line] {
            if text.starts_with([' ', '-']) {
                old += 1;
            }
            if text.starts_with([' ', '+']) {
                new += 1;
            }
        }
        let number = if target.starts_with('-') { old } else { new }.max(1);
        let excerpt = format!(
            "Review {}:{number}\n\n{}",
            path.display(),
            hunk.patch
                .lines()
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        self.append_review_prompt(&excerpt, window, cx);
    }
    pub(super) fn append_review_prompt(
        &mut self,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let draft = self.composer.read(cx).value().trim_end().to_owned();
        let value = if draft.is_empty() {
            text.into()
        } else {
            format!("{draft}\n\n{text}")
        };
        self.composer.update(cx, |input, cx| {
            input.set_value(value, window, cx);
            input.focus(window, cx);
        });
        self.draft_changed(cx);
        cx.notify();
    }
    fn apply_git_hunk(&mut self, hunk: usize, operation: hunks::Operation, cx: &mut Context<Self>) {
        if self.git_panel.hunk_request.is_some() || self.git_panel.diff_request.is_some() {
            return;
        }
        let (Some(root), Some((path, group))) =
            (self.workspace.clone(), self.git_panel.selected.clone())
        else {
            return;
        };
        let request_id = Uuid::new_v4();
        let request = hunks::Request {
            path,
            original: self.git_panel.original.clone(),
            group,
            hunk,
            digest: hunks::digest(&self.git_panel.diff_source),
            operation,
        };
        let result = self
            .host
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Reconnect Xylo to change this hunk"))
            .and_then(|host| host.apply_git_hunk(request_id, root, request));
        match result {
            Ok(()) => {
                self.git_panel.hunk_request = Some(request_id);
                self.git_panel.error = None;
            }
            Err(error) => self.git_panel.error = Some(error.to_string()),
        }
        cx.notify();
    }
    pub(super) fn git_hunk_applied(
        &mut self,
        request_id: Uuid,
        root: PathBuf,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        if self.workspace.as_ref() != Some(&root) || self.git_panel.hunk_request != Some(request_id)
        {
            return;
        }
        self.git_panel.hunk_request = None;
        self.refresh_git(cx);
        if let Some((path, group)) = self.git_panel.selected.clone() {
            self.open_git_change(
                group,
                GitChange {
                    path,
                    original: self.git_panel.original.clone(),
                    index: ' ',
                    worktree: ' ',
                },
                cx,
            );
        }
        self.git_panel.error = result.err();
        cx.notify();
    }
    pub(super) fn git_diff_line(&self, index: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        let line = self.git_panel.diff_lines.get(index)?;
        let coordinates = diff_line_numbers(
            &self.git_panel.diff_lines,
            &self.git_panel.diff_hunks,
            index,
        );
        let addition = coordinates.is_some() && line.starts_with('+');
        let deletion = coordinates.is_some() && line.starts_with('-');
        let mut row = div()
            .id(("diff-line", index))
            .test_support()
            .group("diff-line")
            .w_full()
            .h(px(26.))
            .flex()
            .items_center()
            .gap_2()
            .font_family(mono(cx))
            .text_xs()
            .whitespace_nowrap()
            .bg(rgb(if addition {
                diff_added_bg()
            } else if deletion {
                diff_removed_bg()
            } else {
                bg()
            }))
            .text_color(rgb(text()));
        if let Some(hunk) = self
            .git_panel
            .diff_hunks
            .iter()
            .find(|hunk| hunk.header_line == index)
        {
            let group = self.git_panel.selected.as_ref().map(|(_, group)| *group)?;
            let busy =
                self.git_panel.hunk_request.is_some() || self.git_panel.diff_request.is_some();
            let operation = if group == ChangeGroup::Staged {
                hunks::Operation::Unstage
            } else {
                hunks::Operation::Stage
            };
            let label = if group == ChangeGroup::Staged {
                "Unstage"
            } else {
                "Stage"
            };
            let hunk_index = hunk.index;
            let context = line.split("@@").nth(2).unwrap_or("").trim();
            row = row
                .px_2()
                .bg(rgb(panel()))
                .text_color(rgb(text_secondary()))
                .child(format!("Lines {} → {}", hunk.old_start, hunk.new_start))
                .when(!context.is_empty(), |row| row.child(context.to_owned()));
            row = row.child(
                pill(("apply-hunk", index), label)
                    .h(px(20.))
                    .px_2()
                    .text_size(px(11.))
                    .disabled(busy)
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.apply_git_hunk(hunk_index, operation, cx)
                    })),
            );
            if group == ChangeGroup::Unstaged {
                row = row.child(
                    pill(("revert-hunk", index), "Revert")
                        .h(px(20.))
                        .px_2()
                        .text_size(px(11.))
                        .disabled(busy)
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.apply_git_hunk(hunk_index, hunks::Operation::Revert, cx)
                        })),
                );
            }
        } else if let Some((old, new)) = coordinates {
            let number = |value: Option<usize>| {
                div()
                    .w(px(40.))
                    .flex_shrink_0()
                    .text_right()
                    .text_color(rgb(muted()))
                    .child(value.map(|n| n.to_string()).unwrap_or_default())
            };
            row = row
                .child(number(old))
                .child(number(new))
                .child(
                    div()
                        .w(px(12.))
                        .flex_shrink_0()
                        .text_color(rgb(if addition {
                            success()
                        } else if deletion {
                            danger()
                        } else {
                            muted()
                        }))
                        .child(if addition {
                            "+"
                        } else if deletion {
                            "−"
                        } else {
                            ""
                        }),
                )
                .child(
                    icons::icon_button(
                        ("comment-line", index),
                        AppIcon::Add,
                        format!(
                            "Comment on line {} ({})",
                            new.or(old).unwrap_or(1),
                            if new.is_some() { "new" } else { "old" }
                        ),
                    )
                    .h(px(20.))
                    .w(px(20.))
                    .px_0()
                    .flex_shrink_0()
                    .opacity(0.)
                    .group_hover("diff-line", |style| style.opacity(1.))
                    .focus(|style| style.opacity(1.))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.comment_git_line(index, window, cx)
                    })),
                )
                .child(line.get(1..).unwrap_or(line).to_owned());
        } else {
            row = row.px_2().text_color(rgb(muted())).child(
                if line == "\\ No newline at end of file" {
                    "No newline at end of file".to_owned()
                } else {
                    line.clone()
                },
            );
        }
        Some(row.into_any_element())
    }
}

/// Return source coordinates using the original patch indices, never visible row indices.
fn diff_line_numbers(
    lines: &[String],
    hunks: &[hunks::Hunk],
    index: usize,
) -> Option<(Option<usize>, Option<usize>)> {
    let hunk = hunks.iter().rev().find(|hunk| hunk.header_line < index)?;
    let target = lines.get(index)?;
    if !target.starts_with([' ', '+', '-']) {
        return None;
    }
    let (mut old, mut new) = (hunk.old_start, hunk.new_start);
    for line in &lines[hunk.header_line + 1..index] {
        if line.starts_with([' ', '-']) {
            old += 1;
        }
        if line.starts_with([' ', '+']) {
            new += 1;
        }
    }
    Some((
        (!target.starts_with('+')).then_some(old),
        (!target.starts_with('-')).then_some(new),
    ))
}
#[cfg(all(test, not(feature = "validation")))]
#[path = "git_review/tests.rs"]
mod tests;
