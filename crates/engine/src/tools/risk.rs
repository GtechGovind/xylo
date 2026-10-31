//! Deterministic risk assessment shared by the permission presets.
//!
//! [`assess`] returns a reason when a call is potentially unsafe: destructive
//! or irreversible commands, privilege escalation, code piped from the
//! network into an interpreter, uploads and publishing, credential and
//! secret access, writes outside the project, project files that make tools
//! run code later, and every tool in [`TOOL_RISKS`] that acts outside both
//! the project and the command sandbox (desktop input, the browser, plugins,
//! remote environments, schedules). Everything else returns `None`. The rules
//! are plain tables over tool names and parsed shell words, so the same call
//! always gets the same answer.
//!
//! [`routine_fetch`] recognizes the few network commands that only download:
//! the program is a bare name written first (no environment assignment,
//! wrapper or path), every option is on a per-tool allowlist, and nothing in
//! the command can name another program to run (`git -c`, `--upload-pack`,
//! `ext::` URLs, `--template`, npm git or file specs, brew formula files).
//! Git additionally needs the project's own `.git`, whose configuration the
//! model cannot write, so a crafted repository cannot supply hooks or an
//! `sshCommand`. A command that would be routine still asks, as
//! [`Fetch::Configured`], when project configuration the tool reads can run
//! code or redirect sources: `.cargo/config(.toml)`, `.npmrc`, `.yarnrc`,
//! `.yarnrc.yml`, `.pnpmfile.cjs`, a `go.work` with `replace`, or git
//! settings such as `core.sshCommand`, `url.*.insteadOf` or includes.
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

