//! Bounded, semantic tool output prepared when the engine updates a card.
use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct ToolPresentation {
    pub verb: String,
    pub target: String,
    pub meta: Option<String>,
    pub question: String,
    pub error: Option<String>,
    pub body: ToolBody,
    pub raw_input: String,
    pub raw_result: Option<String>,
    pub parsed_output: Option<std::sync::Arc<Value>>,
}
#[derive(Clone, Debug, Default)]
pub enum ToolBody {
    #[default]
    Empty,
    Files {
        entries: Vec<FileEntry>,
        remaining: usize,
    },
    Excerpt {
        lines: Vec<OutputLine>,
        remaining: usize,
    },
    Search {
        groups: Vec<SearchGroup>,
        remaining: usize,
    },
    Command {
        command: String,
        exit: Option<i64>,
        stdout: Vec<OutputLine>,
        stderr: Vec<OutputLine>,
        truncated: bool,
    },
    Markdown(String),
    Diff(String),
    Message(String),
}
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub name: String,
    pub directory: bool,
}
#[derive(Clone, Debug)]
pub struct OutputLine {
    pub number: Option<u64>,
    pub text: String,
}
#[derive(Clone, Debug)]
pub struct SearchGroup {
    pub path: String,
    pub matches: Vec<OutputLine>,
}

