# Obscura native certificate roots

`obscura-native-roots.patch` applies only to the immutable Obscura 0.2.6
revision `8534b401898ed17b62f69bdc3d5eb186bffb0d60`. It enables reqwest's
`rustls-tls-native-roots` feature alongside its existing bundled WebPKI roots.
This allows the managed browser to trust locally installed certificate
authorities, including enterprise HTTPS inspection certificates, without
turning off certificate or hostname verification. The existing
`SSL_CERT_FILE` / `SSL_CERT_DIR` support remains available. Native certificate
loading honors those environment overrides; it does not modify OS trust.

The loader uses macOS Keychain trust settings, Windows certificate stores, and
system certificate files on Linux. Bundled WebPKI roots remain trusted, so
this is additive trust, not a promise to mirror every platform distrust or
certificate policy. A missing or untrusted issuer must still fail TLS.

The patch includes the exact additional Cargo.lock entries. Existing upstream
package versions are unchanged. The builder checks the pristine pinned source,
applies this patch, and verifies that its tracked changes match the patch
exactly and that no untracked source files appeared. It verifies again after
compilation. The patch's SHA-256 is recorded in both BUILD.json and
XYLO-BUILD.json; missing or mismatching hashes invalidate staged binaries.

Local validation must include builder tests and a rebuilt managed browser.
For the reported regression, run a bounded public-only fetch of
`https://docs.rs/rappct/latest/rappct/` and check `document.title`. Do not use a
successful data: smoke test as proof of HTTPS trust. Native Windows and Linux
runtime acceptance must run on those hosts; a macOS run cannot qualify them.
