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

#[test]
fn line_coordinates_follow_both_sides_and_reset_for_each_hunk() {
    let patch = "diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -10,3 +20,3 @@ first\n same\n-old\n-older\n+new\n+newer\n@@ -40,0 +50,2 @@ second\n+one\n+two\n";
    let lines: Vec<_> = patch.lines().map(str::to_owned).collect();
    let hunks = xylo_runtime::workspace::git::hunks::parse(patch).unwrap();
    let coords = |index| super::diff_line_numbers(&lines, &hunks, index);
    assert_eq!(coords(2), None);
    assert_eq!(coords(3), None);
    assert_eq!(coords(4), Some((Some(10), Some(20))));
    assert_eq!(coords(5), Some((Some(11), None)));
    assert_eq!(coords(6), Some((Some(12), None)));
    assert_eq!(coords(7), Some((None, Some(21))));
    assert_eq!(coords(8), Some((None, Some(22))));
    assert_eq!(coords(9), None);
    assert_eq!(coords(10), Some((None, Some(50))));
    assert_eq!(coords(11), Some((None, Some(51))));
}
