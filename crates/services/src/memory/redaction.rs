//! Removes credentials from text before it is saved as a project fact.
//!
//! Facts outlive the conversation that wrote them and are recalled into
//! later ones, so a secret a model copied from a file or a command's output
//! must never reach the memory tables. Redaction is conservative about what
//! it keeps: it replaces private-key blocks, values with well-known credential
//! prefixes, JSON Web Tokens, `Bearer`/`Basic` credentials, passwords in URLs,
//! and values assigned to credential-named keys (`password=…`,
//! `api_key: …`). It is idempotent: redacting twice gives the same text.

/// What replaces each secret.
pub(crate) const REDACTED: &str = "[REDACTED]";

/// Prefixes of provider tokens that are secrets whatever their context.
const TOKEN_PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xapp-",
    "hf_",
    "npm_",
    "AKIA",
    "ASIA",
    "AIza",
];

/// Shortest token after a known prefix that is treated as a secret, so
/// words such as `sk-learn` survive.
const TOKEN_BODY_CHARS: usize = 16;

/// Key names whose assigned value is a secret.
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "accesskey",
    "access_key",
    "privatekey",
    "private_key",
    "credential",
    "auth",
];

/// `text` with every recognised credential replaced by [`REDACTED`].
pub(crate) fn redact_secrets(text: &str) -> String {
    let text = redact_private_keys(text);
    let mut output = String::with_capacity(text.len());
    let mut pending = None;
    // After a MySQL client, `-p` introduces a password.
    let mut mysql = false;
    for (word, space) in words(&text) {
        if space {
            output.push_str(word);
            continue;
        }
        let (redacted, next) = redact_word(word, pending, mysql);
        output.push_str(&redacted);
        pending = next;
        mysql |= MYSQL_CLIENTS.contains(&program_name(word));
    }
    output
}

/// Programs whose `-p` option takes a password.
const MYSQL_CLIENTS: &[&str] = &["mysql", "mysqldump", "mysqladmin", "mariadb"];

fn program_name(word: &str) -> &str {
    word.rsplit(['/', '\\']).next().unwrap_or(word)
}

/// What the previous word announced about the next one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pending {
    /// A bare credential key: a following `=` makes the next word a value.
    Key,
    /// The next word is the value of a credential key or scheme.
    Value(Separator),
}

/// How a credential key was joined to its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Separator {
    /// `key=value`: any non-trivial value.
    Assign,
    /// `key: value` or `Bearer value`: only values that look like secrets,
    /// so prose such as "token: expires hourly" survives.
    Colon,
}

/// Splits `text` into alternating runs of whitespace and non-whitespace,
/// flagging the whitespace runs.
fn words(text: &str) -> impl Iterator<Item = (&str, bool)> {
    let mut rest = text;
    std::iter::from_fn(move || {
        let first = rest.chars().next()?;
        let space = first.is_whitespace();
        let end = rest
            .find(|c: char| c.is_whitespace() != space)
            .unwrap_or(rest.len());
        let (word, tail) = rest.split_at(end);
        rest = tail;
        Some((word, space))
    })
}

/// Characters that wrap a value without being part of it.
fn wrapper(c: char) -> bool {
    matches!(
        c,
        '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';'
    )
}

fn redact_word(word: &str, pending: Option<Pending>, mysql: bool) -> (String, Option<Pending>) {
    let start = word.len() - word.trim_start_matches(wrapper).len();
    // Sentence punctuation after a value is not part of it.
    let end = word
        .trim_end_matches(|c| wrapper(c) || matches!(c, '.' | '!' | '?'))
        .len()
        .max(start);
    let (prefix, core, suffix) = (&word[..start], &word[start..end], &word[end..]);
    let redacted = || format!("{prefix}{REDACTED}{suffix}");
    if core.is_empty() || core.contains("REDACTED") {
        return (word.to_owned(), None);
    }
    match pending {
        Some(Pending::Value(separator)) if value_is_secret(core, separator) => {
            return (redacted(), None);
        }
        Some(Pending::Key) if core == "=" => {
            return (word.to_owned(), Some(Pending::Value(Separator::Assign)));
        }
        _ => {}
    }
    if core.eq_ignore_ascii_case("bearer") || core.eq_ignore_ascii_case("basic") {
        return (word.to_owned(), Some(Pending::Value(Separator::Colon)));
    }
    if token(core) {
        return (redacted(), None);
    }
    if let Some(next) = flag(core, mysql) {
        return match next {
            Flag::Announces => (word.to_owned(), Some(Pending::Value(Separator::Assign))),
            Flag::Attached(kept) => (format!("{prefix}{kept}{REDACTED}{suffix}"), None),
        };
    }
    let credentials = url_credentials(core);
    let checked = credentials.as_deref().unwrap_or(core);
    let parameters = parameters(checked);
    if let Some(kept) = parameters.or(credentials) {
        return (format!("{prefix}{kept}{suffix}"), None);
    }
    if let Some((key, separator, raw)) = assignment(core)
        && secret_key(key)
    {
        let value = raw.trim_matches(wrapper);
        if value.is_empty() {
            return (word.to_owned(), Some(Pending::Value(separator)));
        }
        if !value_is_secret(value, separator) {
            return (word.to_owned(), None);
        }
        let lead = raw.len() - raw.trim_start_matches(wrapper).len();
        let kept = &core[..key.len() + 1 + lead];
        return (format!("{prefix}{kept}{REDACTED}{suffix}"), None);
    }
    (word.to_owned(), secret_key(core).then_some(Pending::Key))
}

