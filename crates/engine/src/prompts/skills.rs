//! Built-in skills in the Agent Skills format and the skills index.
//!
//! Each skill is a folder `crates/engine/skills/<name>/` with a `SKILL.md`
//! (YAML-style frontmatter `name`, `description`, optional `allowed-tools`,
//! then Markdown instructions) and optional resource files. Everything is
//! compiled in, but only `name: description` lines reach the system prompt
//! (see [`skills_index`]); a body is returned only when the model invokes
//! the skill, and a resource only when the body sends the model to it.

use std::sync::LazyLock;

/// A supporting file bundled with a built-in skill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BundledResource {
    /// Path relative to the skill folder, as the skill body refers to it.
    pub path: &'static str,
    /// File contents.
    pub content: &'static str,
}

/// A built-in skill compiled into the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledSkill {
    /// Unique name; equals the skill's folder name.
    pub name: &'static str,
    /// What the skill does and when to use it; the only text pre-loaded.
    pub description: &'static str,
    /// Tools the skill expects to use. Informational: invoking a skill never
    /// grants permissions.
    pub allowed_tools: Vec<&'static str>,
    /// Markdown instructions after the frontmatter.
    pub body: &'static str,
    /// Supporting files, loaded on demand.
    pub resources: &'static [BundledResource],
}

impl BundledSkill {
    /// The resource at `path` (relative to the skill folder), if bundled.
    pub fn resource(&self, path: &str) -> Option<&'static BundledResource> {
        self.resources.iter().find(|resource| resource.path == path)
    }
}

pub(super) struct RawSkill {
    pub(super) folder: &'static str,
    pub(super) source: &'static str,
    pub(super) resources: &'static [BundledResource],
}

macro_rules! skill {
    ($name:literal $(, $resource:literal)* $(,)?) => {
        RawSkill {
            folder: $name,
            source: include_str!(concat!("../../skills/", $name, "/SKILL.md")),
            resources: &[$(BundledResource {
                path: $resource,
                content: include_str!(concat!("../../skills/", $name, "/", $resource)),
            }),*],
        }
    };
}

/// Every built-in skill folder with the resource files it ships.
pub(super) const RAW_SKILLS: &[RawSkill] = &[
    skill!("code-review"),
    skill!("create-skill", "references/format.md"),
    skill!("debug-failure"),
    skill!("git-commit"),
    skill!("refactor"),
    skill!("write-tests"),
];

static SKILLS: LazyLock<Vec<BundledSkill>> = LazyLock::new(|| {
    RAW_SKILLS
        .iter()
        .filter_map(|raw| match parse(raw) {
            Ok(skill) => Some(skill),
            Err(error) => {
                tracing::error!(skill = raw.folder, %error, "Invalid built-in skill skipped");
                None
            }
        })
        .collect()
});

/// All built-in skills, in name order.
pub fn bundled_skills() -> &'static [BundledSkill] {
    &SKILLS
}

/// The built-in skill called `name`.
pub fn bundled_skill(name: &str) -> Option<&'static BundledSkill> {
    SKILLS.iter().find(|skill| skill.name == name)
}

/// Longest description kept in an index line while the index fits its budget.
const INDEX_DESCRIPTION_CHARS: usize = 400;
/// Description length used when the full index would exceed its budget.
const INDEX_SHORT_DESCRIPTION_CHARS: usize = 100;
/// Byte budget for the whole skills index in the system prompt.
pub(super) const SKILLS_INDEX_BYTES: usize = 8 * 1024;

