//! Native capability process. Stdout is protocol only; diagnostics use stderr.

mod server;

use anyhow::{Context, Result, ensure};
use std::{path::PathBuf, time::Duration};

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() == 1 && args[0] == "--version" {
        println!("Xylo native helper {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    ensure!(
        args.len() == 4 && args[0] == "--data-dir" && args[2] == "--workspace",
        "Usage: xylo-helper --data-dir ABSOLUTE_PATH --workspace ABSOLUTE_PATH"
    );
    let data = PathBuf::from(&args[1]);
    let workspace = PathBuf::from(&args[3]);
    ensure!(
        data.is_absolute() && workspace.is_absolute(),
        "Helper roots must be absolute"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Create helper runtime")?;
    let result = runtime.block_on(server::serve(data, workspace));
    // Native parsers may be inside a noninterruptible call. The process boundary
    // lets application shutdown release those threads instead of hanging the UI.
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}
