# Release acceptance record

This is the signed production release gate, not a statement that distribution is complete. The explicitly unsigned first prerelease follows the separate [preview gate](preview-release.md), with complete license inventories and all six native package smoke results required. Keep evidence alongside each exact artifact version/target/SHA-256.

- [ ] Source is committed, clean, reviewed; version and dependency lock are final.
- [ ] Chosen OS/SDK/native compiler versions are recorded; pinned Rust and packager match.
- [ ] Dependency license texts and notices are complete; review `missing_texts` and native library/model licenses. SBOM is present.
- [ ] No private keys, provider credentials, account tokens, user workspaces, downloaded weights or app-data databases are bundled.
- [ ] Real maintainer update public key, application identity/channel, HTTPS origins/feed and expiry policy are embedded. The checked-in disabled policy is not a production setup.
- [ ] Native runtime payload provenance matches catalog/import rules on this OS; signing did not invalidate asset hashes.
- [ ] macOS nested/outer Developer ID signing, correct Team ID, audio entitlement, notarization and stapling pass. Validate the extracted updater `.app`, not only the prearchive app.
- [ ] Windows all three executables, native DLLs and NSIS installer have valid timestamped Authenticode signatures. Per-user installer blocks downgrade.
- [ ] Linux package dependencies and bundled dynamic libraries are reviewed; a real Vulkan driver/window session is available. AppImage is executable; Debian packages use package-manager updates.
- [ ] Final updater artifact and immutable manifest signatures verify with the exact embedded key. A newer sequence has not reused older metadata bytes. Checksums match downloaded bytes.
- [ ] First install, launch, uninstall and reinstall pass in a **clean user/VM without Rust, Python, Node or build tools**, for each intended platform/architecture.
- [ ] Open workspace, streaming chat with a configured provider, approvals, editor, PTY/Unicode/IME, connectors, export, optional native asset install/activation and graceful Quit work on that clean machine.
- [ ] Explicit microphone recording and audio playback pass with real OS permission prompts/devices; deny/revoke/cancel/disconnect paths are usable. Screen capture is explicitly invoked and permission handling works.
- [ ] Real signed N -> N+1 update passes: Check, Download progress/cancel, Save/Discard/Cancel edits, quiesced explicit Install, restart and preserved chats/settings/credentials.
- [ ] Interrupted download, process crash before/after replacement, denied write permission, disk-full simulation, invalid signature, expired feed, wrong platform, rollback and reused-sequence attempts leave a recoverable installation. Journal reconciliation matches the running version.
- [ ] Gatekeeper/SmartScreen and offline first-launch/stapled-ticket behavior pass after actual browser download/quarantine, without bypass instructions or disabled OS protections.
- [ ] Two isolated unsigned builds were compared before claiming binary reproducibility. Signed timestamped artifact differences are documented.
- [ ] Release domain/access controls, protected signing process, key backup/rotation/revocation response and atomic feed promotion are ready. An authorized maintainer explicitly approves external publication.

Current local evidence: 9 updater Rust tests pass on macOS arm64; release-script tests and metadata validation are recorded in the development handoff. There is no production signing key/feed or externally published release in this workspace. A fixture replacement or a development preview does not satisfy clean-machine or live-update acceptance.
