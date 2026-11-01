//! Layered settings and source-labelled permission rules, read fresh at each boundary.
//! Invalid files fail closed. Credentials remain in the credential vault.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// A rule's explicit decision, ordered independently of source precedence.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDecision {
    Allow,
    Ask,
    Deny,
}
impl PermissionDecision {
    /// The settings array holding this decision.
    pub fn key(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}
/// One editable rule and its exact origin.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionRule {
    pub pattern: String,
    pub decision: PermissionDecision,
    pub source: PathBuf,
    pub managed: bool,
}
/// Effective settings preserve rule provenance separately from the merged JSON.
#[derive(Clone, Debug, Default)]
pub struct EffectiveSettings {
    pub value: Value,
    pub rules: Vec<PermissionRule>,
    workspace: PathBuf,
}
impl EffectiveSettings {
    /// Parses a standalone settings document.
    pub fn from_value(value: Value) -> Result<Self> {
        let mut settings = Self {
            value: json!({}),
            rules: Vec::new(),
            workspace: PathBuf::new(),
        };
        settings.add(value, Path::new(""), false)?;
        Ok(settings)
    }
    fn add(&mut self, value: Value, source: &Path, managed: bool) -> Result<()> {
        ensure!(
            value.is_object(),
            "Settings must be a JSON object: {}",
            source.display()
        );
        if let Some(disabled) = value.get("disableAllHooks") {
            ensure!(disabled.is_boolean(), "disableAllHooks must be a boolean");
        }
        if let Some(permissions) = value.get("permissions") {
            ensure!(permissions.is_object(), "permissions must be an object");
            for decision in [
                PermissionDecision::Deny,
                PermissionDecision::Ask,
                PermissionDecision::Allow,
            ] {
                if let Some(list) = permissions.get(decision.key()) {
                    for pattern in list.as_array().context("Permission rules must be arrays")? {
                        let pattern = pattern
                            .as_str()
                            .context("Permission rules must be strings")?;
                        validate_pattern(pattern)?;
                        self.rules.push(PermissionRule {
                            pattern: pattern.into(),
                            decision,
                            source: source.into(),
                            managed,
                        });
                    }
                }
            }
        }
        merge(&mut self.value, value);
        Ok(())
    }
    /// Deny wins over ask, which wins over allow, regardless of file order.
    pub fn evaluate(&self, name: &str, arguments: &Value) -> Option<PermissionDecision> {
        let normalized = self.normalized(arguments);
        [
            PermissionDecision::Deny,
            PermissionDecision::Ask,
            PermissionDecision::Allow,
        ]
        .into_iter()
        .find(|decision| {
            self.rules.iter().any(|rule| {
                rule.decision == *decision
                    && (matches_rule(&rule.pattern, name, &normalized)
                        || (*decision != PermissionDecision::Allow
                            || (rule.pattern.contains("(json:")
                                || rule.pattern.contains("(fields:")))
                            && matches_rule(&rule.pattern, name, arguments))
            })
        })
    }
    fn normalized(&self, arguments: &Value) -> Value {
        let mut value = arguments.clone();
        if !self.workspace.as_os_str().is_empty()
            && let Some(path) = arguments.get("path").and_then(Value::as_str)
        {
            let root = self
                .workspace
                .canonicalize()
                .unwrap_or_else(|_| self.workspace.clone());
            let path = Path::new(path);
            let absolute = if path.is_absolute() {
                path.to_owned()
            } else {
                root.join(path)
            };
            let mut resolved = PathBuf::new();
            for part in absolute.components() {
                match part {
                    std::path::Component::ParentDir => {
                        resolved.pop();
                    }
                    std::path::Component::CurDir => {}
                    other => {
                        resolved.push(other.as_os_str());
                        if let Ok(canonical) = resolved.canonicalize() {
                            resolved = canonical;
                        }
                    }
                }
            }
            value["path"] = json!(
                resolved
                    .strip_prefix(&root)
                    .unwrap_or(&resolved)
                    .to_string_lossy()
            );
        }
        value
    }
}
/// Ordered settings locations. Explicit paths support isolated policy tests.
#[derive(Clone, Debug)]
pub struct SettingsFiles {
    workspace: PathBuf,
    paths: Vec<(PathBuf, bool)>,
}
impl SettingsFiles {
    /// Discovers managed, user, project and local settings, including Claude files.
    pub fn for_workspace(workspace: &Path) -> Self {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from);
        let managed = if cfg!(target_os = "macos") {
            PathBuf::from("/Library/Application Support/Xylo/managed-settings.json")
        } else if cfg!(target_os = "windows") {
            PathBuf::from(
                std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into()),
            )
            .join("Xylo/managed-settings.json")
        } else {
            PathBuf::from("/etc/xylo/managed-settings.json")
        };
        let mut paths = vec![(managed, true)];
        if let Some(home) = home {
            paths.push((home.join(".claude/settings.json"), false));
            paths.push((home.join(".claude/settings.local.json"), false));
            paths.push((home.join(".xylo/settings.json"), false));
        }
        for file in [
            ".claude/settings.json",
            ".xylo/settings.json",
            ".claude/settings.local.json",
            ".xylo/settings.local.json",
        ] {
            paths.push((workspace.join(file), false));
        }
        Self::from_paths(workspace, paths)
    }
    /// Uses explicit sources without consulting the user's environment.
    pub fn from_paths(workspace: &Path, paths: Vec<(PathBuf, bool)>) -> Self {
        Self {
            workspace: workspace.into(),
            paths,
        }
    }
    /// Paths holding permission authority, including absent user and managed files.
    pub fn sources(&self) -> impl Iterator<Item = &Path> {
        self.paths.iter().map(|(path, _)| path.as_path())
    }
    /// Reads fresh data so edits and revocations apply to the next action.
    pub fn load(&self) -> Result<EffectiveSettings> {
        let mut settings = EffectiveSettings {
            value: json!({}),
            rules: Vec::new(),
            workspace: self.workspace.clone(),
        };
        let mut disabled = Vec::new();
        for (path, managed) in &self.paths {
            let Some(mut value) = read(path)? else {
                continue;
            };
            if path.components().any(|p| p.as_os_str() == ".claude") {
                value = json!({"permissions":value.get("permissions").cloned().unwrap_or(json!({})),"hooks":value.get("hooks").cloned().unwrap_or(json!({}))});
            }
            if *managed {
                collect_disabled(&value, "", &mut disabled);
            }
            settings
                .add(value, path, *managed)
                .with_context(|| format!("Read settings {}", path.display()))?;
        }
        for pointer in disabled {
            let mut target = &mut settings.value;
            for part in pointer.trim_start_matches('/').split('/') {
                if !target.is_object() {
                    *target = json!({});
                }
                let part = part.replace("~1", "/").replace("~0", "~");
                target = target
                    .as_object_mut()
                    .expect("object")
                    .entry(part)
                    .or_insert(Value::Null);
            }
            *target = json!(true);
        }
        Ok(settings)
    }
    /// Adds a rule to the selected project file using an atomic replacement.
    pub fn save_rule(
        &self,
        pattern: &str,
        decision: PermissionDecision,
        project: bool,
    ) -> Result<()> {
        validate_pattern(pattern)?;
        let path = self.workspace.join(if project {
            ".xylo/settings.json"
        } else {
            ".xylo/settings.local.json"
        });
        let parent = path.parent().context("Settings parent missing")?;
        std::fs::create_dir_all(parent)?;
        ensure!(
            parent
                .canonicalize()?
                .starts_with(self.workspace.canonicalize()?),
            "Project settings directory must remain inside the project"
        );
        let _lock = lock(parent)?;
        let mut value = read(&path)?.unwrap_or(json!({}));
        ensure!(value.is_object(), "Settings must be an object");
        if value.get("permissions").is_none() {
            value["permissions"] = json!({});
        }
        ensure!(
            value["permissions"].is_object(),
            "permissions must be an object"
        );
        if value["permissions"].get(decision.key()).is_none() {
            value["permissions"][decision.key()] = json!([]);
        }
        let list = value["permissions"][decision.key()]
            .as_array_mut()
            .context("Rules must be arrays")?;
        if !list.iter().any(|v| v.as_str() == Some(pattern)) {
            list.push(json!(pattern));
        }
        if !project {
            self.ignore_local()?;
        }
        write(&path, &value)?;
        Ok(())
    }
    /// Removes only the selected source's rule. Managed rules are immutable.
    pub fn remove_rule(&self, rule: &PermissionRule) -> Result<()> {
        ensure!(
            !rule.managed && self.paths.iter().any(|(p, m)| p == &rule.source && !m),
            "Managed or unknown rule source cannot be edited"
        );
        let _lock = lock(rule.source.parent().context("Settings parent missing")?)?;
        let mut value = read(&rule.source)?.context("Rule source disappeared")?;
        if let Some(list) = value
            .pointer_mut(&format!("/permissions/{}", rule.decision.key()))
            .and_then(Value::as_array_mut)
        {
            list.retain(|v| v.as_str() != Some(&rule.pattern));
        }
        write(&rule.source, &value)
    }
    fn ignore_local(&self) -> Result<()> {
        let path = self.workspace.join(".gitignore");
        use std::io::Read;
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "The project .gitignore must not be a symlink"
            );
        }
        let mut text = String::new();
        match options.open(&path) {
            Ok(mut file) => {
                ensure!(
                    file.metadata()?.len() <= 1024 * 1024,
                    "The project .gitignore exceeds 1 MiB"
                );
                file.read_to_string(&mut text)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut changed = false;
        for entry in ["/.xylo/settings.local.json", "/.xylo/.settings.lock"] {
            if !text
                .lines()
                .any(|line| line.trim_start_matches('/') == entry.trim_start_matches('/'))
            {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(entry);
                text.push('\n');
                changed = true;
            }
        }
        if changed {
            write_bytes(&path, text.as_bytes())?;
        }
        Ok(())
    }
}
fn read(path: &Path) -> Result<Option<Value>> {
    if let Ok(metadata) = std::fs::metadata(path) {
        ensure!(
            metadata.len() <= 1024 * 1024,
            "Settings exceed 1 MiB: {}",
            path.display()
        );
    }
    let bytes = match std::fs::read(path) {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("Read {}", path.display())),
    };
    ensure!(
        bytes.len() <= 1024 * 1024,
        "Settings exceed 1 MiB: {}",
        path.display()
    );
    Ok(Some(
        serde_json::from_slice(&bytes).with_context(|| format!("Parse {}", path.display()))?,
    ))
}
fn lock(parent: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(parent.join(".settings.lock"))?;
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}
fn write(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    write_bytes(path, &bytes)
}
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("Settings parent missing")?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    Ok(())
}
fn merge(base: &mut Value, incoming: Value) {
    match (base, incoming) {
        (Value::Object(base), Value::Object(incoming)) => {
            for (key, value) in incoming {
                merge(base.entry(key).or_insert(Value::Null), value)
            }
        }
        (Value::Array(base), Value::Array(mut incoming)) => base.append(&mut incoming),
        (base, incoming) => *base = incoming,
    }
}
fn collect_disabled(value: &Value, prefix: &str, output: &mut Vec<String>) {
    if let Some(map) = value.as_object() {
        for (key, value) in map {
            let pointer = format!("{prefix}/{}", key.replace('~', "~0").replace('/', "~1"));
            if key.starts_with("disable") && value == true {
                output.push(pointer.clone());
            }
            collect_disabled(value, &pointer, output);
        }
    }
}
/// The canonical Xylo spelling of a Claude tool name.
pub fn canonical_tool(name: &str) -> &str {
    match name {
        "Bash" => "shell_command",
        "Edit" => "file_edit",
        "Write" => "file_write",
        "Read" => "file_read",
        "WebFetch" => "web_fetch",
        "Glob" => "find_files",
        "Grep" => "search_project",
        _ => name,
    }
}
fn validate_pattern(pattern: &str) -> Result<()> {
    ensure!(
        !pattern.trim().is_empty() && pattern.len() <= 8192,
        "Invalid permission pattern"
    );
    if pattern.contains('(') {
        ensure!(pattern.ends_with(')'), "Permission pattern must end with )");
    }
    Ok(())
}
fn glob(pattern: &str, text: &str) -> bool {
    let expression = format!(
        "^{}$",
        regex::escape(pattern)
            .replace(r"\*\*", ".*")
            .replace(r"\*", ".*")
            .replace(r"\?", ".")
    );
    regex::Regex::new(&expression).is_ok_and(|r| r.is_match(text))
}
/// Matches a permission pattern without interpreting shell text as code.
pub fn matches_rule(pattern: &str, name: &str, args: &Value) -> bool {
    let (tool, selector) = pattern
        .split_once('(')
        .map_or((pattern, None), |(name, tail)| {
            (name, tail.strip_suffix(')'))
        });
    let name = canonical_tool(name);
    let tool = canonical_tool(tool);
    if !(glob(tool, name) || tool == "shell_command" && name == "run_command") {
        return false;
    }
    let Some(selector) = selector else {
        return true;
    };
    if let Some(fields) = selector.strip_prefix("fields:") {
        return serde_json::from_str::<serde_json::Map<String, Value>>(fields).is_ok_and(
            |fields| {
                !fields.is_empty()
                    && fields
                        .iter()
                        .all(|(key, value)| args.get(key) == Some(value))
            },
        );
    }
    if let Some(exact) = selector.strip_prefix("json:") {
        return serde_json::from_str::<Value>(exact).is_ok_and(|v| v == *args);
    }
    let text = if matches!(name, "shell_command" | "run_command") {
        args.get("command")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                args.get("program").and_then(Value::as_str).map(|p| {
                    std::iter::once(p)
                        .chain(
                            args["args"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(Value::as_str),
                        )
                        .collect::<Vec<_>>()
                        .join(" ")
                })
            })
    } else if name == "web_fetch" {
        args.get("url")
            .and_then(Value::as_str)
            .and_then(|u| url::Url::parse(u).ok())
            .and_then(|u| u.host_str().map(|h| format!("domain:{h}")))
    } else {
        args.get("path")
            .and_then(Value::as_str)
            .map(|p| p.trim_start_matches("./").to_owned())
    };
    let Some(text) = text else { return false };
    if let Some(prefix) = selector.strip_suffix(":*") {
        !text
            .chars()
            .any(|c| matches!(c, ';' | '&' | '|' | '\n' | '`'))
            && !text.contains("$(")
            && (text == prefix
                || text
                    .strip_prefix(prefix)
                    .is_some_and(|tail| tail.starts_with(char::is_whitespace)))
    } else {
        glob(selector, &text)
    }
}

#[cfg(test)]
mod tests;