const ROW_LIMIT: usize = 24;
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}
fn short(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let kept = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{kept}…")
    } else {
        kept
    }
}
fn lines(value: &str, numbered: bool, start: u64) -> (Vec<OutputLine>, usize) {
    let total = value.lines().count();
    let rows = value
        .lines()
        .take(ROW_LIMIT)
        .enumerate()
        .map(|(index, line)| {
            let parsed = numbered
                .then(|| line.split_once('\t'))
                .flatten()
                .and_then(|(n, content)| n.trim().parse::<u64>().ok().map(|n| (n, content)));
            OutputLine {
                number: if numbered {
                    Some(parsed.map_or(start + index as u64, |(n, _)| n))
                } else {
                    None
                },
                text: short(parsed.map_or(line, |(_, content)| content), 400),
            }
        })
        .collect();
    (rows, total.saturating_sub(ROW_LIMIT))
}
fn readable(value: &Value) -> Option<&str> {
    value.as_str().or_else(|| {
        [
            "result", "response", "answer", "message", "text", "content", "output",
        ]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
    })
}
fn search_match(row: &str) -> Option<(&str, u64, &str)> {
    row.match_indices(':').find_map(|(index, _)| {
        let (number, content) = row[index + 1..].split_once(':')?;
        number
            .parse()
            .ok()
            .map(|number| (&row[..index], number, content))
    })
}
impl ToolPresentation {
    pub fn prepare(name: &str, input: &Value, result: Option<&str>, success: Option<bool>) -> Self {
        let parsed = result.and_then(|raw| serde_json::from_str::<Value>(raw).ok());
        let summary = parsed
            .as_ref()
            .and_then(|value| value.get("summary"))
            .or(parsed.as_ref());
        let empty = Value::Null;
        let output = summary.unwrap_or(&empty);
        let verb = match name {
            "files_list" => "List",
            "find_files" => "Find",
            "file_read" => "Read",
            "file_write" => "Write",
            "file_edit" => "Edit",
            "search_project" => "Search",
            "run_command" | "shell_command" => "Run",
            "agent_delegate" => "Delegate",
            "image_generate" => "Generate image",
            "git_status" => "Git status",
            "git_diff" => "Git diff",
            "memory_search" => "Recall",
            "memory_remember" => "Remember",
            _ => name,
        };
        let mut target = [
            "query", "pattern", "path", "task", "command", "program", "name", "url",
        ]
        .iter()
        .find_map(|key| input.get(*key).and_then(Value::as_str))
        .unwrap_or("")
        .to_owned();
        if name == "files_list" && target.is_empty() {
            target = ".".into();
        }
        let mut shown = Self {
            verb: verb.replace('_', " "),
            target: short(&target, 160),
            question: match name {
                "file_write" | "file_edit" => "Allow this change to your project?",
                "run_command" | "shell_command" => "Allow this command in your project?",
                "agent_delegate" => "Start a helper agent?",
                _ => "Allow this action?",
            }
            .into(),
            raw_input: short(
                &serde_json::to_string_pretty(input).unwrap_or_default(),
                8000,
            ),
            raw_result: result.map(|raw| short(raw, 12000)),
            ..Default::default()
        };
        if success == Some(false) {
            let error = parsed
                .as_ref()
                .and_then(|v| v.get("error"))
                .and_then(|v| {
                    v.as_str()
                        .or_else(|| v.get("message").and_then(Value::as_str))
                })
                .or_else(|| output.get("error").and_then(Value::as_str))
                .or_else(|| readable(output))
                .or_else(|| result.filter(|_| parsed.is_none()));
            shown.error = Some(short(
                error.unwrap_or("The action could not finish. Open Raw details for diagnostics."),
                500,
            ));
        }
        shown.body = match name {
            "files_list" | "find_files" => {
                let values = output
                    .get(if name == "files_list" {
                        "entries"
                    } else {
                        "files"
                    })
                    .and_then(Value::as_array);
                let total = output
                    .get("count")
                    .and_then(Value::as_u64)
                    .map(|v| v as usize)
                    .unwrap_or_else(|| values.map_or(0, Vec::len));
                if values.is_some() {
                    shown.meta = Some(format!(
                        "{total} {}",
                        if name == "files_list" {
                            "items"
                        } else {
                            "files"
                        }
                    ));
                }
                let entries = values
                    .into_iter()
                    .flatten()
                    .take(ROW_LIMIT)
                    .filter_map(Value::as_str)
                    .map(|name| FileEntry {
                        directory: name.ends_with('/'),
                        name: short(name.trim_end_matches('/'), 300),
                    })
                    .collect::<Vec<_>>();
                if result.is_none() {
                    ToolBody::Empty
                } else if total == 0 {
                    ToolBody::Message("No files found".into())
                } else {
                    ToolBody::Files {
                        remaining: total.saturating_sub(entries.len()),
                        entries,
                    }
                }
            }
            "file_read" => {
                let start = output
                    .get("start_line")
                    .and_then(Value::as_u64)
                    .unwrap_or(1);
                let end = output
                    .get("end_line")
                    .and_then(Value::as_u64)
                    .unwrap_or(start);
                let total = output.get("total_lines").and_then(Value::as_u64);
                if let Some(total) = total {
                    shown.meta = Some(format!("{start}–{end} · {total} lines"));
                }
                let (lines, remaining) = lines(text(output, "content"), true, start);
                if result.is_none() {
                    ToolBody::Empty
                } else if lines.is_empty() {
                    ToolBody::Message("Empty file".into())
                } else {
                    ToolBody::Excerpt { lines, remaining }
                }
            }
            "search_project" => {
                let count = output.get("matches").and_then(Value::as_u64).unwrap_or(0);
                let files = output.get("files").and_then(Value::as_u64).unwrap_or(0);
                if output.get("matches").is_some() {
                    shown.meta = Some(format!("{count} matches · {files} files"));
                }
                let mut groups = Vec::<SearchGroup>::new();
                let all = text(output, "results");
                let paths: Vec<_> = all
                    .lines()
                    .take(ROW_LIMIT * 4)
                    .filter_map(search_match)
                    .map(|(path, _, _)| path)
                    .collect();
                let mut kept = 0;
                for row in all
                    .lines()
                    .filter(|row| !row.is_empty() && *row != "No matches" && *row != "--")
                    .take(ROW_LIMIT)
                {
                    let matched = search_match(row);
                    // Ripgrep uses path-line-context for neighboring lines.
                    // Match a known file prefix so hyphens in paths stay intact.
                    let found = matched
                        .or_else(|| {
                            paths.iter().find_map(|path| {
                                let tail = row.strip_prefix(*path)?.strip_prefix('-')?;
                                let (number, content) = tail.split_once('-')?;
                                number.parse().ok().map(|number| (*path, number, content))
                            })
                        })
                        .map(|(path, number, content)| (path, Some(number), content));
                    let (path, number, content) = found.unwrap_or((row, None, ""));
                    let group =
                        if let Some(index) = groups.iter().position(|group| group.path == path) {
                            index
                        } else {
                            groups.push(SearchGroup {
                                path: short(path, 300),
                                matches: vec![],
                            });
                            groups.len() - 1
                        };
                    if number.is_some() || !content.is_empty() {
                        groups[group].matches.push(OutputLine {
                            number,
                            text: short(content, 400),
                        });
                    }
                    kept += usize::from(matched.is_some() || found.is_none());
                }
                if result.is_none() {
                    ToolBody::Empty
                } else if groups.is_empty() {
                    ToolBody::Message("No matches".into())
                } else {
                    ToolBody::Search {
                        groups,
                        remaining: (count as usize).saturating_sub(kept),
                    }
                }
            }
            "run_command" | "shell_command" => {
                let command = if name == "shell_command" {
                    text(input, "command").to_owned()
                } else {
                    std::iter::once(text(input, "program"))
                        .chain(
                            input
                                .get("args")
                                .and_then(Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str),
                        )
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                shown.target = short(&command, 160);
                let exit = output.get("exit_code").and_then(Value::as_i64);
                shown.meta = exit.map(|code| format!("exit {code}"));
                let (stdout, more_out) = lines(text(output, "stdout"), false, 0);
                let (stderr, more_err) = lines(text(output, "stderr"), false, 0);
                if result.is_none() {
                    ToolBody::Empty
                } else {
                    ToolBody::Command {
                        command: short(&command, 2000),
                        exit,
                        stdout,
                        stderr,
                        truncated: more_out + more_err > 0
                            || output
                                .get("output_truncated")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                    }
                }
            }
            "agent_delegate" => readable(output)
                .map(|text| ToolBody::Markdown(short(text, 16000)))
                .unwrap_or(ToolBody::Empty),
            "file_write" | "file_edit" | "git_diff" if !text(output, "diff").is_empty() => {
                ToolBody::Diff(short(text(output, "diff"), 12000))
            }
            "image_generate" => ToolBody::Empty,
            _ => readable(output)
                .map(|text| ToolBody::Message(short(text, 4000)))
                .unwrap_or_else(|| {
                    if result.is_some() {
                        ToolBody::Message("Result available in Raw details".into())
                    } else {
                        ToolBody::Empty
                    }
                }),
        };
        // A failed response must never also claim an empty successful listing.
        if shown.error.is_some() && !matches!(shown.body, ToolBody::Command { .. }) {
            shown.body = ToolBody::Empty;
        }
        shown.parsed_output = parsed.map(std::sync::Arc::new);
        shown
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolBody, ToolPresentation};
    use serde_json::json;
    fn prepare(name: &str, input: Value, summary: Value) -> ToolPresentation {
        ToolPresentation::prepare(
            name,
            &input,
            Some(&json!({"status":"success","summary":summary}).to_string()),
            Some(true),
        )
    }
    use serde_json::Value;
    #[test]
    fn directory_listing_is_names_and_types_without_envelopes() {
        let shown = prepare(
            "files_list",
            json!({"path":"crates"}),
            json!({"entries":["core/","Cargo.toml"]}),
        );
        assert_eq!(shown.verb, "List");
        assert_eq!(shown.target, "crates");
        let ToolBody::Files { entries, remaining } = shown.body else {
            panic!("Expected semantic file rows")
        };
        assert!(entries[0].directory);
        assert_eq!(entries[0].name, "core");
        assert_eq!(remaining, 0);
    }
    #[test]
    fn read_preserves_real_line_numbers_and_bounds_large_unicode_content() {
        let content = (700..1700)
            .map(|line| format!("{line:>6}\tlet café = {line};\n"))
            .collect::<String>();
        let shown = prepare(
            "file_read",
            json!({"path":"src/main.rs"}),
            json!({"path":"src/main.rs","start_line":700,"end_line":1699,"total_lines":2000,"content":content}),
        );
        assert_eq!(shown.meta.as_deref(), Some("700–1699 · 2000 lines"));
        let ToolBody::Excerpt { lines, remaining } = shown.body else {
            panic!("Expected bounded excerpt")
        };
        assert_eq!(lines[0].number, Some(700));
        assert_eq!(lines[0].text, "let café = 700;");
        assert!(lines.len() <= 24);
        assert_eq!(remaining, 976);
    }
    #[test]
    fn search_groups_matches_by_file_and_keeps_colons_in_content() {
        let shown = prepare(
            "search_project",
            json!({"query":"fn"}),
            json!({"matches":3,"files":2,"results":"src/a.rs:12:fn a() { x: 1 }\nsrc/b.rs:9:fn b() {}\nsrc/a.rs:20:fn c() {}\n"}),
        );
        let ToolBody::Search { groups, remaining } = shown.body else {
            panic!("Expected grouped matches")
        };
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].matches.len(), 2);
        assert_eq!(groups[0].matches[0].number, Some(12));
        assert!(groups[0].matches[0].text.contains("x: 1"));
        assert_eq!(remaining, 0);
    }
    #[test]
    fn commands_show_actual_exit_and_output_while_errors_are_readable() {
        let shown = prepare(
            "run_command",
            json!({"program":"cargo","args":["check"]}),
            json!({"exit_code":1,"stdout":"Checking xylo","stderr":"error: missing import"}),
        );
        let ToolBody::Command {
            command,
            exit,
            stderr,
            ..
        } = shown.body
        else {
            panic!("Expected command output")
        };
        assert_eq!(command, "cargo check");
        assert_eq!(exit, Some(1));
        assert_eq!(stderr[0].text, "error: missing import");
        let error = ToolPresentation::prepare(
            "files_list",
            &json!({"path":"missing"}),
            Some("{\"status\":\"failed\",\"error\":\"Directory does not exist\"}"),
            Some(false),
        );
        assert_eq!(error.error.as_deref(), Some("Directory does not exist"));
        assert!(!error.error.unwrap().starts_with('{'));
    }
    #[test]
    fn delegate_result_is_readable_markdown_and_unknown_json_stays_in_diagnostics() {
        let delegate = prepare(
            "agent_delegate",
            json!({"task":"Review navigation"}),
            json!({"response":"## Findings\nUse a sidebar."}),
        );
        assert!(
            matches!(delegate.body,ToolBody::Markdown(ref text) if text.starts_with("## Findings"))
        );
        let unknown = prepare(
            "custom_tool",
            json!({"internal":"argument"}),
            json!({"opaque":{"protocol":true}}),
        );
        assert!(matches!(unknown.body,ToolBody::Message(ref text) if !text.contains("protocol")));
        assert!(unknown.raw_result.unwrap().contains("protocol"));
    }
    #[test]
    fn search_context_stays_with_its_file_and_pending_tools_have_no_fake_counts() {
        let shown = prepare(
            "search_project",
            json!({"query":"main","context":1}),
            json!({"matches":1,"files":1,"results":"src/a-b.rs-10-before\nsrc/a-b.rs:11:main\nsrc/a-b.rs-12-after\n--\n"}),
        );
        let ToolBody::Search { groups, .. } = shown.body else {
            panic!("Expected grouped context")
        };
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0]
                .matches
                .iter()
                .map(|line| line.number)
                .collect::<Vec<_>>(),
            vec![Some(10), Some(11), Some(12)]
        );
        let pending = ToolPresentation::prepare("files_list", &json!({"path":"src"}), None, None);
        assert!(pending.meta.is_none());
    }
}
