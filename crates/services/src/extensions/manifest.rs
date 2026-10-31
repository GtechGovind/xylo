use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Package metadata is untrusted. Bound both input and expanded YAML work;
/// optional filesystem includes and property interpolation are not compiled in.
pub(super) fn parse_yaml<T: serde::de::DeserializeOwned>(input: &str) -> Result<T> {
    ensure!(input.len() <= 256 * 1024, "YAML metadata exceeds 256 KiB");
    let options = serde_saphyr::options! {
        with_snippet: false,
        reject_unsupported_tags: true,
        budget: serde_saphyr::budget! {
            max_documents: 1,
            max_depth: 32,
            flow_nesting_limit: 32,
            max_events: 10_000,
            max_nodes: 10_000,
            max_anchors: 64,
            max_aliases: 64,
            max_recorded_anchor_events: 10_000,
            max_recorded_anchor_bytes: 1024 * 1024,
            max_total_scalar_bytes: 1024 * 1024,
            max_inclusion_depth: 0,
        },
        alias_limits: serde_saphyr::alias_limits! {
            max_total_replayed_events: 64,
            max_alias_expansions_per_anchor: 8,
            max_replay_stack_depth: 8,
        },
    };
    serde_saphyr::from_str_with_options(input, options)
        .context("Invalid or over-budget YAML metadata")
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComponentKind {
    Skill,
    Command,
    Agent,
    Hooks,
    Mcp,
    LanguageServer,
    HostedIntegration,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Readiness {
    Installed,
    NeedsSetup,
    UnsupportedRequirement,
    ExecutionVerified,
}
impl Readiness {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Installed => "Installed",
            Self::NeedsSetup => "Needs setup",
            Self::UnsupportedRequirement => "Unsupported requirement",
            Self::ExecutionVerified => "Execution verified",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtensionComponent {
    pub name: String,
    pub kind: ComponentKind,
    pub path: String,
    pub description: String,
    pub readiness: Readiness,
    pub requirements: Vec<String>,
    pub evidence: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtensionPackage {
    pub id: String,
    pub name: String,
    pub version: String,
    pub source: String,
    pub revision: Option<String>,
    pub digest: String,
    pub enabled: bool,
    pub components: Vec<ExtensionComponent>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub id: String,
    pub name: String,
    pub description: String,
    pub automatic: bool,
    pub user_invocable: bool,
    pub kind: ComponentKind,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LoadedSkill {
    pub id: String,
    /// One-line description from the component manifest, for display.
    pub description: String,
    pub body: String,
    pub package_root: PathBuf,
    pub allowed_tools: Vec<String>,
    pub tools_restricted: bool,
    pub agent: Option<String>,
    pub fork: bool,
    pub model: Option<String>,
    pub dynamic_commands: Vec<String>,
}
#[derive(Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub(super) struct Frontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub disable_model_invocation: bool,
    pub user_invocable: Option<bool>,
    pub allowed_tools: Option<serde_json::Value>,
    pub tools: Option<serde_json::Value>,
    pub context: Option<String>,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub hooks: Option<serde_json::Value>,
    #[serde(skip)]
    pub host_requirements: Vec<String>,
    #[serde(flatten)]
    pub other: BTreeMap<String, serde_json::Value>,
}
pub(super) fn read_prompt(path: &Path) -> Result<(Frontmatter, String)> {
    let bytes = std::fs::metadata(path)?.len();
    ensure!(bytes <= 256 * 1024, "Instruction file exceeds 256 KiB");
    let text = std::fs::read_to_string(path)?.replace("\r\n", "\n");
    if let Some(rest) = text.strip_prefix("---\n") {
        let (yaml, body) = rest
            .split_once("\n---\n")
            .context("Missing frontmatter closing delimiter")?;
        let mut metadata = parse_yaml(yaml).context("Invalid skill frontmatter")?;
        apply_codex_metadata(path, &mut metadata)?;
        Ok((metadata, body.into()))
    } else {
        let mut metadata = Frontmatter::default();
        apply_codex_metadata(path, &mut metadata)?;
        Ok((metadata, text))
    }
}
pub(super) fn read_metadata(path: &Path) -> Result<Frontmatter> {
    use std::io::{BufRead, BufReader, Read};
    ensure!(
        std::fs::metadata(path)?.len() <= 256 * 1024,
        "Instruction file exceeds 256 KiB"
    );
    let mut reader = BufReader::new(std::fs::File::open(path)?).take(256 * 1024);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut metadata = if line.trim_end_matches(['\r', '\n']) == "---" {
        let mut yaml = String::new();
        loop {
            line.clear();
            ensure!(
                reader.read_line(&mut line)? != 0,
                "Missing frontmatter closing delimiter"
            );
            if line.trim_end_matches(['\r', '\n']) == "---" {
                break;
            }
            yaml.push_str(&line);
        }
        parse_yaml(&yaml).context("Invalid skill frontmatter")?
    } else {
        Frontmatter::default()
    };
    apply_codex_metadata(path, &mut metadata)?;
    Ok(metadata)
}
// Codex's policy sidecar is harness input, not text to append to the prompt.
fn apply_codex_metadata(path: &Path, metadata: &mut Frontmatter) -> Result<()> {
    if path.file_name().is_none_or(|name| name != "SKILL.md") {
        return Ok(());
    }
    let sidecar = path
        .parent()
        .context("Skill directory missing")?
        .join("agents/openai.yaml");
    if !sidecar.exists() {
        return Ok(());
    }
    ensure!(
        std::fs::metadata(&sidecar)?.len() <= 256 * 1024,
        "Skill host metadata exceeds 256 KiB"
    );
    let value: Value =
        parse_yaml(&std::fs::read_to_string(sidecar)?).context("Invalid agents/openai.yaml")?;
    let object = value
        .as_object()
        .context("Skill host metadata must be a mapping")?;
    for key in object
        .keys()
        .filter(|key| !["interface", "policy", "dependencies"].contains(&key.as_str()))
    {
        metadata
            .host_requirements
            .push(format!("Unrecognized skill host metadata: {key}"));
    }
    if let Some(policy) = value.get("policy") {
        let policy = policy
            .as_object()
            .context("Skill host policy must be a mapping")?;
        for key in policy
            .keys()
            .filter(|key| !["allow_implicit_invocation", "products"].contains(&key.as_str()))
        {
            metadata
                .host_requirements
                .push(format!("Unsupported skill host policy: {key}"));
        }
        if let Some(allow) = policy.get("allow_implicit_invocation") {
            metadata.disable_model_invocation |= !allow
                .as_bool()
                .context("allow_implicit_invocation must be boolean")?;
        }
        if let Some(products) = policy.get("products") {
            let products = products
                .as_array()
                .context("Skill products policy must be a list")?;
            if !products.is_empty() && !products.iter().any(|p| p.as_str() == Some("xylo")) {
                metadata
                    .host_requirements
                    .push("Skill restricts invocation to another host product".into());
            }
        }
    }
    if let Some(dependencies) = value.get("dependencies") {
        let dependencies = dependencies
            .as_object()
            .context("Skill dependencies must be a mapping")?;
        for key in dependencies.keys().filter(|key| key.as_str() != "tools") {
            metadata
                .host_requirements
                .push(format!("Unsupported skill dependency declaration: {key}"));
        }
        if let Some(tools) = dependencies.get("tools") {
            for tool in tools
                .as_array()
                .context("Skill tool dependencies must be a list")?
            {
                let kind = tool["type"]
                    .as_str()
                    .context("Skill dependency type missing")?;
                let name = tool["value"]
                    .as_str()
                    .context("Skill dependency identifier missing")?;
                metadata.host_requirements.push(format!(
                    "Required {kind} dependency needs a bound adapter: {name}"
                ));
            }
        }
    }
    Ok(())
}
pub(super) fn tool_names(value: Option<&serde_json::Value>) -> Vec<String> {
    match value {
        Some(serde_json::Value::String(s)) => {
            let mut names = Vec::new();
            let mut current = String::new();
            let mut depth = 0usize;
            for c in s.chars() {
                if c == '(' {
                    depth += 1;
                } else if c == ')' {
                    depth = depth.saturating_sub(1);
                }
                if depth == 0 && (c == ',' || c.is_whitespace()) {
                    if !current.is_empty() {
                        names.push(std::mem::take(&mut current));
                    }
                } else {
                    current.push(c);
                }
            }
            if !current.is_empty() {
                names.push(current);
            }
            names
        }
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .filter_map(|v| v.as_str().map(str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}
/// Claude Code tool aliases the host maps onto its own tools when a skill or
/// agent restricts its child's tools.
const TOOL_ALIASES: &[&str] = &["Read", "Write", "Edit", "Glob", "Grep", "Bash"];

/// Why an `allowed-tools`/`tools` entry cannot be honored, if it cannot. The
/// host accepts the aliases above and exact host tool names (lowercase
/// snake_case); parameter patterns such as `Bash(git:*)` would widen or
/// narrow permissions in ways the host cannot enforce, so they are refused
/// here, at install, rather than when the skill is invoked.
fn unsupported_tool(name: &str) -> Option<String> {
    if TOOL_ALIASES.contains(&name) {
        return None;
    }
    if name.contains(['(', ')', '*', ':']) {
        return Some(format!(
            "Unsupported allowed-tools pattern: {name}; list whole tools without parameter patterns"
        ));
    }
    let host_name = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
    (!host_name).then(|| {
        format!(
            "Unsupported allowed-tools entry: {name}; use {} or an exact Xylo tool name",
            TOOL_ALIASES.join(", ")
        )
    })
}
pub(super) fn safe_path(root: &Path, relative: &str) -> Result<PathBuf> {
    let relative = Path::new(relative);
    ensure!(
        !relative.is_absolute()
            && relative.components().all(|c| matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )),
        "Package path must stay inside its root"
    );
    let path = root.join(relative).canonicalize()?;
    ensure!(
        path.starts_with(root.canonicalize()?),
        "Package path escapes root"
    );
    Ok(path)
}
fn component(root: &Path, path: &Path, kind: ComponentKind) -> Result<ExtensionComponent> {
    let meta = read_metadata(path)?;
    let default_name = if kind == ComponentKind::Skill {
        path.parent().and_then(Path::file_name)
    } else {
        path.file_stem()
    }
    .context("Missing component name")?
    .to_string_lossy()
    .into_owned();
    let name = meta.name.unwrap_or(default_name);
    ensure!(
        !name.is_empty() && !name.contains([':', '/', '\\']),
        "Invalid component name"
    );
    let mut requirements = meta.host_requirements.clone();
    let supported = [
        "argument-hint",
        "license",
        "compatibility",
        "metadata",
        "version",
    ];
    for key in meta
        .other
        .keys()
        .filter(|key| !supported.contains(&key.as_str()))
    {
        requirements.push(format!("Unrecognized frontmatter: {key}"));
    }
    if meta.context.as_deref().is_some_and(|c| c != "fork") {
        requirements.push("Unknown execution context".into());
    }
    if meta.hooks.is_some() {
        requirements.push("Skill-scoped hooks require lifecycle setup".into());
    }
    requirements.extend(
        tool_names(meta.allowed_tools.as_ref().or(meta.tools.as_ref()))
            .iter()
            .filter_map(|name| unsupported_tool(name)),
    );
    Ok(ExtensionComponent {
        name,
        kind,
        path: path.strip_prefix(root)?.to_string_lossy().into_owned(),
        description: meta.description.unwrap_or_default(),
        readiness: if requirements.is_empty() {
            Readiness::Installed
        } else {
            Readiness::UnsupportedRequirement
        },
        requirements,
        evidence: None,
    })
}
fn walk_prompts(
    root: &Path,
    dir: &Path,
    kind: ComponentKind,
    out: &mut Vec<ExtensionComponent>,
) -> Result<()> {
    if dir.is_file() {
        out.push(component(root, dir, kind)?);
        return Ok(());
    }
    let mut entries = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if e.file_type()?.is_symlink() {
            bail!("Symlinks are not allowed in extension packages");
        }
        let path = e.path();
        if path.is_dir() {
            walk_prompts(root, &path, kind.clone(), out)?;
        } else if (kind == ComponentKind::Skill && e.file_name() == "SKILL.md")
            || (kind != ComponentKind::Skill && path.extension().is_some_and(|e| e == "md"))
        {
            out.push(component(root, &path, kind.clone())?);
        }
        ensure!(out.len() <= 1000, "Too many extension components");
    }
    Ok(())
}
pub(super) fn inspect(
    root: &Path,
    fallback: &str,
) -> Result<(String, String, Vec<ExtensionComponent>)> {
    let mut out = Vec::new();
    let manifest_path = [
        ".codex-plugin/plugin.json",
        ".claude-plugin/plugin.json",
        ".agent-plugin/plugin.json",
    ]
    .into_iter()
    .find(|p| root.join(p).is_file());
    let manifest: Value = if let Some(path) = manifest_path {
        let path = safe_path(root, path)?;
        ensure!(
            std::fs::metadata(&path)?.len() <= 256 * 1024,
            "Manifest too large"
        );
        serde_json::from_slice(&std::fs::read(path)?)?
    } else {
        Value::Null
    };
    let mut name = manifest["name"].as_str().unwrap_or(fallback).to_string();
    let version = manifest["version"]
        .as_str()
        .unwrap_or("unversioned")
        .to_owned();
    if root.join("SKILL.md").is_file() {
        let item = component(root, &root.join("SKILL.md"), ComponentKind::Skill)?;
        name = item.name.clone();
        out.push(item);
    }
    for (key, kind) in [
        ("skills", ComponentKind::Skill),
        ("commands", ComponentKind::Command),
        ("agents", ComponentKind::Agent),
    ] {
        let mut paths = Vec::new();
        if root.join(key).is_dir() {
            paths.push(key.to_string());
        }
        match manifest.get(key) {
            Some(Value::String(p)) => paths.push(p.clone()),
            Some(Value::Array(a)) => {
                for p in a {
                    paths.push(
                        p.as_str()
                            .context("Component path must be a string")?
                            .into(),
                    );
                }
            }
            Some(_) => bail!("Invalid {key} paths"),
            None => {}
        }
        let mut resolved = std::collections::BTreeSet::new();
        for path in paths {
            let path = safe_path(root, &path)?;
            if resolved.insert(path.clone()) {
                walk_prompts(root, &path, kind.clone(), &mut out)?;
            }
        }
    }
    for (key, default, kind) in [
        ("mcpServers", ".mcp.json", ComponentKind::Mcp),
        ("hooks", "hooks/hooks.json", ComponentKind::Hooks),
        ("lspServers", ".lsp.json", ComponentKind::LanguageServer),
        ("apps", ".app.json", ComponentKind::HostedIntegration),
    ] {
        let declared = manifest.get(key);
        let alternate = key == "hooks" && root.join("hooks.json").is_file();
        if declared.is_some() || root.join(default).is_file() || alternate {
            let path = declared.and_then(Value::as_str).unwrap_or(if alternate {
                "hooks.json"
            } else {
                default
            });
            if declared.is_none_or(|v| v.is_string()) {
                safe_path(root, path)?;
            }
            out.push(ExtensionComponent {
                name: key.into(),
                kind,
                path: if declared.is_some_and(|v| !v.is_string()) {
                    format!("{}#{key}", manifest_path.unwrap_or_default())
                } else {
                    path.into()
                },
                description: String::new(),
                readiness: Readiness::NeedsSetup,
                requirements: vec![format!("Configure and verify {key}")],
                evidence: None,
            });
        }
    }
    let mut names = std::collections::BTreeSet::new();
    out.retain(|c| names.insert((c.path.clone(), format!("{:?}", c.kind))));
    let mut invocations = std::collections::BTreeSet::new();
    for c in &out {
        if matches!(
            c.kind,
            ComponentKind::Skill | ComponentKind::Command | ComponentKind::Agent
        ) {
            ensure!(
                invocations.insert(c.name.clone()),
                "Duplicate component invocation: {}",
                c.name
            );
        }
    }
    for c in out.iter_mut().filter(|c| c.kind == ComponentKind::Agent) {
        let meta = read_metadata(&safe_path(root, &c.path)?)?;
        if meta.agent.is_some() {
            c.requirements
                .push("Nested agent-role selection is unsupported".into());
            c.readiness = Readiness::UnsupportedRequirement;
        }
    }
    let agents = out
        .iter()
        .filter(|c| c.kind == ComponentKind::Agent)
        .map(|c| (c.name.clone(), c.readiness.clone()))
        .collect::<BTreeMap<_, _>>();
    for c in &mut out {
        if !matches!(c.kind, ComponentKind::Skill | ComponentKind::Command) {
            continue;
        }
        let meta = read_metadata(&safe_path(root, &c.path)?)?;
        if let Some(agent) = meta.agent.as_deref() {
            if meta.context.as_deref() != Some("fork") || c.kind == ComponentKind::Agent {
                c.requirements
                    .push("Agent selection requires a forked skill or command".into());
            } else if !matches!(agent, "Explore" | "Plan" | "general-purpose")
                && agents.get(agent) != Some(&Readiness::Installed)
            {
                c.requirements.push(format!(
                    "Required agent is unavailable or unsupported: {agent}"
                ));
            }
            if !c.requirements.is_empty() {
                c.readiness = Readiness::UnsupportedRequirement;
            }
        }
    }
    ensure!(!out.is_empty(), "No portable extension components found");
    Ok((name, version, out))
}