/// Why `name` with `arguments` needs the user's decision, if it does.
pub(crate) fn assess(name: &str, arguments: &Value, workspace: &Path) -> Option<String> {
    let text = |key: &str| arguments.get(key).and_then(Value::as_str);
    match name {
        "file_write" | "file_edit" => {
            let path = resolve(workspace, text("path")?);
            if !inside(workspace, &path) {
                return Some(format!(
                    "Changes a file outside the project: {}",
                    path.display()
                ));
            }
            secret_path(&path)
                .map(|kind| format!("Changes {kind}"))
                .or_else(|| auto_run_config(workspace, &path))
        }
        "file_read" | "files_list" | "find_files" | "search_project" => {
            let path = resolve(workspace, text("path").unwrap_or("."));
            secret_path(&path).map(|kind| format!("Reads {kind}"))
        }
        "run_command" => {
            let program = text("program")?;
            let args: Vec<String> = arguments
                .get("args")
                .and_then(Value::as_array)
                .map(|args| {
                    args.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            let mut words = vec![program.to_owned()];
            words.extend(args);
            command_words(&words, workspace, 0)
        }
        "shell_command" => command(text("command")?, workspace),
        _ => tool_risk(name).map(str::to_owned),
    }
}

/// Which tools a [`TOOL_RISKS`] entry covers.
#[derive(Clone, Copy, Debug)]
enum Tools {
    /// Exactly this tool.
    Named(&'static str),
    /// Every tool whose name starts with this prefix.
    Family(&'static str),
}

impl Tools {
    fn matches(self, name: &str) -> bool {
        match self {
            Self::Named(named) => name == named,
            Self::Family(prefix) => name.starts_with(prefix),
        }
    }
}

/// Tools whose risk does not depend on their arguments; the first matching
/// entry wins and `None` marks a tool known to be harmless. Each family ends
/// with a catch-all, so a tool added to it later asks until it is listed.
/// These tools act outside the command sandbox: native input lands in
/// whatever app has focus (a terminal included), the browser can send data
/// to any site, plugins and remote environments run code the sandbox never
/// sees, and schedules run later without anyone watching.
const TOOL_RISKS: &[(Tools, Option<&str>)] = {
    use Tools::{Family, Named};
    &[
        (Named("desktop_displays"), None),
        (
            Named("desktop_screenshot"),
            Some("Captures your screen, which can show anything outside the project"),
        ),
        (
            Named("desktop_click"),
            Some("Clicks in whatever app is in front, outside the sandbox"),
        ),
        (
            Named("desktop_type"),
            Some("Types into whatever app has focus, outside the sandbox"),
        ),
        (
            Family("desktop_"),
            Some("Uses your screen, mouse or keyboard outside the sandbox"),
        ),
        (Named("browser_snapshot"), None),
        (Named("browser_wait"), None),
        (Named("browser_screenshot"), None),
        (Named("browser_close"), None),
        (
            Named("browser_navigate"),
            Some("Opens a web page, which can send data to any site"),
        ),
        (
            Named("browser_click"),
            Some("Clicks in a web page, which can submit forms or start actions on a site"),
        ),
        (
            Named("browser_type"),
            Some("Types into a web page, which can send data to a site"),
        ),
        (
            Named("browser_download"),
            Some("Downloads a file from the web"),
        ),
        (
            Family("browser_"),
            Some("Uses the browser and the internet"),
        ),
        (Named("plugin_list"), None),
        (
            Family("plugin_"),
            Some("Runs code from an installed plugin"),
        ),
        (Named("environment_list"), None),
        (Named("environment_status"), None),
        (
            Family("environment_"),
            Some("Runs in a remote or container environment outside the sandbox"),
        ),
        (Named("schedule_list"), None),
        (
            Named("schedule_create"),
            Some("Schedules work that runs later without supervision"),
        ),
        (Family("schedule_"), Some("Changes scheduled work")),
        (
            Named("image_generate"),
            Some("Generates an image with the saved provider, which may incur a charge"),
        ),
    ]
};

/// The argument-independent risk of `name`, from [`TOOL_RISKS`].
fn tool_risk(name: &str) -> Option<&'static str> {
    TOOL_RISKS
        .iter()
        .find(|(tools, _)| tools.matches(name))
        .and_then(|(_, reason)| *reason)
}

/// Whether a command that asked for internet access only downloads
/// dependencies or repository history with a well-known tool. Every simple
/// command of a shell line must qualify or be a harmless local step; changing
/// directory is not one, since a fetch must read the project root's own
/// configuration.
pub(crate) fn routine_fetch(name: &str, arguments: &Value, workspace: &Path) -> Fetch {
    let Some(commands) = fetch_commands(name, arguments) else {
        return Fetch::Other;
    };
    let routine = !commands.is_empty()
        && commands.iter().any(|words| fetches(words, workspace))
        && commands
            .iter()
            .all(|words| fetches(words, workspace) || harmless(words));
    if !routine {
        return Fetch::Other;
    }
    commands
        .iter()
        .filter(|words| fetches(words, workspace))
        .find_map(|words| project_configuration(&words[0], workspace))
        .map_or(Fetch::Routine, Fetch::Configured)
}

/// How [`routine_fetch`] classifies a network command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Fetch {
    /// Only downloads, with no project configuration steering the tool.
    Routine,
    /// Would be routine, but project configuration the tool reads can run
    /// code or redirect where it downloads from; the reason says which.
    Configured(String),
    /// Anything else.
    Other,
}

/// The simple commands of a `run_command` or `shell_command` call, or `None`
/// when the line uses substitutions, redirections or unbalanced quotes.
fn fetch_commands(name: &str, arguments: &Value) -> Option<Vec<Vec<String>>> {
    let commands: Vec<Vec<String>> = match name {
        "run_command" => {
            let program = arguments.get("program").and_then(Value::as_str)?;
            let mut words = vec![program.to_owned()];
            words.extend(
                arguments
                    .get("args")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_owned),
            );
            vec![words]
        }
        "shell_command" => {
            let line = arguments.get("command").and_then(Value::as_str)?;
            if !substitutions(line).is_empty() || line.contains(['>', '<']) {
                return None;
            }
            segments(line)
                .into_iter()
                .map(|(segment, _)| shell_words::split(&segment).ok())
                .collect::<Option<_>>()?
        }
        _ => return None,
    };
    Some(commands)
}

/// Project files each fetch tool reads that can run code or redirect where
/// it downloads from: cargo credential providers and source replacement,
/// npm's `git` and registry settings, yarn and pnpm hooks. Fetches run at
/// the project root (no `cd`, `--prefix` or `-C` qualifies), so the root is
/// the only project directory these tools consult.
const FETCH_CONFIGURATION: &[(&str, &[&str])] = &[
    ("cargo", &[".cargo/config", ".cargo/config.toml"]),
    (
        "npm",
        &[".npmrc", ".yarnrc", ".yarnrc.yml", ".pnpmfile.cjs"],
    ),
    (
        "pnpm",
        &[".npmrc", ".yarnrc", ".yarnrc.yml", ".pnpmfile.cjs"],
    ),
];

/// Git settings that run a program or redirect a fetch (`section.key`, or
/// `section.*.key` for any subsection). Includes ask too: the included file
/// was not read.
const GIT_RISKY_SETTINGS: &[&str] = &[
    "core.sshcommand",
    "core.fsmonitor",
    "core.hookspath",
    "core.gitproxy",
    "core.askpass",
    "credential.helper",
    "credential.*.helper",
    "protocol.allow",
    "protocol.*.allow",
    "url.*.insteadof",
    "remote.*.uploadpack",
    "include.path",
    "includeif.*.path",
];

/// Why the project's configuration makes a fetch with `program` not
/// routine, if it does.
fn project_configuration(program: &str, workspace: &Path) -> Option<String> {
    match program {
        "git" => git_configuration(workspace),
        "go" => {
            let work = std::fs::read_to_string(workspace.join("go.work")).ok()?;
            work.lines()
                .any(|line| line.trim_start().starts_with("replace"))
                .then(|| "go.work replaces modules, so the download is redirected".into())
        }
        _ => FETCH_CONFIGURATION
            .iter()
            .find(|(tool, _)| *tool == program)?
            .1
            .iter()
            .find(|file| workspace.join(file).exists())
            .map(|file| {
                format!("{file} can run code or redirect downloads for {program}; review it first")
            }),
    }
}

/// Why the project's git configuration makes a fetch not routine, if it
/// does. A linked worktree's `.git` file is followed to its own and the
/// shared configuration; anything unreadable asks.
fn git_configuration(workspace: &Path) -> Option<String> {
    let unreadable = || Some("The repository's git configuration could not be checked".into());
    let dot_git = workspace.join(".git");
    let files = if dot_git.is_dir() {
        vec![dot_git.join("config")]
    } else {
        let Some(pointer) = std::fs::read_to_string(&dot_git).ok().and_then(|text| {
            text.trim()
                .strip_prefix("gitdir:")
                .map(|dir| dir.trim().to_owned())
        }) else {
            return unreadable();
        };
        let git_dir = workspace.join(pointer);
        let common = std::fs::read_to_string(git_dir.join("commondir"))
            .map_or_else(|_| git_dir.clone(), |common| git_dir.join(common.trim()));
        vec![common.join("config"), git_dir.join("config.worktree")]
    };
    for file in files {
        let text = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return unreadable(),
        };
        if let Some(setting) = risky_git_setting(&text) {
            return Some(format!(
                "The repository's git configuration sets {setting}, which can run a program or redirect the fetch"
            ));
        }
    }
    None
}

