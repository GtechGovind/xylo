//! Compact, wrapped change previews for chat. The full patch stays available
//! through the tool's copy and full-output actions.
use super::*;

#[derive(Debug, PartialEq, Eq)]
enum Row {
    File(String),
    Section(String),
    Line {
        old: Option<usize>,
        new: Option<usize>,
        mark: char,
        text: String,
    },
}

struct Preview {
    rows: Vec<Row>,
    added: usize,
    removed: usize,
    shortened: bool,
}

fn preview(patch: &str) -> Preview {
    let mut result = Preview {
        rows: Vec::new(),
        added: 0,
        removed: 0,
        shortened: patch.ends_with('…'),
    };
    let (mut old, mut new) = (None, None);
    let mut previous_path = None;
    for (index, line) in patch.lines().enumerate() {
        if index == 600 {
            result.shortened = true;
            break;
        }
        if line.starts_with("diff --git ") {
            old = None;
            new = None;
        } else if let Some(header) = line.strip_prefix("@@ ") {
            let fields: Vec<_> = header.split_whitespace().collect();
            let coordinate = |value: &str, sign| {
                value
                    .strip_prefix(sign)?
                    .split(',')
                    .next()?
                    .parse::<usize>()
                    .ok()
            };
            if fields.len() >= 3 && fields[2] == "@@" {
                old = coordinate(fields[0], '-');
                new = coordinate(fields[1], '+');
                if let (Some(before), Some(after)) = (old, new) {
                    let start = if after == 0 { before } else { after };
                    result.rows.push(Row::Section(format!("From line {start}")));
                    continue;
                }
            }
            result.rows.push(Row::Section(line.into()));
        } else if old.is_some() && new.is_some() && line.starts_with([' ', '+', '-']) {
            let mark = line.chars().next().unwrap();
            result.rows.push(Row::Line {
                old: (mark != '+').then_some(old.unwrap()),
                new: (mark != '-').then_some(new.unwrap()),
                mark,
                text: line[1..].into(),
            });
            if mark != '+' {
                old = old.map(|n| n.saturating_add(1));
            }
            if mark != '-' {
                new = new.map(|n| n.saturating_add(1));
            }
            result.added += usize::from(mark == '+');
            result.removed += usize::from(mark == '-');
        } else if let Some(path) = line.strip_prefix("--- ") {
            previous_path = Some(path.trim_start_matches("a/").to_owned());
        } else if let Some(path) = line.strip_prefix("+++ ") {
            let path = if path == "/dev/null" {
                previous_path.take().unwrap_or_else(|| path.into())
            } else {
                path.trim_start_matches("b/").into()
            };
            result.rows.push(Row::File(path));
        } else if !line.starts_with("index ") && !line.is_empty() {
            // Preserve unsupported/binary patches and newline markers as text.
            result.rows.push(Row::Line {
                old: None,
                new: None,
                mark: ' ',
                text: line.into(),
            });
        }
    }
    result
}

