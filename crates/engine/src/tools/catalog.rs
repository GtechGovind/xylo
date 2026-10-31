//! Lexical tool discovery: a BM25 [`Index`] for `search_tools`, nearest-name
//! suggestions for unknown tools and arguments, and compact parameter
//! signatures for model-facing messages.
//!
//! Invariants:
//! - Ranking is deterministic: equal scores keep catalog order, and the broker
//!   lists built-in tools before connector tools.
//! - Connector names and descriptions come from third-party servers, so they
//!   are untrusted ranking input. Their descriptions are cut to
//!   [`UNTRUSTED_DESCRIPTION_TERMS`] terms (so keyword stuffing cannot buy
//!   rank), their scores are scaled by [`UNTRUSTED_WEIGHT`], and a connector
//!   tool with a built-in tool's name never ranks above that tool.
//! - Nothing here touches the network or the store; callers build the entries.
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

/// BM25 term-frequency saturation.
const K1: f64 = 1.2;
/// BM25 length normalization.
const B: f64 = 0.75;
/// How often a tool's name terms repeat in its document: names say most
/// about what a tool does.
const NAME_WEIGHT: usize = 3;
/// How often a connector's own name repeats in each of its tools' documents.
const SERVER_WEIGHT: usize = 2;
/// Description terms of a third-party tool that count for ranking.
pub(crate) const UNTRUSTED_DESCRIPTION_TERMS: usize = 48;
/// Score factor for third-party tools.
pub(crate) const UNTRUSTED_WEIGHT: f64 = 0.8;
/// Longest description shown in a search result.
const SHORT_DESCRIPTION_CHARS: usize = 200;
/// Longest parameter signature in a model-facing message.
const SIGNATURE_CHARS: usize = 600;

/// Words that carry no meaning for tool selection.
const STOP_WORDS: [&str; 24] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "into", "is", "it", "of",
    "on", "or", "the", "this", "to", "with", "you", "your", "via",
];

/// One searchable tool.
pub(crate) struct Entry<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub parameters: &'a Value,
    /// The connector's name for a connector tool; `None` for built-in tools.
    pub server: Option<&'a str>,
}

/// Lowercase terms of `text`: split at non-alphanumerics and camelCase
/// boundaries, stop words dropped, plurals folded (`files` → `file`).
pub(crate) fn terms(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut previous_lower = false;
    for character in text.chars() {
        if !character.is_alphanumeric() {
            flush(&mut current, &mut words);
            previous_lower = false;
            continue;
        }
        if character.is_uppercase() && previous_lower {
            flush(&mut current, &mut words);
        }
        previous_lower = character.is_lowercase() || character.is_numeric();
        current.extend(character.to_lowercase());
    }
    flush(&mut current, &mut words);
    words
}

fn flush(current: &mut String, words: &mut Vec<String>) {
    if current.is_empty() {
        return;
    }
    let word = std::mem::take(current);
    if !STOP_WORDS.contains(&word.as_str()) {
        words.push(stem(word));
    }
}

fn stem(word: String) -> String {
    let length = word.chars().count();
    if length > 4 && word.ends_with("ies") {
        format!("{}y", &word[..word.len() - 3])
    } else if length > 3 && word.ends_with('s') && !word.ends_with("ss") {
        word[..word.len() - 1].to_owned()
    } else {
        word
    }
}

/// The weighted term document of one tool.
fn document(entry: &Entry) -> Vec<String> {
    let mut document = Vec::new();
    let name = terms(entry.name);
    for _ in 0..NAME_WEIGHT {
        document.extend(name.iter().cloned());
    }
    if let Some(server) = entry.server {
        let server = terms(server);
        for _ in 0..SERVER_WEIGHT {
            document.extend(server.iter().cloned());
        }
    }
    let description = terms(entry.description);
    let limit = if entry.server.is_some() {
        UNTRUSTED_DESCRIPTION_TERMS
    } else {
        description.len()
    };
    document.extend(description.into_iter().take(limit));
    document.extend(parameter_names(entry.parameters).flat_map(terms));
    document
}

fn parameter_names(schema: &Value) -> impl Iterator<Item = &str> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(Map::keys)
        .map(String::as_str)
}