/// The first [`GIT_RISKY_SETTINGS`] entry a git config file sets, by a line
/// scan: `[section "subsection"]` headers (or `[section.subsection]`), then
/// `key = value` or bare `key` lines. Names compare ignoring case.
fn risky_git_setting(text: &str) -> Option<&'static str> {
    let mut section = String::new();
    let mut subsection = false;
    for line in text.lines() {
        let mut line = line.trim();
        if let Some(header) = line.strip_prefix('[') {
            let (name, rest) = header.split_once(']').unwrap_or((header, ""));
            let name = name.trim();
            let base = name
                .split(|c: char| c.is_whitespace() || c == '"' || c == '.')
                .next()
                .unwrap_or_default();
            subsection = base.len() < name.len();
            section = base.to_ascii_lowercase();
            // A key may follow the header on the same line.
            line = rest.trim();
        }
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        let key = line
            .split(['=', ' ', '\t'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let found = GIT_RISKY_SETTINGS.iter().find(|setting| {
            match setting.split('.').collect::<Vec<_>>().as_slice() {
                [name, key_name] => section == *name && !subsection && key == *key_name,
                [name, "*", key_name] => section == *name && subsection && key == *key_name,
                _ => false,
            }
        });
        if found.is_some() {
            return found.copied();
        }
    }
    None
}

/// The options a download command may carry.
struct Options {
    /// Options that stand alone.
    flags: &'static [&'static str],
    /// Options taking a value, as `--name value` or `--name=value`.
    valued: &'static [&'static str],
}

const CARGO_FETCH: Options = Options {
    flags: &[
        "--locked",
        "--frozen",
        "--offline",
        "-q",
        "--quiet",
        "-v",
        "-vv",
        "--verbose",
    ],
    valued: &["--manifest-path", "--target", "--color"],
};

const GO_MOD_DOWNLOAD: Options = Options {
    flags: &["-json", "-x"],
    valued: &[],
};

const NPM_INSTALL: Options = Options {
    flags: &[
        "--ignore-scripts",
        "--frozen-lockfile",
        "--prefer-offline",
        "--no-audit",
        "--no-fund",
        "--legacy-peer-deps",
        "--prod",
        "--production",
        "--no-save",
        "--save-dev",
        "-D",
        "--save-exact",
        "-E",
        "--silent",
    ],
    valued: &["--omit", "--include", "--loglevel"],
};

const BREW_FETCH: Options = Options {
    flags: &[
        "--formula",
        "--formulae",
        "--cask",
        "--casks",
        "--force",
        "-f",
        "--quiet",
        "-q",
        "--verbose",
        "-v",
    ],
    valued: &[],
};

/// Options of `git fetch`, `pull` and `clone` that only shape what is
/// downloaded. Everything else asks, notably `-c`/`--config`,
/// `--upload-pack`/`-u`, `--template`, `--recurse-submodules`, merge
/// strategies and signature checks, which can each run another program.
const GIT_FETCH: Options = Options {
    flags: &[
        "--all",
        "--prune",
        "-p",
        "--prune-tags",
        "--tags",
        "--no-tags",
        "-n",
        "--unshallow",
        "--update-shallow",
        "--quiet",
        "-q",
        "--verbose",
        "-v",
        "--progress",
        "--no-progress",
        "--force",
        "-f",
        "--atomic",
        "--dry-run",
        "--ff-only",
        "--ff",
        "--no-ff",
        "--rebase",
        "-r",
        "--no-rebase",
        "--autostash",
        "--no-autostash",
        "--no-edit",
        "--single-branch",
        "--no-single-branch",
        "--no-checkout",
        "--bare",
        "--no-recurse-submodules",
    ],
    valued: &[
        "--depth",
        "--deepen",
        "--shallow-since",
        "--shallow-exclude",
        "--filter",
        "--branch",
        "-b",
        "--origin",
        "-o",
        "--jobs",
        "-j",
    ],
};

