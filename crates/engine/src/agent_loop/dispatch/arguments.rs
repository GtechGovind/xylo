//! Lenient parsing, schema-guided repair and JSON Schema validation of tool
//! arguments, before any permission decision.
//!
//! Invariants:
//! - Strict JSON is always tried first; the lenient fallbacks (code fences,
//!   surrounding prose, trailing commas, single quotes, bare keys, Python
//!   literals, double encoding) only run on text that does not parse.
//! - Repairs are deterministic and only make a value conform where its
//!   meaning is unambiguous under the schema: `"5"` for an integer,
//!   `"true"` for a boolean, a JSON string where an object or array belongs,
//!   `5.0` for an integer, a number where a string belongs, an enum value in
//!   the wrong case or separator style, `null` for an optional field, a
//!   misspelled field name that folds to exactly one declared name when the
//!   schema forbids extra fields, a missing required field that declares a
//!   default, and a top-level `arguments`/`input`/`parameters` wrapper the
//!   schema does not declare. Repairing the same input twice gives the same
//!   result, so a resumed call reproduces the journaled arguments.
//! - Validation never resolves remote references; a schema that does not
//!   compile (a third-party server's mistake) is not enforced here, and the
//!   tool or server reports its own error.
use crate::tools::{catalog, lookup::CompiledSchema};
use serde_json::{Map, Number, Value};

/// Most schema problems listed in one error.
const MAX_PROBLEMS: usize = 5;
/// Longest single problem line; long offending values are cut.
const PROBLEM_CHARS: usize = 300;
/// Deepest schema nesting repair descends into.
const MAX_DEPTH: usize = 24;
/// Top-level keys some models wrap all arguments in.
const WRAPPERS: [&str; 4] = ["arguments", "input", "parameters", "args"];

/// Parses a model's raw argument text into a JSON object.
///
/// Empty text and `null` mean "no arguments". On failure the error names
/// what was wrong in model-facing words.
pub(super) fn parse(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    let strict = serde_json::from_str::<Value>(trimmed);
    let parsed = match &strict {
        Ok(value) => Some(value.clone()),
        Err(_) => lenient(trimmed),
    };
    let Some(mut value) = parsed else {
        let error = strict
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        return Err(format!(
            "are not valid JSON ({error}). Send the call again with a JSON object of named arguments"
        ));
    };
    if let Value::String(inner) = &value
        && let Some(decoded) = serde_json::from_str::<Value>(inner.trim())
            .ok()
            .or_else(|| lenient(inner.trim()))
    {
        value = decoded;
    }
    match value {
        Value::Object(_) => Ok(value),
        Value::Null => Ok(Value::Object(Map::new())),
        _ => Err(
            "must be a JSON object. Send the call again with an object of named arguments".into(),
        ),
    }
}

/// The lenient fallbacks, in order of how little they change.
fn lenient(text: &str) -> Option<Value> {
    let unfenced = unfence(text);
    let object = first_object(unfenced).unwrap_or(unfenced);
    [unfenced, object]
        .into_iter()
        .find_map(|candidate| serde_json::from_str(candidate).ok())
        .or_else(|| serde_json::from_str(&relax(object)).ok())
}

/// The body of a Markdown code fence, or `text` itself.
fn unfence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let body = rest.split_once('\n').map_or("", |(_, body)| body);
    body.trim_end().strip_suffix("```").unwrap_or(body).trim()
}

/// The first balanced `{…}` in `text`, ignoring braces inside strings.
fn first_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    for (offset, character) in text[start..].char_indices() {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == open {
                quote = None;
            }
            continue;
        }
        match character {
            '"' | '\'' => quote = Some(character),
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..start + offset + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Rewrites near-JSON into JSON: single-quoted strings become double-quoted,
/// trailing commas go, bare object keys are quoted and Python's `True`,
/// `False` and `None` become JSON literals.
fn relax(text: &str) -> String {
    let characters: Vec<char> = text.chars().collect();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        match character {
            '"' | '\'' => {
                index = copy_string(&characters, index, &mut output);
                continue;
            }
            ',' => {
                let next = characters[index + 1..]
                    .iter()
                    .find(|next| !next.is_whitespace());
                if !matches!(next, Some('}' | ']')) {
                    output.push(',');
                }
            }
            character if character.is_alphabetic() || character == '_' => {
                let end = characters[index..]
                    .iter()
                    .position(|next| !(next.is_alphanumeric() || *next == '_'))
                    .map_or(characters.len(), |length| index + length);
                let word: String = characters[index..end].iter().collect();
                let key = characters[end..]
                    .iter()
                    .find(|next| !next.is_whitespace())
                    .is_some_and(|next| *next == ':');
                match (key, word.as_str()) {
                    (true, _) => output.push_str(&format!("\"{word}\"")),
                    (false, "True") => output.push_str("true"),
                    (false, "False") => output.push_str("false"),
                    (false, "None") => output.push_str("null"),
                    (false, _) => output.push_str(&word),
                }
                index = end;
                continue;
            }
            character => output.push(character),
        }
        index += 1;
    }
    output
}

