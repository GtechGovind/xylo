//! Headless Xylo frontend. Stdout contains only the requested output format.
mod options;
mod session;
use anyhow::{Context, Result};
use options::Options;
use tokio::io::AsyncReadExt;
use xylo_runtime::{AppPaths, headless::Profile, observability::Observability};

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let status = match execute().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("xylo-cli: {error:#}");
            1
        }
    };
    std::process::exit(status);
}
async fn execute() -> Result<i32> {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if arguments == ["--help"] || arguments == ["-h"] {
        print!("{}", options::HELP);
        return Ok(0);
    }
    if arguments == ["--version"] {
        println!(
            "xylo-cli {} (stdio protocol {})",
            env!("CARGO_PKG_VERSION"),
            xylo_core::stdio::VERSION
        );
        return Ok(0);
    }
    let mut options = Options::parse(arguments)?;
    if !options.stream_input && options.prompt.is_none() {
        let mut text = String::new();
        tokio::io::stdin()
            .take((xylo_core::MAX_PROTOCOL_BYTES + 1) as u64)
            .read_to_string(&mut text)
            .await
            .context("Read prompt from stdin")?;
        anyhow::ensure!(
            text.len() <= xylo_core::MAX_PROTOCOL_BYTES,
            "Prompt exceeds input limit"
        );
        options.prompt = Some(text);
    }
    let paths = match &options.data {
        Some(data) => AppPaths::at(if data.is_absolute() {
            data.clone()
        } else {
            std::env::current_dir()?.join(data)
        })?,
        None => AppPaths::discover()?,
    };
    let telemetry = Observability::init(&paths.data, options.debug)?;
    let profile = Profile::open(paths)?;
    let code = session::serve(&profile, &options, &telemetry).await?;
    drop(profile);
    drop(telemetry);
    Ok(code)
}