/// Whether one simple command only downloads, without running project or
/// dependency code or any program the command names: build scripts, tests,
/// install scripts, source builds and plugins all ask even when they would
/// otherwise be routine.
fn fetches(words: &[String], workspace: &Path) -> bool {
    // An assignment (`GIT_SSH_COMMAND=…`), a wrapper or a path (`./git`)
    // could change what runs, so the program is a bare name written first.
    let Some((program, rest)) = words.split_first() else {
        return false;
    };
    if program.contains(['/', '\\', '=']) {
        return false;
    }
    let first = rest.first().map(String::as_str);
    let after = |count: usize| positionals(rest.get(count..).unwrap_or_default(), options(program));
    match program.as_str() {
        "cargo" => first == Some("fetch") && after(1).is_some_and(|words| words.is_empty()),
        "go" => rest.len() >= 2 && rest[0] == "mod" && rest[1] == "download" && after(2).is_some(),
        "npm" | "pnpm" => {
            matches!(first, Some("ci" | "install" | "i"))
                && rest.iter().any(|word| word == "--ignore-scripts")
                && after(1).is_some_and(|specs| specs.into_iter().all(registry_package))
        }
        "brew" => {
            first == Some("fetch")
                && after(1).is_some_and(|names| names.into_iter().all(formula_name))
        }
        // Global options (`-c`, `-C`, `--git-dir`, `--exec-path`, …) can
        // select another configuration or program, so none qualify.
        "git" => {
            matches!(first, Some("fetch" | "pull" | "clone"))
                && workspace.join(".git").exists()
                && after(1).is_some_and(|words| match words.split_first() {
                    // The rest are refspecs or the clone's directory.
                    Some((repository, rest)) => {
                        remote(repository, workspace)
                            && rest.iter().all(|word| !word.contains("::"))
                    }
                    None => true,
                })
        }
        _ => false,
    }
}

/// The allowed options of `program`'s download subcommand.
fn options(program: &str) -> &'static Options {
    const NONE: Options = Options {
        flags: &[],
        valued: &[],
    };
    match program {
        "cargo" => &CARGO_FETCH,
        "go" => &GO_MOD_DOWNLOAD,
        "npm" | "pnpm" => &NPM_INSTALL,
        "brew" => &BREW_FETCH,
        "git" => &GIT_FETCH,
        _ => &NONE,
    }
}

/// The positional arguments, when every option is allowed; anything after
/// `--` is positional.
fn positionals<'a>(args: &'a [String], options: &Options) -> Option<Vec<&'a str>> {
    let mut output = Vec::new();
    let mut words = args.iter().map(String::as_str);
    let mut only_positional = false;
    while let Some(word) = words.next() {
        let valued = |name: &str| options.valued.contains(&name);
        if only_positional || !word.starts_with('-') || word == "-" {
            output.push(word);
        } else if word == "--" {
            only_positional = true;
        } else if let Some((name, _)) = word.split_once('=') {
            valued(name).then_some(())?;
        } else if valued(word) {
            words.next()?;
        } else if !options.flags.contains(&word) {
            return None;
        }
    }
    Some(output)
}

/// A repository git may contact for a routine fetch: a network URL or the
/// name of a configured remote. A local path or `file://` URL would run
/// `upload-pack` against a repository the model may have crafted, and a
/// `<helper>::` address runs the helper it names.
fn remote(word: &str, workspace: &Path) -> bool {
    if let Some((scheme, rest)) = word.split_once("://") {
        return matches!(scheme, "https" | "http" | "ssh" | "git") && !rest.starts_with('-');
    }
    if word.contains("::") {
        return false;
    }
    // `user@host:path`; a one-letter "host" is a Windows drive.
    if let Some((host, _)) = word.split_once(':') {
        return host.len() > 1 && !host.contains(['/', '\\']) && !host.starts_with('-');
    }
    // Git takes a name that is no configured remote as a path.
    !word.is_empty()
        && !word.contains(['/', '\\'])
        && !word.starts_with(['.', '-', '~'])
        && !workspace.join(word).exists()
}

/// A package from the configured registry (`name`, `name@range`,
/// `@scope/name@range`). Git, URL, file and GitHub `owner/repo` specs fetch
/// and build code from elsewhere, so they ask.
fn registry_package(spec: &str) -> bool {
    let slashes = spec.matches('/').count();
    !spec.is_empty()
        && !spec.contains(':')
        && !spec.starts_with(['.', '~', '-'])
        && (slashes == 0 || (slashes == 1 && spec.starts_with('@')))
}