/// Copies the string literal starting at `start` as a double-quoted JSON
/// string and returns the index after it.
fn copy_string(characters: &[char], start: usize, output: &mut String) -> usize {
    let open = characters[start];
    output.push('"');
    let mut index = start + 1;
    while index < characters.len() {
        let character = characters[index];
        if character == '\\' && index + 1 < characters.len() {
            let next = characters[index + 1];
            if open == '\'' && next == '\'' {
                output.push('\'');
            } else {
                output.push('\\');
                output.push(next);
            }
            index += 2;
            continue;
        }
        if character == open {
            output.push('"');
            return index + 1;
        }
        if character == '"' {
            output.push_str("\\\"");
        } else {
            output.push(character);
        }
        index += 1;
    }
    index
}

/// Makes `arguments` conform to `schema` where the intended value is
/// unambiguous. See the module documentation for the exact repairs.
pub(super) fn repair(arguments: &mut Value, schema: &Value) {
    unwrap_wrapper(arguments, schema);
    conform(arguments, schema, 0);
}

/// `{"arguments": {…}}` for a tool that declares no such field and allows no
/// extra fields is the whole call wrapped once.
fn unwrap_wrapper(arguments: &mut Value, schema: &Value) {
    let Some(map) = arguments.as_object() else {
        return;
    };
    let declared = |key: &str| {
        schema
            .get("properties")
            .and_then(Value::as_object)
            .is_some_and(|properties| properties.contains_key(key))
    };
    if map.len() != 1 || !closed(schema) {
        return;
    }
    let Some((key, inner)) = map.iter().next() else {
        return;
    };
    if WRAPPERS.contains(&key.as_str()) && !declared(key) && inner.is_object() {
        *arguments = inner.clone();
    }
}

/// Whether `schema` forbids properties it does not declare.
fn closed(schema: &Value) -> bool {
    schema.get("additionalProperties") == Some(&Value::Bool(false))
}

/// The JSON types `schema` admits; empty when it does not say.
fn types(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(kind)) => vec![kind.as_str()],
        Some(Value::Array(kinds)) => kinds.iter().filter_map(Value::as_str).collect(),
        _ if schema.get("properties").is_some() => vec!["object"],
        _ if schema.get("items").is_some() => vec!["array"],
        _ => Vec::new(),
    }
}

fn admits(value: &Value, kinds: &[&str]) -> bool {
    kinds.iter().any(|kind| match *kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        _ => false,
    })
}

fn conform(value: &mut Value, schema: &Value, depth: usize) {
    if depth > MAX_DEPTH || !schema.is_object() {
        return;
    }
    let kinds = types(schema);
    if !kinds.is_empty()
        && !admits(value, &kinds)
        && let Some(converted) = convert(value, &kinds)
    {
        *value = converted;
    }
    conform_enum(value, schema);
    match value {
        Value::Object(map) => conform_object(map, schema, depth),
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items").filter(|items| items.is_object()) {
                for item in items {
                    conform(item, item_schema, depth + 1);
                }
            }
        }
        _ => {}
    }
}

/// A value of an admitted type that `value` unambiguously stands for.
fn convert(value: &Value, kinds: &[&str]) -> Option<Value> {
    match value {
        Value::String(text) => {
            let text = text.trim();
            if (kinds.contains(&"object") || kinds.contains(&"array"))
                && let Ok(parsed) = serde_json::from_str::<Value>(text)
                && admits(&parsed, kinds)
            {
                return Some(parsed);
            }
            if kinds.contains(&"integer") {
                if let Ok(integer) = text.parse::<i64>() {
                    return Some(Value::from(integer));
                }
                if let Ok(float) = text.parse::<f64>()
                    && float.fract() == 0.0
                    && float.abs() < 9.0e15
                {
                    return Some(Value::from(float as i64));
                }
            }
            if kinds.contains(&"number")
                && let Ok(float) = text.parse::<f64>()
            {
                return Number::from_f64(float).map(Value::Number);
            }
            if kinds.contains(&"boolean") {
                match text.to_ascii_lowercase().as_str() {
                    "true" => return Some(Value::Bool(true)),
                    "false" => return Some(Value::Bool(false)),
                    _ => {}
                }
            }
            None
        }
        Value::Number(number) => {
            if kinds.contains(&"integer")
                && let Some(float) = number.as_f64()
                && float.fract() == 0.0
                && float.abs() < 9.0e15
            {
                return Some(Value::from(float as i64));
            }
            kinds
                .contains(&"string")
                .then(|| Value::String(number.to_string()))
        }
        Value::Bool(flag) => kinds
            .contains(&"string")
            .then(|| Value::String(flag.to_string())),
        _ => None,
    }
}

