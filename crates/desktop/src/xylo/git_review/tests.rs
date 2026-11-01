use crate::xylo::Xylo;
use gpui_kit::{AppContext, TestAppContext, WindowOptions};
#[gpui_kit::test]
fn commenting_a_changed_line_preserves_the_draft_and_never_sends(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            cx.new(|cx| Xylo::new(Err(anyhow::anyhow!("Isolated fixture")), window, cx))
        })
        .unwrap()
    });
    cx.update_window(window,|_,window,cx|view.update(cx,|view,cx|{
        view.git_panel.selected=Some(("src/file.rs".into(),xylo_runtime::workspace::git::ChangeGroup::Unstaged));
        view.git_panel.diff_lines="diff --git a/src/file.rs b/src/file.rs\n--- a/src/file.rs\n+++ b/src/file.rs\n@@ -10,2 +10,2 @@\n same\n-old\n+new".lines().map(str::to_owned).collect();
        view.composer.update(cx,|input,cx|input.set_value("Keep this draft",window,cx));
        view.comment_git_line(6,window,cx);
        let draft=view.composer.read(cx).value().to_string();
        assert!(draft.starts_with("Keep this draft\n\n"));
        assert!(draft.contains("src/file.rs:11")&&draft.contains("-old")&&draft.contains("+new"));
        assert!(!view.sending_turn());
    })).unwrap();
}
