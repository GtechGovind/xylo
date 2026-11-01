//! Startup options are resolved before any profile, service or native window opens.
use anyhow::{Context, Result, bail, ensure};
use std::{ffi::OsString, path::PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Version,
    Help,
    Launch { data: Option<PathBuf>, debug: bool },
}
impl Command {
    pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self> {
        let arguments = arguments.into_iter().collect::<Vec<_>>();
        if arguments == [OsString::from("--version")] {
            return Ok(Self::Version);
        }
        if arguments == [OsString::from("--help")] || arguments == [OsString::from("-h")] {
            return Ok(Self::Help);
        }
        let mut arguments = arguments.into_iter();
        let mut data = None;
        let mut debug = false;
        while let Some(option) = arguments.next() {
            match option.to_str() {
                Some("--debug") if !debug => debug = true,
                Some("--data-dir") if data.is_none() => {
                    let path =
                        PathBuf::from(arguments.next().context("--data-dir requires a path")?);
                    ensure!(path.is_absolute(), "--data-dir requires an absolute path");
                    data = Some(path);
                }
                _ => bail!("Unknown or duplicate startup option; use --help"),
            }
        }
        Ok(Self::Launch { data, debug })
    }
}
#[cfg(test)]
#[path = "startup/tests.rs"]
mod tests;