/// An enum value written in another case or separator style.
fn conform_enum(value: &mut Value, schema: &Value) {
    let (Some(options), Value::String(text)) =
        (schema.get("enum").and_then(Value::as_array), &*value)
    else {
        return;
    };
    if options.contains(value) {
        return;
    }
    let wanted = catalog::fold(text);
    let mut matching = options.iter().filter(|option| {
        option
            .as_str()
            .is_some_and(|option| catalog::fold(option) == wanted)
    });
    if let (Some(option), None) = (matching.next(), matching.next()) {
        *value = option.clone();
    }
}

fn conform_object(map: &mut Map<String, Value>, schema: &Value, depth: usize) {
    let empty = Map::new();
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if closed(schema) {
        rename_unknown(map, properties);
    }
    map.retain(|key, value| {
        let optional_null = value.is_null()
            && !required.contains(&key.as_str())
            && properties
                .get(key)
                .is_some_and(|property| !types(property).contains(&"null"));
        !optional_null
    });
    for key in &required {
        if !map.contains_key(*key)
            && let Some(default) = properties
                .get(*key)
                .and_then(|property| property.get("default"))
        {
            map.insert((*key).to_owned(), default.clone());
        }
    }
    let extra = schema
        .get("additionalProperties")
        .filter(|extra| extra.is_object());
    for (key, value) in map.iter_mut() {
        if let Some(property) = properties.get(key).or(extra) {
            conform(value, property, depth + 1);
        }
    }
}

/// Renames each undeclared key that folds to exactly one declared, absent
/// key (`oldString` → `old_string`).
fn rename_unknown(map: &mut Map<String, Value>, properties: &Map<String, Value>) {
    let unknown: Vec<String> = map
        .keys()
        .filter(|key| !properties.contains_key(*key))
        .cloned()
        .collect();
    for key in unknown {
        let folded = catalog::fold(&key);
        let mut matching = properties
            .keys()
            .filter(|declared| catalog::fold(declared) == folded && !map.contains_key(*declared));
        if let (Some(declared), None) = (matching.next(), matching.next()) {
            let declared = declared.clone();
            if let Some(value) = map.remove(&key) {
                map.insert(declared, value);
            }
        }
    }
}

/// Validates `arguments` against `schema`. `Err` lists what is wrong, one
/// actionable line per problem: where, what was expected and, for unknown
/// fields, the closest declared names.
pub(super) fn validate(arguments: &Value, schema: &CompiledSchema) -> Result<(), Vec<String>> {
    let Some(validator) = schema.validator() else {
        return Ok(());
    };
    let problems: Vec<String> = validator
        .iter_errors(arguments)
        .take(MAX_PROBLEMS)
        .map(|error| describe(&error, schema.schema()))
        .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

fn describe(error: &jsonschema::ValidationError, schema: &Value) -> String {
    let path = error.instance_path().as_str().trim_start_matches('/');
    let place = if path.is_empty() {
        String::new()
    } else {
        format!("`{path}`: ")
    };
    let line = match error.kind() {
        jsonschema::error::ValidationErrorKind::AdditionalProperties { unexpected } => {
            let declared = error
                .schema_path()
                .as_str()
                .rsplit_once('/')
                .and_then(|(parent, _)| schema.pointer(parent))
                .and_then(|parent| parent.get("properties"))
                .and_then(Value::as_object);
            unexpected
                .iter()
                .map(|field| {
                    let suggestions = declared
                        .map(|declared| {
                            catalog::nearest(field, declared.keys().map(String::as_str))
                        })
                        .unwrap_or_default();
                    match suggestions.as_slice() {
                        [] => format!("{place}unknown field `{field}`"),
                        names => format!(
                            "{place}unknown field `{field}` (did you mean {}?)",
                            quoted(names)
                        ),
                    }
                })
                .collect::<Vec<_>>()
                .join("; ")
        }
        _ => format!("{place}{error}"),
    };
    cut(&line, PROBLEM_CHARS)
}

/// `a`, `b` or `c`.
pub(super) fn quoted(names: &[&str]) -> String {
    let names: Vec<String> = names.iter().map(|name| format!("`{name}`")).collect();
    match names.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => names.concat(),
    }
}

fn cut(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().take(limit.saturating_sub(1)).collect();
    format!("{kept}…")
}