pub(super) fn view(id: impl Into<ElementId>, patch: &str, cx: &App) -> AnyElement {
    let preview = preview(patch);
    let files = preview
        .rows
        .iter()
        .filter(|row| matches!(row, Row::File(_)))
        .count();
    let mut surface = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .rounded(px(8.))
        .border_1()
        .border_color(rgb(border()))
        .overflow_hidden();
    if preview.added + preview.removed > 0 {
        surface = surface.child(
            div()
                .px_3()
                .py_2()
                .flex()
                .items_center()
                .gap_2()
                .bg(rgb(panel()))
                .text_size(px(META_SIZE))
                .child(
                    div()
                        .flex_1()
                        .text_color(rgb(muted()))
                        .child(if preview.shortened {
                            "Changes in preview"
                        } else {
                            "Changes"
                        }),
                )
                .when(preview.added > 0, |row| {
                    row.child(badge(format!("+{}", preview.added), BadgeTone::Success))
                })
                .when(preview.removed > 0, |row| {
                    row.child(badge(format!("−{}", preview.removed), BadgeTone::Danger))
                }),
        );
    }
    let mut body = div().w_full().min_w_0().flex().flex_col();
    for row in preview.rows {
        match row {
            Row::File(path) if files > 1 => {
                body = body.child(div().px_3().py_2().text_color(rgb(muted())).child(path))
            }
            Row::File(_) => {}
            Row::Section(label) => {
                body = body.child(
                    div()
                        .px_3()
                        .py_1()
                        .bg(rgb(well()))
                        .text_size(px(META_SIZE))
                        .text_color(rgb(muted()))
                        .child(label),
                )
            }
            Row::Line {
                old,
                new,
                mark,
                text: content,
            } => {
                let tone = match mark {
                    '+' => success(),
                    '-' => danger(),
                    _ => muted(),
                };
                let row = div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_start()
                    .py(px(2.))
                    .when(mark == '+', |row| row.bg(rgb(diff_added_bg()).opacity(0.6)))
                    .when(mark == '-', |row| {
                        row.bg(rgb(diff_removed_bg()).opacity(0.6))
                    })
                    .font_family(mono(cx))
                    .text_size(px(12.))
                    .line_height(relative(1.65))
                    .child(
                        div()
                            .w(px(34.))
                            .flex_shrink_0()
                            .pr_1()
                            .text_right()
                            .text_color(rgb(muted()))
                            .child(old.map(|n| n.to_string()).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .w(px(34.))
                            .flex_shrink_0()
                            .pr_1()
                            .text_right()
                            .text_color(rgb(muted()))
                            .child(new.map(|n| n.to_string()).unwrap_or_default()),
                    )
                    .child(
                        div()
                            .w(px(20.))
                            .flex_shrink_0()
                            .text_center()
                            .text_color(rgb(tone))
                            .child(mark.to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .pr_3()
                            .whitespace_normal()
                            .text_color(rgb(text()))
                            .child(if content.is_empty() {
                                " ".into()
                            } else {
                                content
                            }),
                    );
                body = body.child(row);
            }
        }
    }
    surface = surface.child(
        div()
            .id(id)
            .test_support()
            .w_full()
            .min_w_0()
            .max_h(px(320.))
            .overflow_y_scroll()
            .bg(rgb(code_bg()))
            .child(body),
    );
    if preview.shortened {
        surface = surface.child(
            div()
                .px_3()
                .py_2()
                .text_size(px(META_SIZE))
                .text_color(rgb(muted()))
                .child("Preview shortened · Open full output to see the complete change."),
        );
    }
    surface.into_any_element()
}

#[cfg(all(test, not(feature = "validation")))]
mod tests {
    use super::{Row, preview};
    #[test]
    fn coordinates_and_counts_follow_hunks_without_counting_file_headers() {
        let p = preview(
            "--- a/README.md\n+++ b/README.md\n@@ -24,2 +24,3 @@\n old\n-before\n+after\n+++literal source\n@@ -40 +41 @@\n tail\n",
        );
        assert_eq!((p.added, p.removed), (2, 1));
        assert!(p.rows.contains(&Row::Line {
            old: Some(25),
            new: None,
            mark: '-',
            text: "before".into()
        }));
        assert!(p.rows.contains(&Row::Line {
            old: None,
            new: Some(26),
            mark: '+',
            text: "++literal source".into()
        }));
        assert!(p.rows.contains(&Row::Line {
            old: Some(40),
            new: Some(41),
            mark: ' ',
            text: "tail".into()
        }));
    }
    #[test]
    fn deleted_files_and_unsupported_output_keep_their_meaning() {
        let p = preview("--- a/old.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone\n");
        assert_eq!(p.rows[0], Row::File("old.rs".into()));
        assert_eq!((p.added, p.removed), (0, 1));
        let p = preview("Binary files a/photo.png and b/photo.png differ");
        assert!(matches!(&p.rows[0], Row::Line { text, .. } if text.contains("Binary files")));
    }
}
