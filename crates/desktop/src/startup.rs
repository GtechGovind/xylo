//! Startup options are resolved before any profile, service or native window opens.
use anyhow::{Context, Result, bail, ensure};
use std::{ffi::OsString, path::PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Version,
    Help,
    Launch { data: Option<PathBuf> },
}

impl Command {
    pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self> {
        let mut arguments = arguments.into_iter();
        let Some(option) = arguments.next() else {
            return Ok(Self::Launch { data: None });
        };
        let command = match option.to_str() {
            Some("--version") => Self::Version,
            Some("--help" | "-h") => Self::Help,
            Some("--data-dir") => {
                let data = PathBuf::from(arguments.next().context("--data-dir requires a path")?);
                ensure!(data.is_absolute(), "--data-dir requires an absolute path");
                Self::Launch { data: Some(data) }
            }
            _ => bail!("Unknown startup option; use --help"),
        };
        ensure!(
            arguments.next().is_none(),
            "Unexpected startup arguments; use --help"
        );
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_launch_uses_profile_discovery_and_information_commands_do_not_launch() {
        assert_eq!(Command::parse([]).unwrap(), Command::Launch { data: None });
        assert_eq!(
            Command::parse(["--version".into()]).unwrap(),
            Command::Version
        );
        assert_eq!(Command::parse(["--help".into()]).unwrap(), Command::Help);
    }

    #[test]
    fn explicit_profiles_require_one_absolute_path_without_creating_it() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("isolated profile");
        assert_eq!(
            Command::parse(["--data-dir".into(), data.clone().into_os_string()]).unwrap(),
            Command::Launch {
                data: Some(data.clone())
            }
        );
        assert!(
            !data.exists(),
            "Parsing must never create or open a profile"
        );
        for arguments in [
            vec!["--data-dir"],
            vec!["--data-dir", "relative/path"],
            vec!["--version", "--data-dir"],
            vec!["--help", "unexpected"],
            vec!["--unknown"],
        ] {
            assert!(Command::parse(arguments.into_iter().map(OsString::from)).is_err());
        }
    }
}