/// A formula or cask from an installed tap. A path or a `user/tap/name`
/// reference loads Ruby code from elsewhere.
fn formula_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\', ':'])
        && !name.starts_with(['.', '-'])
        && !name.ends_with(".rb")
}

fn harmless(words: &[String]) -> bool {
    words
        .first()
        .is_some_and(|program| matches!(program.as_str(), "echo" | "pwd" | "true" | "ls"))
}

/// Whether the arguments of a connector tool the server calls read-only are
/// a small lookup that carries no file contents and no credentials. The
/// server's own hint is untrusted, so anything larger or secret-looking asks.
pub(crate) fn lookup_arguments(arguments: &Value) -> bool {
    const LOOKUP_BYTES: usize = 2048;
    const LOOKUP_STRING_CHARS: usize = 512;
    const CREDENTIAL_MARKERS: &[&str] = &[
        "-----BEGIN",
        "sk-",
        "sk_live_",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "glpat-",
        "xoxb-",
        "xoxp-",
        "AKIA",
        "ASIA",
        "AIza",
        "Bearer ",
        "Basic ",
        "eyJ",
        "password",
        "passwd",
        "secret",
        "token",
        "api_key",
        "apikey",
    ];
    fn clean(value: &Value) -> bool {
        match value {
            Value::String(text) => {
                let lower = text.to_lowercase();
                text.chars().count() <= LOOKUP_STRING_CHARS
                    && text.lines().count() <= 3
                    && !CREDENTIAL_MARKERS.iter().any(|marker| {
                        text.contains(marker) || lower.contains(&marker.to_lowercase())
                    })
                    && secret_word(text).is_none()
            }
            Value::Array(items) => items.iter().all(clean),
            Value::Object(map) => map.iter().all(|(key, item)| {
                let key = key.to_lowercase();
                !CREDENTIAL_MARKERS
                    .iter()
                    .any(|marker| key.contains(&marker.to_lowercase()))
                    && clean(item)
            }),
            _ => true,
        }
    }
    serde_json::to_vec(arguments).is_ok_and(|bytes| bytes.len() <= LOOKUP_BYTES) && clean(arguments)
}

/// `requested` relative to the project, with symlinks resolved; `None` when
/// it is outside the project.
pub(crate) fn project_relative(workspace: &Path, requested: &str) -> Option<String> {
    resolve(workspace, requested)
        .strip_prefix(real_path(workspace))
        .ok()
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
}

/// Assesses a shell command line.
fn command(line: &str, workspace: &Path) -> Option<String> {
    command_line(line, workspace, 0)
}

const MAX_DEPTH: usize = 4;

fn command_line(line: &str, workspace: &Path, depth: usize) -> Option<String> {
    if depth > MAX_DEPTH {
        return Some("Runs deeply nested shell code".into());
    }
    let segments = segments(line);
    // Downloads piped into an interpreter run unreviewed remote code.
    for pair in segments.windows(2) {
        let fetches =
            first_word(&pair[0].0).is_some_and(|word| matches!(word.as_str(), "curl" | "wget"));
        let runs =
            first_word(&pair[1].0).is_some_and(|word| is_shell(&word) || is_interpreter(&word));
        if pair[0].1 == Some('|') && fetches && runs {
            return Some("Runs code downloaded from the network".into());
        }
    }
    if let Some(reason) = redirect_outside(line, workspace) {
        return Some(reason);
    }
    for inner in substitutions(line) {
        if let Some(reason) = command_line(inner, workspace, depth + 1) {
            return Some(reason);
        }
    }
    for (segment, _) in segments {
        let words = shell_words::split(&segment)
            .unwrap_or_else(|_| segment.split_whitespace().map(str::to_owned).collect());
        if let Some(reason) = command_words(&words, workspace, depth) {
            return Some(reason);
        }
    }
    None
}

/// The bodies of `$(...)` and backtick command substitutions.
fn substitutions(line: &str) -> Vec<&str> {
    let mut output = Vec::new();
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'$' && bytes.get(index + 1) == Some(&b'(') {
            let start = index + 2;
            let mut depth = 1;
            let mut end = start;
            while end < bytes.len() && depth > 0 {
                match bytes[end] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                end += 1;
            }
            output.push(&line[start..end.saturating_sub(usize::from(depth == 0)).max(start)]);
            index = end;
        } else if bytes[index] == b'`' {
            let start = index + 1;
            let end = line[start..]
                .find('`')
                .map_or(line.len(), |offset| start + offset);
            output.push(&line[start..end]);
            index = end + 1;
        } else {
            index += 1;
        }
    }
    output
}

