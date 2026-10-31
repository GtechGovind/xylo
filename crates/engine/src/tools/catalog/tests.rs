use super::*;
use serde_json::json;

/// A fresh index per query, as the reused index must behave.
fn rank(entries: &[Entry], query: &str, limit: usize) -> Vec<usize> {
    Index::new(entries).rank(query, limit)
}

fn entry<'a>(name: &'a str, description: &'a str, parameters: &'a Value) -> Entry<'a> {
    Entry {
        name,
        description,
        parameters,
        server: None,
    }
}

#[test]
fn terms_split_names_fold_plurals_and_drop_stop_words() {
    assert_eq!(terms("file_read"), ["file", "read"]);
    assert_eq!(terms("readFile"), ["read", "file"]);
    assert_eq!(
        terms("List the Files in a directory"),
        ["list", "file", "directory"]
    );
    assert_eq!(terms("issues and queries"), ["issue", "query"]);
    assert_eq!(terms("class process"), ["class", "process"]);
}

#[test]
fn ranking_orders_by_relevance_not_registration() {
    let none = json!({});
    let entries = [
        entry("files_list", "List one workspace directory", &none),
        entry("browser_open", "Open a web page in the browser", &none),
        entry(
            "browser_screenshot",
            "Capture a screenshot of the open page",
            &none,
        ),
        entry("document_read", "Read a PDF or office document", &none),
    ];
    assert_eq!(rank(&entries, "take a browser screenshot", 8), [2, 1]);
    assert_eq!(rank(&entries, "read pdf", 8), [3]);
    assert!(rank(&entries, "teleport", 8).is_empty());
    assert!(rank(&entries, "", 8).is_empty());
}

#[test]
fn ranking_uses_parameter_names_and_limits_results() {
    let with_url = json!({"properties":{"url":{"type":"string"}}});
    let none = json!({});
    let entries = [
        entry("alpha", "Does one thing", &none),
        entry("beta", "Does another thing", &with_url),
    ];
    assert_eq!(rank(&entries, "url", 8), [1]);
    assert_eq!(rank(&entries, "thing", 1).len(), 1);
}

#[test]
fn an_exact_tool_name_in_the_query_ranks_that_tool_first() {
    let none = json!({});
    let entries = [
        entry("image_describe", "Describe an image image image", &none),
        entry("image_generate", "Generate one image", &none),
    ];
    assert_eq!(rank(&entries, "image_generate", 8)[0], 1);
}

#[test]
fn built_in_tools_outrank_equally_matching_connector_tools() {
    let none = json!({});
    let mut connector = entry("search_files", "Search files", &none);
    connector.server = Some("Drive");
    let entries = [connector, entry("search_files", "Search files", &none)];
    assert_eq!(rank(&entries, "search files", 8), [1, 0]);
}

#[test]
fn keyword_stuffed_connector_descriptions_do_not_buy_rank() {
    let none = json!({});
    let stuffed = format!("{} commit", "helper ".repeat(UNTRUSTED_DESCRIPTION_TERMS));
    let mut connector = entry("helper", &stuffed, &none);
    connector.server = Some("Sneaky");
    let entries = [connector, entry("git_commit", "Record a commit", &none)];
    assert_eq!(rank(&entries, "commit", 8), [1]);
}

#[test]
fn nearest_names_cover_reordering_spelling_and_typos() {
    let names = ["file_read", "file_write", "files_list", "shell_command"];
    assert_eq!(nearest("read_file", names)[0], "file_read");
    assert_eq!(nearest("shel_command", names), ["shell_command"]);
    assert_eq!(
        nearest("oldString", ["old_string", "new_string"]),
        ["old_string"]
    );
    assert!(nearest("teleport", names).is_empty());
    assert!(nearest("file_read", ["file_read"]).is_empty());
}

#[test]
fn signatures_list_types_requirements_and_enum_values() {
    let schema = json!({"type":"object","properties":{
        "path":{"type":"string"},
        "limit":{"type":"integer"},
        "status":{"type":"string","enum":["open","closed"]},
        "todos":{"type":"array","items":{"type":"object","properties":{}}}
    },"required":["path"]});
    let line = signature(&schema);
    assert!(line.contains("path: string (required)"), "{line}");
    assert!(line.contains("limit: integer"), "{line}");
    assert!(line.contains("status: one of open, closed"), "{line}");
    assert!(line.contains("todos: array of object"), "{line}");
    assert_eq!(
        signature(&json!({"type":"object","properties":{}})),
        "no arguments ({})"
    );
}

#[test]
fn short_descriptions_keep_the_first_sentence_within_the_limit() {
    assert_eq!(
        short_description("Reads a file. Then more."),
        "Reads a file"
    );
    let long = "word ".repeat(100);
    let short = short_description(&long);
    assert!(short.chars().count() <= SHORT_DESCRIPTION_CHARS);
    assert!(short.ends_with('…'));
}

#[test]
fn connector_tools_never_outrank_the_built_in_tool_they_shadow() {
    let short = json!({});
    let long = "Read a text file in the workspace as numbered lines, with offsets, limits and paging across many many words of detail";
    let mut connector = entry("file_read", "Read file", &short);
    connector.server = Some("Files");
    let entries = [entry("file_read", long, &short), connector];
    assert_eq!(rank(&entries, "read file", 8), [0, 1]);
    assert_eq!(rank(&entries, "file_read", 8), [0, 1]);
}

#[test]
fn one_index_answers_every_query_like_a_fresh_one() {
    let none = json!({});
    let mut connector = entry("file_read", "Read file", &none);
    connector.server = Some("Files");
    let entries = [
        entry("file_read", "Read a text file", &none),
        entry("browser_open", "Open a web page in the browser", &none),
        connector,
    ];
    let index = Index::new(&entries);
    for query in ["read file", "browser", "file_read", "teleport", ""] {
        assert_eq!(index.rank(query, 8), rank(&entries, query, 8), "{query}");
    }
}
