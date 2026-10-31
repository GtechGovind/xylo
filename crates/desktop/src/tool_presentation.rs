//! Bounded, semantic tool output prepared when the engine updates a card.
//!
//! The user sees only human-readable presentation: a diff, a command and its
//! output, a file excerpt, search hits or a short plain summary. Model-facing
//! envelopes (skill instructions, raw JSON) never reach the screen; the raw
//! input and output stay available to copy from the card.
use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct ToolPresentation {
    pub verb: String,
    pub target: String,
    pub meta: Option<String>,
    pub question: String,
    pub error: Option<String>,
    pub body: ToolBody,
    /// The complete arguments, pretty-printed once when the card changes.
    pub pretty_input: std::sync::Arc<str>,
    pub parsed_output: Option<std::sync::Arc<Value>>,
    /// Extra context shown on hover, such as a skill's description.
    pub tooltip: Option<String>,
}
impl ToolPresentation {
    /// Whether the row has a readable body to expand into.
    pub fn expandable(&self) -> bool {
        !matches!(self.body, ToolBody::Empty)
    }
}
/// "code-review" or "pkg:code_review" as "Code review".
pub fn humanize_skill(id: &str) -> String {
    let name = id.rsplit(':').next().unwrap_or(id).replace(['-', '_'], " ");
    let mut chars = name.trim().chars();
    chars
        .next()
        .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
        .unwrap_or_default()
}
/// The first few lines of human-readable text, as a short summary.
fn first_lines(text: &str) -> String {
    const LINES: usize = 6;
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let shown = lines.by_ref().take(LINES).collect::<Vec<_>>().join("\n");
    let summary = short(&shown, 600);
    if lines.next().is_some() {
        format!("{summary}\n…")
    } else {
        summary
    }
}
/// Text for a one-line label: every run of whitespace, line breaks included,
/// becomes one space. GPUI breaks a label at `\n` even when it truncates.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
/// The first non-empty line, with an ellipsis when more lines follow.
fn first_line(text: &str) -> String {
    let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
    let first = lines.next().unwrap_or_default();
    if lines.next().is_some() {
        format!("{first} …")
    } else {
        first.to_owned()
    }
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
fn component_preview(value: &Value, depth: usize) -> String {
    if let Some(text) = readable(value).filter(|text| !text.is_empty()) {
        return short(text, 2000);
    }
    let command_output = ["stdout", "stderr"]
        .iter()
        .filter_map(|key| value.get(*key).and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .map(|text| short(text, 1000))
        .collect::<Vec<_>>();
    if !command_output.is_empty() {
        return command_output.join("\n");
    }
    if let Some(content) = value.get("content").or_else(|| value.get("contents")) {
        let blocks: Vec<&Value> = content
            .as_array()
            .map(|blocks| blocks.iter().take(ROW_LIMIT).collect())
            .unwrap_or_else(|| vec![content]);
        let texts = blocks
            .into_iter()
            .filter_map(|block| {
                block
                    .as_str()
                    .or_else(|| block.get("text").and_then(Value::as_str))
                    .or_else(|| block.get("value").and_then(Value::as_str))
            })
            .map(|text| short(text, 400))
            .collect::<Vec<_>>();
        if !texts.is_empty() {
            return short(&texts.join("\n"), 2000);
        }
    }
    if let Some(tools) = value.get("tools").and_then(Value::as_array) {
        return format!("{} tools available", tools.len());
    }
    if depth < 2
        && let Some(result) = value.get("result")
    {
        return component_preview(result, depth + 1);
    }
    "Completed".into()
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
            "extension_skill" => "Using skill",
            "extension_resource" => "Read skill file",
            "extension_component" => "Run plugin",
            _ => name,
        };
        let pretty_input: std::sync::Arc<str> = serde_json::to_string_pretty(input)
            .unwrap_or_default()
            .into();
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
        if name == "extension_skill" {
            target = humanize_skill(text(input, "id"));
        }
        if name == "extension_component" {
            target = input
                .get("input")
                .and_then(|value| value.get("entry"))
                .and_then(Value::as_str)
                .unwrap_or_else(|| match text(input, "component") {
                    "mcpServers" => "MCP servers",
                    "lspServers" => "Language servers",
                    "hooks" => "Hooks",
                    other => other,
                })
                .to_owned();
        }
        let mut shown = Self {
            verb: verb.replace('_', " "),
            target: short(&one_line(&target), 160),
            question: match name {
                "file_write" | "file_edit" => "Allow this change to your project?",
                "run_command" | "shell_command" => "Allow this command in your project?",
                "agent_delegate" => "Start a helper agent?",
                "extension_skill" => "Run this skill with the current project permissions?",
                "extension_component" => "Run this plugin component in its configured project?",
                _ => "Allow this action?",
            }
            .into(),
            pretty_input,
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
            shown.error = Some(short(error.unwrap_or("The action could not finish."), 500));
        }
        shown.body = match name {
            // A skill's instructions and resources are for the model; the row
            // names the skill and its description is the tooltip.
            "extension_skill" => {
                shown.tooltip = Some(text(output, "description"))
                    .filter(|description| !description.is_empty())
                    .map(|description| short(description, 300));
                ToolBody::Empty
            }
            "extension_resource" => ToolBody::Empty,
            "extension_component" => {
                shown.meta = Some(short(text(input, "operation"), 40));
                if let Some(results) = output.get("results").and_then(Value::as_array) {
                    let rows = results
                        .iter()
                        .take(ROW_LIMIT)
                        .map(|row| {
                            let entry = text(row, "entry");
                            let detail = component_preview(&row["output"], 0);
                            format!("{}: {}", short(entry, 100), short(&detail, 2000))
                        })
                        .collect::<Vec<_>>();
                    ToolBody::Message(short(&rows.join("\n"), 8000))
                } else if result.is_some() {
                    ToolBody::Message("Component status available.".into())
                } else {
                    ToolBody::Empty
                }
            }
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
                // The row is one line: a multi-line script shows its first
                // line; the detail view keeps the whole command.
                shown.target = short(&first_line(&command), 160);
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
            // A helper's final answer is its whole report; the engine bounds it.
            "agent_delegate" => readable(output)
                .map(|text| ToolBody::Markdown(short(text, 64 * 1024)))
                .unwrap_or(ToolBody::Empty),
            "file_write" | "file_edit" | "git_diff" if !text(output, "diff").is_empty() => {
                ToolBody::Diff(short(text(output, "diff"), 12000))
            }
            "image_generate" => ToolBody::Empty,
            // Unknown tools get a short plain summary, never the envelope.
            _ => readable(output)
                .filter(|text| !text.trim().is_empty())
                .map(|text| ToolBody::Message(first_lines(text)))
                .unwrap_or_else(|| {
                    if result.is_some() {
                        ToolBody::Message("Done".into())
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
#[path = "tool_presentation/tests.rs"]
mod tests;