/// Splits a command line on unquoted `;`, `|`, `&` and newlines, returning
/// each simple command with the separator that ends it (`|` for a pipe).
fn segments(line: &str) -> Vec<(String, Option<char>)> {
    let mut output = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                current.push(c);
            }
            (Some(_), c) => current.push(c),
            (None, '\\') => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            (None, '\'' | '"') => {
                quote = Some(c);
                current.push(c);
            }
            (None, ';' | '\n' | '|' | '&') => {
                let doubled = chars.peek() == Some(&c);
                if doubled {
                    chars.next();
                }
                let separator = (c == '|' && !doubled).then_some('|');
                let segment = std::mem::take(&mut current);
                if !segment.trim().is_empty() {
                    output.push((segment.trim().to_owned(), separator));
                }
            }
            (None, c) => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        output.push((current.trim().to_owned(), None));
    }
    output
}

fn first_word(stage: &str) -> Option<String> {
    let words = shell_words::split(stage).ok()?;
    let index = command_start(&words)?;
    Some(base(&words[index]).to_owned())
}

/// The index of the actual program after environment assignments and
/// transparent wrappers such as `env`, `nohup` or `xargs`.
fn command_start(words: &[String]) -> Option<usize> {
    let mut index = 0;
    while index < words.len() {
        let word = &words[index];
        let name = base(word);
        if word.contains('=') && !word.starts_with('-') && !word.starts_with('=') {
            index += 1;
        } else if matches!(
            name,
            "env" | "nohup" | "time" | "nice" | "command" | "exec" | "xargs" | "timeout" | "stdbuf"
        ) {
            index += 1;
            // Skip the wrapper's own options and a timeout duration.
            while index < words.len()
                && (words[index].starts_with('-')
                    || (name == "timeout"
                        && words[index]
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_digit())))
            {
                index += 1;
            }
        } else {
            return Some(index);
        }
    }
    None
}

fn base(word: &str) -> &str {
    Path::new(word)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(word)
}

fn is_shell(word: &str) -> bool {
    matches!(word, "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh")
}

fn is_interpreter(word: &str) -> bool {
    word.starts_with("python") || matches!(word, "node" | "deno" | "bun" | "ruby" | "perl" | "php")
}

fn command_words(words: &[String], workspace: &Path, depth: usize) -> Option<String> {
    let start = command_start(words)?;
    let program = base(&words[start]);
    let args = &words[start + 1..];
    let has = |flag: &str| args.iter().any(|arg| arg == flag);
    let short = |letter: char| {
        args.iter()
            .any(|arg| arg.starts_with('-') && !arg.starts_with("--") && arg.contains(letter))
    };
    for word in words {
        if let Some(kind) = secret_word(word) {
            return Some(format!("Accesses {kind}"));
        }
    }
    let reason = match program {
        "sudo" | "doas" | "su" | "pkexec" => Some("Runs with elevated privileges"),
        "rm" if short('r') || short('R') || short('f') || has("--recursive") || has("--force") => {
            Some("Deletes files recursively or forcibly")
        }
        "find" if has("-delete") || (has("-exec") && args.iter().any(|arg| base(arg) == "rm")) => {
            Some("Deletes files found by a search")
        }
        "dd" => Some("Writes raw data to a file or device"),
        "shred" | "wipefs" | "fdisk" | "parted" | "sfdisk" | "gdisk" => {
            Some("Erases or repartitions storage")
        }
        name if name.starts_with("mkfs") => Some("Formats a file system"),
        "diskutil"
            if args
                .iter()
                .any(|arg| arg.starts_with("erase") || arg == "partitionDisk") =>
        {
            Some("Erases or repartitions storage")
        }
        "chmod" | "chown" | "chgrp" if short('R') || has("--recursive") => {
            Some("Changes permissions recursively")
        }
        "shutdown" | "reboot" | "halt" | "poweroff" => Some("Shuts down or restarts the computer"),
        "security"
            if args.first().is_some_and(|arg| {
                arg.starts_with("find-") || arg.starts_with("dump-") || arg.starts_with("export")
            }) =>
        {
            Some("Reads secrets from the keychain")
        }
        "git" => git(args),
        "curl"
            if args.iter().any(|arg| {
                matches!(
                    arg.as_str(),
                    "-T" | "--upload-file"
                        | "-F"
                        | "--form"
                        | "-d"
                        | "--data"
                        | "--data-binary"
                        | "--data-raw"
                        | "--data-urlencode"
                        | "--json"
                ) || arg.starts_with("--data")
                    || arg.starts_with("-d")
                    || arg.starts_with("-F")
            }) || args.windows(2).any(|pair| {
                pair[0] == "-X"
                    && matches!(
                        pair[1].to_uppercase().as_str(),
                        "POST" | "PUT" | "PATCH" | "DELETE"
                    )
            }) =>
        {
            Some("Uploads data over the network")
        }
        "wget" if args.iter().any(|arg| arg.starts_with("--post")) => {
            Some("Uploads data over the network")
        }
        "scp" | "sftp" | "ftp" | "nc" | "ncat" | "netcat" | "socat" => {
            Some("Transfers data over the network")
        }
        "rsync"
            if args
                .iter()
                .any(|arg| !arg.starts_with('-') && arg.contains(':')) =>
        {
            Some("Transfers data over the network")
        }
        "npm" | "pnpm" | "yarn" | "cargo" | "gem" | "dotnet" if has("publish") || has("push") => {
            Some("Publishes a package")
        }
        "twine" if has("upload") => Some("Publishes a package"),
        "docker" | "podman" if has("push") => Some("Publishes a container image"),
        "gh" if args
            .first()
            .is_some_and(|arg| matches!(arg.as_str(), "release" | "repo" | "api" | "secret")) =>
        {
            Some("Changes a GitHub repository or its settings")
        }
        "crontab" | "launchctl" | "at" | "systemctl" => {
            Some("Changes background services or scheduled jobs")
        }
        shell if is_shell(shell) => {
            return args
                .windows(2)
                .find(|pair| pair[0] == "-c" || pair[0].ends_with('c') && pair[0].starts_with('-'))
                .and_then(|pair| command_line(&pair[1], workspace, depth + 1));
        }
        _ => None,
    };
    if let Some(reason) = reason {
        return Some(reason.into());
    }
    if matches!(
        program,
        "rm" | "mv"
            | "cp"
            | "tee"
            | "touch"
            | "mkdir"
            | "ln"
            | "chmod"
            | "chown"
            | "truncate"
            | "install"
            | "rmdir"
            | "unlink"
    ) {
        for arg in args.iter().filter(|arg| !arg.starts_with('-')) {
            if let Some(path) = absolute(arg)
                && !inside(workspace, &path)
                && !scratch(&path)
            {
                return Some(format!(
                    "Changes files outside the project: {}",
                    path.display()
                ));
            }
        }
    }
    None
}

