//! cargo-packager uses Base64 of the complete Minisign key/signature files.
use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use minisign_verify::{PublicKey, Signature};
use std::{fs, io::Read, path::Path};

// Compile the repository trust anchor into this standalone tool. Downloaded keys
// are only checked for equality and are never used to authenticate anything.
pub const PINNED_PUBLIC: &str = include_str!("../../../.github/release-signing.pub");
pub const PUBLIC_FILENAME: &str = "Xylo-release-signing.pub";

pub fn verify(artifact: &Path) -> Result<()> {
    verify_with_public(PINNED_PUBLIC, artifact).with_context(|| {
        format!(
            "Project artifact signature verification: {}",
            artifact.display()
        )
    })
}

fn verify_with_public(public: &str, artifact: &Path) -> Result<()> {
    let public = STANDARD
        .decode(public.trim())
        .context("Invalid public key Base64")?;
    let signature = STANDARD
        .decode(fs::read_to_string(crate::util::sidecar(artifact))?.trim())
        .context("Invalid signature Base64")?;
    let key = PublicKey::decode(std::str::from_utf8(&public)?)?;
    let signature = Signature::decode(std::str::from_utf8(&signature)?)?;
    let mut verifier = key.verify_stream(&signature)?;
    let mut file = fs::File::open(artifact)?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        verifier.update(&buffer[..count]);
    }
    verifier.finalize().context("Artifact signature is invalid")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Public, fixed prehashed test vector from minisign-verify 0.2.5's
    // src/lib.rs documentation. No private key or signing dependency is needed.
    const PUBLIC: &str = "untrusted comment: test vector public key\nRWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3\n";
    const SIGNATURE: &str = "untrusted comment: signature from minisign secret key\nRUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=\ntrusted comment: timestamp:1633700835\tfile:test\tprehashed\nwLMDjy9FLAuxZ3q4NlEvkgtyhrr0gtTu6KC4KBJdITbbOeAi1zBIYo0v4iTgt8jJpIidRJnp94ABQkJAgAooBQ==";

    #[test]
    fn real_signature_accepts_vector_rejects_tampering_wrong_key_and_bad_encoding() {
        let temporary = tempfile::tempdir().unwrap();
        let artifact = temporary.path().join("test");
        let public = STANDARD.encode(PUBLIC);
        fs::write(&artifact, b"test").unwrap();
        fs::write(crate::util::sidecar(&artifact), STANDARD.encode(SIGNATURE)).unwrap();
        verify_with_public(&public, &artifact).unwrap();
        // The fixed vector is NOT signed by the repository release key.
        assert!(verify(&artifact).is_err());
        fs::write(&artifact, b"tampered").unwrap();
        assert!(verify_with_public(&public, &artifact).is_err());
        fs::write(&artifact, b"test").unwrap();
        for signature in [
            SIGNATURE.replace("timestamp:1633700835", "timestamp:1633700836"),
            SIGNATURE.replace("RUQf6LRC", "RUQf6LRD"),
            "not a signature".into(),
        ] {
            fs::write(crate::util::sidecar(&artifact), STANDARD.encode(signature)).unwrap();
            assert!(verify_with_public(&public, &artifact).is_err());
        }
        fs::write(crate::util::sidecar(&artifact), "not base64!").unwrap();
        assert!(verify_with_public(&public, &artifact).is_err());
        assert!(verify_with_public("not base64!", &artifact).is_err());
    }
}