/// A BM25 index over a fixed list of tools, built once and queried many
/// times. Ranking is identical to building it afresh for every query.
pub(crate) struct Index {
    documents: Vec<Document>,
    /// How many documents contain each term.
    frequency: HashMap<String, usize>,
    /// Average document length in terms.
    average: f64,
}

/// One tool's indexed form.
struct Document {
    counts: HashMap<String, usize>,
    length: usize,
    /// The tool's name, lowercase, for exact-name queries.
    name: String,
    /// The tool's name words, sorted and unique, for shadowing.
    name_terms: Vec<String>,
    connector: bool,
}

impl Index {
    /// Indexes `entries`; ranks refer to their positions.
    pub(crate) fn new(entries: &[Entry]) -> Self {
        let documents: Vec<Document> = entries
            .iter()
            .map(|entry| {
                let words = document(entry);
                let mut counts: HashMap<String, usize> = HashMap::new();
                for word in &words {
                    *counts.entry(word.clone()).or_default() += 1;
                }
                let mut name_terms = terms(entry.name);
                name_terms.sort();
                name_terms.dedup();
                Document {
                    counts,
                    length: words.len(),
                    name: entry.name.to_lowercase(),
                    name_terms,
                    connector: entry.server.is_some(),
                }
            })
            .collect();
        let mut frequency: HashMap<String, usize> = HashMap::new();
        for document in &documents {
            for term in document.counts.keys() {
                *frequency.entry(term.clone()).or_default() += 1;
            }
        }
        let average = documents
            .iter()
            .map(|document| document.length)
            .sum::<usize>() as f64
            / (documents.len() as f64).max(1.0);
        Self {
            documents,
            frequency,
            average,
        }
    }

    /// The indexes of the best `limit` matches for `query`, best first. A
    /// query word equal to a tool's exact name puts that tool first. Tools
    /// that share no term with the query are never returned.
    pub(crate) fn rank(&self, query: &str, limit: usize) -> Vec<usize> {
        let wanted: Vec<String> = {
            let mut seen = HashSet::new();
            terms(query)
                .into_iter()
                .filter(|term| seen.insert(term.clone()))
                .collect()
        };
        let exact: HashSet<String> = query
            .split(|character: char| character.is_whitespace() || character == ',')
            .map(|word| word.trim_matches(|c: char| c == '`' || c == '"' || c == '\''))
            .filter(|word| !word.is_empty())
            .map(str::to_lowercase)
            .collect();
        let count = self.documents.len() as f64;
        let mut scored: Vec<(usize, f64)> = self
            .documents
            .iter()
            .enumerate()
            .filter_map(|(index, document)| {
                let length = document.length as f64;
                let mut score = 0.0;
                for term in &wanted {
                    let Some(&occurrences) = document.counts.get(term) else {
                        continue;
                    };
                    let occurrences = occurrences as f64;
                    let containing = self.frequency.get(term).copied().unwrap_or(0) as f64;
                    let idf = (1.0 + (count - containing + 0.5) / (containing + 0.5)).ln();
                    score += idf * occurrences * (K1 + 1.0)
                        / (occurrences + K1 * (1.0 - B + B * length / self.average.max(1.0)));
                }
                if score <= 0.0 {
                    return None;
                }
                if document.connector {
                    score *= UNTRUSTED_WEIGHT;
                }
                if exact.contains(&document.name) {
                    score += 1_000.0;
                }
                Some((index, score))
            })
            .collect();
        self.shadow(&mut scored);
        scored.sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
        scored
            .into_iter()
            .take(limit)
            .map(|(index, _)| index)
            .collect()
    }

    /// Keeps each connector tool that shadows a built-in tool (the same name
    /// words, as with `files__file_read` and `file_read`) just below that
    /// built-in tool, so a third-party server cannot take over a capability
    /// Xylo already has by matching the query better.
    fn shadow(&self, scored: &mut [(usize, f64)]) {
        let builtin: Vec<(&[String], f64)> = scored
            .iter()
            .filter(|(index, _)| !self.documents[*index].connector)
            .map(|(index, score)| (self.documents[*index].name_terms.as_slice(), *score))
            .collect();
        for (index, score) in scored.iter_mut() {
            let document = &self.documents[*index];
            if !document.connector {
                continue;
            }
            if let Some((_, ceiling)) = builtin
                .iter()
                .find(|(words, _)| *words == document.name_terms.as_slice())
            {
                *score = score.min(ceiling * 0.99);
            }
        }
    }
}

