//! Verify a cargo-packager artifact signature without loading the artifact into memory.
use anyhow::{Context, Result, ensure};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use minisign_verify::{PublicKey, Signature};
use std::{fs, io::Read};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    ensure!(
        args.len() == 3,
        "Usage: xylo-signature-verify PUBLIC_KEY SIGNATURE ARTIFACT"
    );
    let public = STANDARD.decode(fs::read_to_string(&args[0])?.trim())?;
    let signature = STANDARD.decode(fs::read_to_string(&args[1])?.trim())?;
    let key = PublicKey::decode(std::str::from_utf8(&public)?)?;
    let signature = Signature::decode(std::str::from_utf8(&signature)?)?;
    let mut verifier = key.verify_stream(&signature)?;
    let mut file = fs::File::open(&args[2])?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        verifier.update(&buffer[..count]);
    }
    verifier
        .finalize()
        .context("Artifact signature is invalid")?;
    println!("Verified artifact signature");
    Ok(())
}
