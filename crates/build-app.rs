//! Optional convenience CLI. Ordinary `cargo build` uses the same Rust builder
//! through the desktop crate's default `bundled-browser` feature.
use anyhow::{Result, bail};
use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};
use xylo_build::{
    browser::{self, Profile},
    bundle, run,
};

#[derive(Debug, PartialEq, Eq)]
enum Action {
    App(Profile),
    Browser {
        profile: Profile,
        target: Option<String>,
    },
    Bundle {
        profile: Profile,
        skip_build: bool,
    },
}

fn parse(args: &[String]) -> Result<Action> {
    if args.is_empty() {
        return Ok(Action::App(Profile::Debug));
    }
    if args == ["--release"] {
        return Ok(Action::App(Profile::Release));
    }
    let Some(command) = args.first().map(String::as_str) else {
        unreachable!()
    };
    let mut profile = Profile::Release;
    let mut target = None;
    let mut skip_build = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--profile" if index + 1 < args.len() => {
                index += 1;
                profile = Profile::parse(&args[index])?;
            }
            "--target" if command == "browser" && index + 1 < args.len() => {
                index += 1;
                target = Some(args[index].clone());
            }
            "--skip-build" if command == "bundle-macos" => skip_build = true,
            _ => bail!(
                "usage: cargo app [--release] | cargo run -p xylo-build -- browser [--profile debug|release] [--target NATIVE_TARGET] | cargo bundle-macos [--profile debug|release] [--skip-build]"
            ),
        }
        index += 1;
    }
    match command {
        "browser" => Ok(Action::Browser { profile, target }),
        "bundle-macos" => Ok(Action::Bundle {
            profile,
            skip_build,
        }),
        _ => bail!("unknown build command: {command}"),
    }
}

fn cargo_args(profile: Profile) -> Vec<&'static str> {
    let mut args = vec![
        "build",
        "--locked",
        "-p",
        "xylo",
        "-p",
        "xylo-worker",
        "-p",
        "xylo-helper",
        "-p",
        "xylo-cli",
        "--bins",
    ];
    if profile == Profile::Release {
        args.push("--release");
    }
    args
}

fn build() -> Result<()> {
    let action = parse(&env::args().skip(1).collect::<Vec<_>>())?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    match action {
        Action::App(profile) => run(Command::new(cargo)
            .args(cargo_args(profile))
            .current_dir(root)),
        Action::Browser { profile, target } => {
            let host = browser::host(root)?;
            let target = target.as_deref().unwrap_or(&host);
            let target_dir = env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| root.join("target"));
            let target_dir = if target_dir.is_absolute() {
                target_dir
            } else {
                root.join(target_dir)
            };
            browser::build(
                root,
                &target_dir.join(profile.name()),
                profile,
                target,
                &host,
            )
        }
        Action::Bundle {
            profile,
            skip_build,
        } => bundle::build(root, profile, skip_build),
    }
}

fn main() -> ExitCode {
    match build() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xylo-build: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).into()).collect()
    }
    #[test]
    fn defaults_and_explicit_commands() {
        assert_eq!(parse(&[]).unwrap(), Action::App(Profile::Debug));
        assert_eq!(
            parse(&args(&["--release"])).unwrap(),
            Action::App(Profile::Release)
        );
        assert_eq!(
            parse(&args(&[
                "browser",
                "--profile",
                "debug",
                "--target",
                "native"
            ]))
            .unwrap(),
            Action::Browser {
                profile: Profile::Debug,
                target: Some("native".into())
            }
        );
        assert_eq!(
            parse(&args(&["bundle-macos", "--skip-build"])).unwrap(),
            Action::Bundle {
                profile: Profile::Release,
                skip_build: true
            }
        );
        for invalid in [
            &["--debug"][..],
            &["--release", "--release"],
            &["browser", "--profile"],
            &["browser", "--skip-build"],
            &["bundle-macos", "--target", "native"],
            &["unknown"],
        ] {
            assert!(parse(&args(invalid)).is_err());
        }
    }
    #[test]
    fn application_build_uses_cargo_defaults_and_only_required_binaries() {
        assert_eq!(
            cargo_args(Profile::Debug),
            [
                "build",
                "--locked",
                "-p",
                "xylo",
                "-p",
                "xylo-worker",
                "-p",
                "xylo-helper",
                "-p",
                "xylo-cli",
                "--bins"
            ]
        );
        assert_eq!(cargo_args(Profile::Release).last(), Some(&"--release"));
    }
}
