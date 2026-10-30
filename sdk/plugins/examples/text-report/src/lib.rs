//! A usable extension: read a workspace text file, count its contents and
//! publish a report through the host. No source file is changed.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use xylo_plugin_sdk::Guest;

const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_UNIQUE_WORDS: usize = 4096;

struct TextReport;

impl Guest for TextReport {
    fn run(input: String) -> Result<String, String> {
        report(&NativeHost, &input)
    }
}

xylo_plugin_sdk::export!(TextReport);

/// A local abstraction allows algorithm tests without granting or faking host
/// authority. The installed-component test exercises the actual Wasmtime host.
trait Host {
    fn read(&self, path: &str) -> Result<String, String>;
    fn write(&self, name: &str, text: &str) -> Result<String, String>;
}

struct NativeHost;
impl Host for NativeHost {
    fn read(&self, path: &str) -> Result<String, String> {
        xylo_plugin_sdk::read_text(path)
    }
    fn write(&self, name: &str, text: &str) -> Result<String, String> {
        xylo_plugin_sdk::write_text(name, text)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    path: String,
    output_name: String,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
struct WordCount<'a> {
    word: &'a str,
    occurrences: usize,
}

#[derive(Serialize)]
struct Statistics<'a> {
    bytes: usize,
    characters: usize,
    lines: usize,
    words: usize,
    vocabulary_limited: bool,
    top_words: Vec<WordCount<'a>>,
}

#[derive(Serialize)]
struct ReportReceipt<'a> {
    artifact: String,
    #[serde(flatten)]
    statistics: Statistics<'a>,
}

fn report(host: &impl Host, input: &str) -> Result<String, String> {
    let request: Request = serde_json::from_str(input)
        .map_err(|_| "Enter a JSON object containing path and output_name.".to_owned())?;
    if request.path.is_empty() || request.path.len() > 4096 {
        return Err("Choose a workspace text file.".into());
    }
    if request.output_name.is_empty()
        || request.output_name.len() > 120
        || !request
            .output_name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        return Err("Choose an output name using letters, digits, '-' or '_'.".into());
    }
    let text = host.read(&request.path)?;
    // Host reads are also bounded. Keep this example's smaller workload limit
    // independent from the result budget and a different embedding host.
    if text.len() > MAX_TEXT_BYTES {
        return Err("This example accepts text files up to 16 KiB.".into());
    }
    let mut counts = BTreeMap::<&str, usize>::new();
    let mut words = 0;
    let mut vocabulary_limited = false;
    for word in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        words += 1;
        if let Some(count) = counts.get_mut(word) {
            *count += 1;
        } else if counts.len() < MAX_UNIQUE_WORDS {
            counts.insert(word, 1);
        } else {
            vocabulary_limited = true;
        }
    }
    let mut top_words: Vec<_> = counts
        .into_iter()
        .map(|(word, occurrences)| WordCount { word, occurrences })
        .collect();
    top_words.sort_unstable_by(|a, b| b.occurrences.cmp(&a.occurrences).then(a.word.cmp(b.word)));
    top_words.truncate(10);
    let statistics = Statistics {
        bytes: text.len(),
        characters: text.chars().count(),
        lines: text.lines().count(),
        words,
        vocabulary_limited,
        top_words,
    };
    // JSON preserves arbitrary Unicode/line breaks without allowing a file path
    // or word to change the report structure. No source contents are copied.
    let contents = serde_json::to_string_pretty(&statistics).map_err(|e| e.to_string())?;
    let artifact = host.write(&request.output_name, &contents)?;
    serde_json::to_string(&ReportReceipt {
        artifact,
        statistics,
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fixture {
        text: String,
        writes: RefCell<Vec<String>>,
        denied: bool,
    }
    impl Host for Fixture {
        fn read(&self, _: &str) -> Result<String, String> {
            if self.denied {
                Err("read_workspace capability not granted".into())
            } else {
                Ok(self.text.clone())
            }
        }
        fn write(&self, _: &str, text: &str) -> Result<String, String> {
            self.writes.borrow_mut().push(text.into());
            Ok("/artifacts/report.txt".into())
        }
    }

    #[test]
    fn unicode_counts_and_artifact_are_consistent() {
        let host = Fixture {
            text: "hello\r\n世界 hello\n".into(),
            writes: RefCell::default(),
            denied: false,
        };
        let output: serde_json::Value = serde_json::from_str(
            &report(&host, r#"{"path":"notes.txt","output_name":"report-one"}"#).unwrap(),
        )
        .unwrap();
        assert_eq!(output["characters"], 16);
        assert_eq!(output["bytes"], 20);
        assert_eq!(output["lines"], 2);
        assert_eq!(output["words"], 3);
        assert_eq!(
            output["top_words"][0],
            serde_json::json!({"word":"hello","occurrences":2})
        );
        assert_eq!(output["artifact"], "/artifacts/report.txt");
        let saved: serde_json::Value = serde_json::from_str(&host.writes.borrow()[0]).unwrap();
        assert_eq!(saved["top_words"], output["top_words"]);
    }

    #[test]
    fn denied_read_and_malformed_input_publish_nothing() {
        let host = Fixture {
            text: String::new(),
            writes: RefCell::default(),
            denied: true,
        };
        assert!(
            report(&host, r#"{"path":"notes.txt","output_name":"report-one"}"#)
                .unwrap_err()
                .contains("not granted")
        );
        assert!(
            report(
                &host,
                r#"{"path":"notes.txt","output_name":"report-one","capabilities":["write_artifacts"]}"#
            )
            .is_err()
        );
        assert!(host.writes.borrow().is_empty());
    }

    #[test]
    fn large_text_is_refused_before_any_publication() {
        let host = Fixture {
            text: "x".repeat(MAX_TEXT_BYTES + 1),
            writes: RefCell::default(),
            denied: false,
        };
        assert!(
            report(&host, r#"{"path":"notes.txt","output_name":"report-one"}"#)
                .unwrap_err()
                .contains("16 KiB")
        );
        assert!(host.writes.borrow().is_empty());
    }
}