/// `key=value` or `key:value` (not a URL scheme), split at the first
/// separator.
fn assignment(core: &str) -> Option<(&str, Separator, &str)> {
    let at = core.find(['=', ':'])?;
    if core[at..].starts_with("://") {
        return None;
    }
    let separator = if core.as_bytes()[at] == b'=' {
        Separator::Assign
    } else {
        Separator::Colon
    };
    Some((&core[..at], separator, &core[at + 1..]))
}

fn secret_key(key: &str) -> bool {
    let key = key.trim_matches(wrapper).to_ascii_lowercase();
    !key.is_empty() && key.len() <= 64 && SECRET_KEYS.iter().any(|secret| key.ends_with(secret))
}

fn value_is_secret(value: &str, separator: Separator) -> bool {
    let value = value.trim_matches(wrapper);
    match separator {
        Separator::Assign => value.chars().count() >= 4,
        Separator::Colon => {
            value.chars().count() >= 8
                && value
                    .chars()
                    .any(|c| c.is_ascii_digit() || !c.is_alphanumeric())
        }
    }
}

/// A value with a well-known credential prefix, or a JSON Web Token.
fn token(core: &str) -> bool {
    let body_is_token = |body: &str| {
        body.chars().count() >= TOKEN_BODY_CHARS
            && body
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '+'))
    };
    if TOKEN_PREFIXES
        .iter()
        .any(|prefix| core.strip_prefix(prefix).is_some_and(body_is_token))
    {
        return true;
    }
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && core.starts_with("eyJ")
        && parts.iter().all(|part| {
            part.len() >= 4
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '='))
        })
}

/// `scheme://user:password@host…` with the password redacted, or
/// `scheme://token@host…` with a credential-shaped user name redacted.
fn url_credentials(core: &str) -> Option<String> {
    let scheme_end = core.find("://")? + 3;
    let authority_end = core[scheme_end..]
        .find(['/', '?', '#'])
        .map_or(core.len(), |end| scheme_end + end);
    let at = core[scheme_end..authority_end].rfind('@')? + scheme_end;
    match core[scheme_end..at].find(':') {
        Some(colon) => {
            let colon = colon + scheme_end;
            (colon + 1 < at).then(|| format!("{}{REDACTED}{}", &core[..=colon], &core[at..]))
        }
        None => token(&core[scheme_end..at])
            .then(|| format!("{}{REDACTED}{}", &core[..scheme_end], &core[at..])),
    }
}

/// `core` with the values of credential parameters redacted, for query
/// strings (`?api_key=…&…`) and `;`-separated connection strings
/// (`Server=…;Password=…`); `None` when there is nothing to redact.
fn parameters(core: &str) -> Option<String> {
    if !core.contains(['?', '&', ';']) {
        return None;
    }
    let mut output = String::with_capacity(core.len());
    let mut changed = false;
    let mut rest = core;
    loop {
        let end = rest.find(['?', '&', ';']).unwrap_or(rest.len());
        let (piece, tail) = rest.split_at(end);
        match piece.split_once('=') {
            Some((key, value))
                if secret_key(key)
                    && !value.contains("REDACTED")
                    && value_is_secret(value, Separator::Assign) =>
            {
                output.push_str(key);
                output.push('=');
                output.push_str(REDACTED);
                changed = true;
            }
            _ => output.push_str(piece),
        }
        let Some(separator) = tail.chars().next() else {
            break;
        };
        output.push(separator);
        rest = &tail[separator.len_utf8()..];
    }
    changed.then_some(output)
}

/// What a command-line option says about a secret.
enum Flag<'a> {
    /// `--password`, `--api-key`, `-p` after a MySQL client: the next word
    /// is the value.
    Announces,
    /// `-psecret` after a MySQL client: the value follows this prefix.
    Attached(&'a str),
}

fn flag(core: &str, mysql: bool) -> Option<Flag<'_>> {
    if core.contains('=') {
        return None;
    }
    if let Some(name) = core.strip_prefix("--") {
        return secret_key(name).then_some(Flag::Announces);
    }
    if !mysql {
        return None;
    }
    match core.strip_prefix("-p")? {
        "" => Some(Flag::Announces),
        _ => Some(Flag::Attached("-p")),
    }
}

/// Replaces each PEM private-key block, through its END line when present
/// and to the end of the text otherwise.
fn redact_private_keys(text: &str) -> String {
    const BEGIN: &str = "-----BEGIN ";
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let header_end = rest[start + BEGIN.len()..]
            .find("-----")
            .map(|end| start + BEGIN.len() + end);
        let private = header_end.is_some_and(|end| rest[start..end].contains("PRIVATE KEY"));
        let Some(header_end) = header_end.filter(|_| private) else {
            output.push_str(&rest[..start + BEGIN.len()]);
            rest = &rest[start + BEGIN.len()..];
            continue;
        };
        output.push_str(&rest[..start]);
        output.push_str(REDACTED);
        let after = &rest[header_end + 5..];
        rest = match after.find("-----END ") {
            Some(end) => {
                let tail = &after[end + "-----END ".len()..];
                tail.find("-----").map_or("", |close| &tail[close + 5..])
            }
            None => "",
        };
    }
    output.push_str(rest);
    output
}

#[cfg(test)]
mod tests;
