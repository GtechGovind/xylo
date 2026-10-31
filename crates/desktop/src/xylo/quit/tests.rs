use super::{running_tasks_detail, unsaved_kinds};

#[test]
fn unsaved_work_is_named_in_product_words() {
    assert_eq!(
        unsaved_kinds(true, false, true, false),
        vec!["message drafts", "document edits"]
    );
    assert!(unsaved_kinds(false, false, false, false).is_empty());
}

#[test]
fn running_task_prompt_counts_tasks() {
    assert_eq!(
        running_tasks_detail(1),
        "1 task is still running and will be stopped."
    );
    assert_eq!(
        running_tasks_detail(3),
        "3 tasks are still running and will be stopped."
    );
}
