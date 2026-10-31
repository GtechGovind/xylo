//! Reads the parts of `~/.ssh/config` and `~/.ssh/known_hosts` the
//! Environments form needs to prefill and warn. Nothing here decides trust:
//! `ssh` still verifies host keys itself.
use std::path::{Path, PathBuf};

/// One concrete `Host` alias with the settings the form can use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct SshHost {
    pub alias: String,
    pub host_name: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<PathBuf>,
}
impl SshHost {
    /// The address to connect to: `HostName` when set, otherwise the alias.
    pub fn address(&self) -> &str {
        self.host_name.as_deref().unwrap_or(&self.alias)
    }
}

/// Parses `Host` blocks. Wildcard and negated patterns are skipped, `Match`
/// blocks end the current hosts, and the first value of a keyword wins, as in
/// `ssh` itself. `~/` in `IdentityFile` expands against `home`.
pub(super) fn parse_config(text: &str, home: &Path) -> Vec<SshHost> {
    let mut hosts: Vec<SshHost> = Vec::new();
    // Indices of the hosts the current block applies to.
    let mut current: Vec<usize> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, value) = match line.split_once(|c: char| c == '=' || c.is_whitespace()) {
            Some((keyword, value)) => (keyword, value.trim().trim_start_matches('=').trim()),
            None => (line, ""),
        };
        let value = value.trim_matches('"');
        match keyword.to_ascii_lowercase().as_str() {
            "host" => {
                current.clear();
                for alias in value.split_whitespace() {
                    if alias.contains(['*', '?', '!']) {
                        continue;
                    }
                    current.push(hosts.len());
                    hosts.push(SshHost {
                        alias: alias.to_owned(),
                        ..SshHost::default()
                    });
                }
            }
            "match" => current.clear(),
            "hostname" => apply(&mut hosts, &current, |host| {
                host.host_name.get_or_insert_with(|| value.to_owned());
            }),
            "user" => apply(&mut hosts, &current, |host| {
                host.user.get_or_insert_with(|| value.to_owned());
            }),
            "port" => {
                if let Ok(port) = value.parse::<u16>() {
                    apply(&mut hosts, &current, |host| {
                        host.port.get_or_insert(port);
                    });
                }
            }
            "identityfile" => {
                let path = match value.strip_prefix("~/") {
                    Some(rest) => home.join(rest),
                    None => PathBuf::from(value),
                };
                apply(&mut hosts, &current, |host| {
                    host.identity_file.get_or_insert_with(|| path.clone());
                });
            }
            _ => {}
        }
    }
    hosts
}
fn apply(hosts: &mut [SshHost], current: &[usize], mut change: impl FnMut(&mut SshHost)) {
    for index in current {
        change(&mut hosts[*index]);
    }
}

/// `~/.ssh/known_hosts` when it exists: the default trusted-hosts file.
pub(super) fn default_known_hosts(home: &Path) -> Option<PathBuf> {
    let path = home.join(".ssh").join("known_hosts");
    path.is_file().then_some(path)
}

/// Whether `known_hosts` lists `host` (and `port`). `None` when the file
/// cannot tell: it has hashed entries and no plain match.
pub(super) fn known_host(known_hosts: &str, host: &str, port: u16) -> Option<bool> {
    let wanted = if port == 22 {
        host.to_owned()
    } else {
        format!("[{host}]:{port}")
    };
    let mut hashed = false;
    for line in known_hosts.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let mut names = fields.next().unwrap_or("");
        if names.starts_with('@') {
            names = fields.next().unwrap_or("");
        }
        if names.starts_with('|') {
            hashed = true;
            continue;
        }
        if names
            .split(',')
            .any(|name| name.eq_ignore_ascii_case(&wanted))
        {
            return Some(true);
        }
    }
    if hashed { None } else { Some(false) }
}

/// The user's home directory.
pub(super) fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[cfg(test)]
#[path = "ssh_config/tests.rs"]
mod tests;
