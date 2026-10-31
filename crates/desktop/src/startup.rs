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
#[path = "startup/tests.rs"]
mod tests;
