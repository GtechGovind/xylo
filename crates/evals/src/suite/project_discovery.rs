//! Path-scoped rules become durable model context only after a matching file is read.
use super::*;
pub(super) fn scoped_instructions() -> Task {
    let mut task = task(
        "scoped-project-instructions",
        "Load path rules once when a matching file is touched",
        "Inspect README.md, then src/lib.rs, and summarize.",
        |trial| {
            let requests = trial
                .trace
                .exchanges
                .iter()
                .filter(|exchange| !exchange.request.tools.is_empty())
                .collect::<Vec<_>>();
            let count = |index: usize| {
                requests
                    .get(index)
                    .map(|exchange| {
                        exchange
                            .request
                            .messages
                            .iter()
                            .map(|message| message.content.matches("RUST_SCOPE_CONVENTION").count())
                            .sum::<usize>()
                    })
                    .unwrap_or_default()
            };
            vec![
                completed(trial),
                Check::expect(
                    "lazy_rules",
                    count(0) == 0 && count(1) == 0 && count(2) == 1,
                    format!(
                        "rule occurrences before/after matching read: {}, {}, {}",
                        count(0),
                        count(1),
                        count(2)
                    ),
                ),
            ]
        },
    );
    task.files = vec![
        ("README.md", "Overview".into()),
        ("src/lib.rs", "pub fn value() {}".into()),
        (
            ".xylo/rules/rust.md",
            "---\npaths: [\"src/**/*.rs\"]\n---\nRUST_SCOPE_CONVENTION".into(),
        ),
    ];
    task.script = Script::replies([
        ModelReply::calls(1, [("file_read", json!({"path":"README.md"}))]),
        ModelReply::calls(2, [("file_read", json!({"path":"src/lib.rs"}))]),
        ModelReply::text("Inspected both files."),
    ]);
    task
}