/// Git global options that take a separate value (`git -C <path> …`).
const GIT_OPTIONS_WITH_VALUE: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--super-prefix",
    "--config-env",
    "--list-cmds",
];

/// The git subcommand and its arguments, after global options such as
/// `-C <path>`, `-c <key=value>`, `--git-dir=<dir>`, `-P` or `--no-pager`.
fn git_subcommand(args: &[String]) -> Option<(&str, &[String])> {
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if GIT_OPTIONS_WITH_VALUE.contains(&arg) {
            index += 2;
        } else if arg.starts_with('-') {
            index += 1;
        } else {
            return Some((arg, &args[index + 1..]));
        }
    }
    None
}

fn git(args: &[String]) -> Option<&'static str> {
    let (subcommand, rest) = git_subcommand(args)?;
    let has = |flag: &str| rest.iter().any(|arg| arg == flag);
    let short = |letter: char| {
        rest.iter()
            .any(|arg| arg.starts_with('-') && !arg.starts_with("--") && arg.contains(letter))
    };
    match subcommand {
        "push"
            if has("--force")
                || has("-f")
                || rest
                    .iter()
                    .any(|arg| arg.starts_with("--force") || arg.starts_with('+')) =>
        {
            Some("Force-pushes and can overwrite remote history")
        }
        "push" if has("--delete") || has("-d") || rest.iter().any(|arg| arg.starts_with(':')) => {
            Some("Deletes a remote branch or tag")
        }
        "push" => Some("Pushes commits to a remote"),
        "reset" if has("--hard") => Some("Discards uncommitted changes"),
        "clean" if short('f') || has("--force") => Some("Deletes untracked files"),
        "checkout" | "restore" if has("--") || has(".") || has("--worktree") => {
            Some("Discards uncommitted changes")
        }
        "branch" if short('D') || (has("--delete") && has("--force")) => {
            Some("Force-deletes a branch")
        }
        "stash"
            if rest
                .first()
                .is_some_and(|arg| matches!(arg.as_str(), "drop" | "clear")) =>
        {
            Some("Discards stashed changes")
        }
        "filter-branch" | "filter-repo" => Some("Rewrites repository history"),
        "update-ref" if has("-d") => Some("Deletes a reference"),
        "reflog"
            if rest
                .first()
                .is_some_and(|arg| arg == "expire" || arg == "delete") =>
        {
            Some("Discards recovery history")
        }
        "gc" if rest.iter().any(|arg| arg.starts_with("--prune")) => {
            Some("Discards recovery history")
        }
        _ => None,
    }
}

/// Output redirections to files outside the project.
fn redirect_outside(line: &str, workspace: &Path) -> Option<String> {
    let mut rest = line;
    while let Some(index) = rest.find('>') {
        let after = rest[index + 1..]
            .trim_start_matches(['>', '|', '&'])
            .trim_start();
        let target: String = after
            .chars()
            .take_while(|c| !c.is_whitespace() && !matches!(c, ';' | '|' | '&' | ')'))
            .collect();
        let target = target.trim_matches(['"', '\'']);
        if let Some(path) = absolute(target)
            && !inside(workspace, &path)
            && !scratch(&path)
        {
            return Some(format!(
                "Writes a file outside the project: {}",
                path.display()
            ));
        }
        rest = &rest[index + 1..];
    }
    None
}