/// The compact `- name: description` index of available skills: the only
/// skill content placed in the system prompt. Descriptions are flattened to
/// one line and bounded. When the index exceeds [`SKILLS_INDEX_BYTES`],
/// descriptions are shortened first, then the remaining skills are counted
/// in a final line instead of listed. Empty when there are no skills.
pub fn skills_index<'s>(skills: impl IntoIterator<Item = (&'s str, &'s str)>) -> String {
    let skills: Vec<(&str, &str)> = skills.into_iter().collect();
    let full = index_lines(&skills, INDEX_DESCRIPTION_CHARS);
    let lines = if full.iter().map(|line| line.len() + 1).sum::<usize>() <= SKILLS_INDEX_BYTES {
        full
    } else {
        index_lines(&skills, INDEX_SHORT_DESCRIPTION_CHARS)
    };
    let mut index = String::new();
    for (shown, line) in lines.iter().enumerate() {
        // Reserve room for the omission line.
        if index.len() + line.len() + 1 > SKILLS_INDEX_BYTES - 64 {
            index.push_str(&format!(
                "- ({} more skills not listed)",
                lines.len() - shown
            ));
            return index;
        }
        index.push_str(line);
        index.push('\n');
    }
    index.truncate(index.trim_end().len());
    index
}

fn index_lines(skills: &[(&str, &str)], description_chars: usize) -> Vec<String> {
    skills
        .iter()
        .map(|(name, description)| {
            format!(
                "- {}: {}",
                one_line(name, 80),
                one_line(description, description_chars)
            )
        })
        .collect()
}

/// Collapses whitespace to single spaces and bounds to `limit` characters.
pub(super) fn one_line(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(limit) {
        Some((cut, _)) => format!("{}…", flat[..cut].trim_end()),
        None => flat,
    }
}

/// Why a built-in skill's `SKILL.md` was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SkillError {
    MissingFrontmatter,
    MissingField(&'static str),
    UnknownField(String),
    InvalidName(String),
    NameMismatch(String),
    DescriptionTooLong,
    EmptyBody,
}

impl std::fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillError::MissingFrontmatter => write!(f, "SKILL.md has no --- frontmatter block"),
            SkillError::MissingField(field) => write!(f, "frontmatter has no {field}"),
            SkillError::UnknownField(field) => write!(f, "unsupported frontmatter field {field}"),
            SkillError::InvalidName(name) => write!(f, "invalid skill name {name:?}"),
            SkillError::NameMismatch(name) => write!(f, "name {name:?} differs from its folder"),
            SkillError::DescriptionTooLong => write!(f, "description exceeds 1024 characters"),
            SkillError::EmptyBody => write!(f, "SKILL.md has no instructions"),
        }
    }
}

/// Parses the flat `key: value` frontmatter subset the bundled skills use.
pub(super) fn parse(raw: &RawSkill) -> Result<BundledSkill, SkillError> {
    let source = raw
        .source
        .strip_prefix("---\n")
        .ok_or(SkillError::MissingFrontmatter)?;
    let (header, body) = source
        .split_once("\n---\n")
        .ok_or(SkillError::MissingFrontmatter)?;
    let (mut name, mut description, mut allowed_tools) = (None, None, Vec::new());
    for line in header.lines().filter(|line| !line.trim().is_empty()) {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| SkillError::UnknownField(line.to_owned()))?;
        let value = unquote(value.trim());
        match key.trim() {
            "name" => name = Some(value),
            "description" => description = Some(value),
            "allowed-tools" => {
                allowed_tools = value
                    .split([',', ' '])
                    .filter(|tool| !tool.is_empty())
                    .collect()
            }
            other => return Err(SkillError::UnknownField(other.to_owned())),
        }
    }
    let name = name.ok_or(SkillError::MissingField("name"))?;
    let description = description
        .filter(|description| !description.is_empty())
        .ok_or(SkillError::MissingField("description"))?;
    if !valid_name(name) {
        return Err(SkillError::InvalidName(name.to_owned()));
    }
    if name != raw.folder {
        return Err(SkillError::NameMismatch(name.to_owned()));
    }
    if description.chars().count() > 1024 {
        return Err(SkillError::DescriptionTooLong);
    }
    let body = body.trim();
    if body.is_empty() {
        return Err(SkillError::EmptyBody);
    }
    Ok(BundledSkill {
        name,
        description,
        allowed_tools,
        body,
        resources: raw.resources,
    })
}

/// Agent Skills naming: 1–64 lowercase letters, digits and single hyphens,
/// not starting or ending with a hyphen.
fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(value)
}