/// Up to three `candidates` that `wanted` most plausibly meant, best first:
/// spelling variants (`oldString` for `old_string`, and then only those),
/// reordered or shared words (`read_file` for `file_read`) and small typos.
pub(crate) fn nearest<'a>(
    wanted: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Vec<&'a str> {
    let folded = fold(wanted);
    let wanted_terms: HashSet<String> = terms(wanted).into_iter().collect();
    let tolerance = (folded.chars().count() / 3).max(2);
    let mut scored: Vec<(&str, bool, usize, usize)> = candidates
        .into_iter()
        .filter(|candidate| *candidate != wanted)
        .filter_map(|candidate| {
            let same = fold(candidate) == folded;
            let shared = terms(candidate)
                .into_iter()
                .collect::<HashSet<_>>()
                .intersection(&wanted_terms)
                .count();
            let distance = levenshtein(&folded, &fold(candidate));
            (same || shared > 0 || distance <= tolerance)
                .then_some((candidate, same, shared, distance))
        })
        .collect();
    scored.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then(right.2.cmp(&left.2))
            .then(left.3.cmp(&right.3))
            .then(left.0.cmp(right.0))
    });
    scored.dedup_by(|left, right| left.0 == right.0);
    // A spelling variant is the answer; weaker matches would only distract.
    if scored.first().is_some_and(|best| best.1) {
        scored.retain(|candidate| candidate.1);
    }
    scored.into_iter().take(3).map(|entry| entry.0).collect()
}

/// `wanted` with case and separators removed, so `oldString`, `old-string`
/// and `old_string` compare equal.
pub(crate) fn fold(wanted: &str) -> String {
    wanted
        .chars()
        .filter(|character| character.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn levenshtein(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (row, left) in left.chars().enumerate() {
        let mut current = vec![row + 1; right.len() + 1];
        for (column, right) in right.iter().enumerate() {
            let substitution = previous[column] + usize::from(left != *right);
            current[column + 1] = substitution
                .min(previous[column + 1] + 1)
                .min(current[column] + 1);
        }
        previous = current;
    }
    previous[right.len()]
}

/// `name: type` for each top-level parameter, required ones marked, enum
/// values listed: what a model needs to fix a call without the full schema.
pub(crate) fn signature(schema: &Value) -> String {
    let required: HashSet<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return "an object of named arguments".into();
    };
    if properties.is_empty() {
        return "no arguments ({})".into();
    }
    let parts: Vec<String> = properties
        .iter()
        .map(|(name, property)| {
            let mut part = format!("{name}: {}", describe_type(property));
            if required.contains(name.as_str()) {
                part.push_str(" (required)");
            }
            part
        })
        .collect();
    shorten(&parts.join("; "), SIGNATURE_CHARS)
}

fn describe_type(property: &Value) -> String {
    if let Some(options) = property.get("enum").and_then(Value::as_array) {
        let options: Vec<String> = options
            .iter()
            .map(|option| match option {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            })
            .collect();
        return format!("one of {}", options.join(", "));
    }
    let kind = match property.get("type") {
        Some(Value::String(kind)) => kind.clone(),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        _ if property.get("properties").is_some() => "object".into(),
        _ => "any".into(),
    };
    match (kind.as_str(), property.get("items")) {
        ("array", Some(items)) => format!("array of {}", describe_type(items)),
        _ => kind,
    }
}

/// The first sentence of `description`, at most [`SHORT_DESCRIPTION_CHARS`].
pub(crate) fn short_description(description: &str) -> String {
    let sentence = description
        .split_once(". ")
        .map_or(description, |(first, _)| first);
    shorten(sentence.trim(), SHORT_DESCRIPTION_CHARS)
}

fn shorten(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let cut: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests;