/// Home-relative or absolute paths in command words, symlinks resolved.
fn absolute(word: &str) -> Option<PathBuf> {
    if let Some(rest) = word.strip_prefix("~/") {
        return home().map(|home| real_path(&home.join(rest)));
    }
    if word == "~" {
        return home().map(|home| real_path(&home));
    }
    let path = Path::new(word);
    path.is_absolute().then(|| real_path(path))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Lexical normalization: `..` and `.` resolved without touching the disk.
fn normalize(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                output.pop();
            }
            Component::CurDir => {}
            other => output.push(other),
        }
    }
    output
}

fn resolve(workspace: &Path, requested: &str) -> PathBuf {
    absolute(requested).unwrap_or_else(|| real_path(&workspace.join(requested)))
}

/// The path with every symlink resolved: the nearest existing ancestor is
/// canonicalized and the components that do not exist yet are appended, so a
/// link cannot hide where a new file would land. Never a purely lexical guess.
fn real_path(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(existing) {
            let mut output = real;
            for component in missing.iter().rev() {
                match component {
                    Component::ParentDir => {
                        output.pop();
                    }
                    Component::CurDir => {}
                    other => output.push(other),
                }
            }
            return output;
        }
        match (existing.parent(), existing.components().next_back()) {
            (Some(parent), Some(last)) => {
                missing.push(last);
                existing = parent;
            }
            // Nothing exists (not even the root): keep the lexical form,
            // which the caller compares against a real project root.
            _ => return normalize(path),
        }
    }
}

fn inside(workspace: &Path, path: &Path) -> bool {
    real_path(path).starts_with(real_path(workspace))
}

/// Scratch locations whose contents are disposable.
fn scratch(path: &Path) -> bool {
    path == Path::new("/dev/null")
        || path.starts_with("/tmp")
        || path.starts_with("/private/tmp")
        || path.starts_with(real_path(&std::env::temp_dir()))
}

/// Files whose contents are credentials or secrets.
fn secret_path(path: &Path) -> Option<&'static str> {
    let text = path.to_string_lossy().replace('\\', "/");
    secret_word(&text)
}

fn secret_word(word: &str) -> Option<&'static str> {
    const DIRECTORIES: &[(&str, &str)] = &[
        ("/.ssh", "SSH keys"),
        ("~/.ssh", "SSH keys"),
        ("/.gnupg", "GPG keys"),
        ("/.aws", "cloud credentials"),
        ("/.azure", "cloud credentials"),
        ("/.config/gcloud", "cloud credentials"),
        ("/.kube", "cluster credentials"),
        ("/.docker/config.json", "registry credentials"),
        ("/.config/gh", "GitHub credentials"),
        ("/Library/Keychains", "the keychain"),
        (
            "/Library/Application Support/Google/Chrome",
            "a browser profile",
        ),
        ("/Library/Application Support/Firefox", "a browser profile"),
        (
            "/Library/Application Support/Microsoft Edge",
            "a browser profile",
        ),
        (
            "/Library/Application Support/BraveSoftware",
            "a browser profile",
        ),
        ("/Library/Safari", "a browser profile"),
        ("/Library/Cookies", "browser cookies"),
        ("/.mozilla", "a browser profile"),
        ("/.config/google-chrome", "a browser profile"),
        ("/.config/chromium", "a browser profile"),
        ("/.config/BraveSoftware", "a browser profile"),
        ("/.local/share/keyrings", "the keyring"),
        ("/.password-store", "the password store"),
        ("/etc/shadow", "system passwords"),
        ("/etc/sudoers", "system privileges"),
    ];
    for (marker, kind) in DIRECTORIES {
        if word.contains(marker) {
            return Some(kind);
        }
    }
    let name = word.rsplit('/').next().unwrap_or(word);
    let secret_file = name == ".env"
        || (name.starts_with(".env.") && !name.ends_with(".example") && !name.ends_with(".sample"))
        || matches!(
            name,
            ".netrc" | ".npmrc" | ".pypirc" | ".git-credentials" | "credentials.json"
        )
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("id_ecdsa")
        || name.ends_with(".pem")
        || name.ends_with(".p12")
        || name.ends_with(".key");
    secret_file.then_some("a secrets file")
}

/// Project files that make tools run code automatically later.
fn auto_run_config(workspace: &Path, path: &Path) -> Option<String> {
    let relative = path
        .strip_prefix(real_path(workspace))
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    let automatic = relative.starts_with(".git/")
        || relative == ".cargo/config.toml"
        || relative == ".cargo/config"
        || relative == ".vscode/tasks.json"
        || relative == ".envrc"
        || relative.starts_with(".husky/")
        || relative.starts_with(".github/workflows/");
    automatic.then(|| format!("Changes {relative}, which can run code automatically"))
}

#[cfg(test)]
mod tests;
