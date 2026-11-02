# Distribution and notifications (`xylo-distribution`, `xylo-notifications`)

Two small leaf crates used by `xylo-runtime` ([runtime](runtime.md): `updates.rs`, `host/update_control.rs`, `host/notice_control.rs`). Neither contains UI code.

# Part 1: `xylo-distribution`

## Purpose and boundaries

"Authenticated cargo-packager updates." Both the release manifest and the installer bytes are minisign-signed. Blocking methods must be called off the UI thread. **Installation is a separate explicit action** after the host has saved edits and stopped running work (`lib.rs` header, `Updater::install`).

**A fresh build has no update feed:** `UpdatePolicy::default()` is `enabled: false` with no URLs or key, so `Updater::for_application` returns `Ok(None)`. Updates only work where trusted application configuration supplies a policy; `Updater::new` is for that and for test fixtures, never a remote document.

## Module map

| Path under `crates/distribution/src` | Purpose |
| --- | --- |
| `lib.rs` | `UpdatePolicy`, `ReleaseManifest`, `ReleaseArtifact`, `Updater`, staging, install, `product_error` |
| `transport.rs` | Bounded HTTPS `get_bytes` and `download` with cancellation |
| `state.rs` | Crash-safe install journal `installed.json` and `reconcile` |
| `archive.rs` | Preflight of the macOS `.app` tar.gz layout before extraction |
| `macos.rs` | Transactional bundle replacement and publisher checks (macOS only) |
| `bin/xylo-release-verify.rs` | Offline build tool: `POLICY PLATFORM MANIFEST SIGNATURE ARTIFACT`; not an app sidecar |
| `bin/xylo-signature-verify.rs` | Streams and verifies a download against the public key and `.sig`; not an app sidecar |
| `tests/mod.rs` | Tests |

## Release candidates and signatures

Release-candidate tags (`v…-rc.…`) build native macOS, Windows, and Linux packages through `.github/workflows/release.yml`. The CI publisher waits for all three jobs and successful main-branch CI at the same commit. A locally verified macOS build can be published first with `publish_release.py --allow-partial`; its notes identify the pending platforms. CI adds the remaining downloads without replacing already published assets. The tag, Cargo package version, and `packager.json` version must agree.

Linux packaging derives shared-library dependencies with `dpkg-shlibdeps` and declares the dynamically loaded graphics libraries, `bubblewrap`, `socat`, and `ripgrep`. Both Linux formats are extracted and checked for executable permissions, helper files, and CLI startup before signing. Debian package versions use `~rc.…` so candidates sort before stable releases; app versions and download names retain SemVer. AppImage users must install `bubblewrap`, `socat`, and `ripgrep` separately. Host policy must allow Bubblewrap's user and network namespaces; Ubuntu's AppArmor restrictions can block them. CI enables these namespaces only on its disposable runner and checks startup before compiling Rust. Installed applications never change host security policy. These package checks do not qualify live desktop behavior on every Linux distribution.

macOS bundles always receive an application signature, including an ad-hoc seal when Developer ID credentials are unavailable. Rust linker signatures do not cover the completed bundle’s Info.plist and resources. Packaging validates the source app, the read-only mounted DMG app, and the extracted archive with `codesign --verify --deep --strict`, checks required helpers, and starts the packaged CLI. Native regression coverage rejects unsealed and modified bundles. Ad-hoc signing verifies bundle integrity, but does not provide Gatekeeper publisher trust or notarization; users of the preview may need [Apple’s Open Anyway flow](https://support.apple.com/102445). The app and installer never disable Gatekeeper or remove download quarantine.

Every published download, checksum file, and platform receipt is signed with the project key. The encrypted private key and password are GitHub Actions secrets named `MINISIGN_KEY` and `MINISIGN_PASSWORD`; only the public key is checked in at [`.github/release-signing.pub`](../../.github/release-signing.pub). The local encrypted backup is outside the checkout, and its password is in the macOS login Keychain under service `com.qurkos.xylo.release`, account `MINISIGN_PASSWORD`. Do not put private keys in the repository or release uploads.

Verify a downloaded file from the repository root:

```sh
cargo run --locked --release -p xylo-distribution --bin xylo-signature-verify -- \
  .github/release-signing.pub /path/to/download.sig /path/to/download
```

The public key and signatures use cargo-packager's Base64 encoding of complete Minisign files. A project signature verifies download integrity and provenance; it does not provide an Apple Developer ID identity, Apple notarization, or a Windows Authenticode publisher certificate. Receipts and prerelease notes state that distinction explicitly. The initial RC uses project signatures while platform certificates are not configured.

For Apple signing, configure `APPLE_CERTIFICATE` (Base64 PKCS#12), `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD` (app-specific), and `APPLE_TEAM_ID`. Use a Developer ID Application certificate. The packaging job validates the signed app and its notarization ticket before describing platform signing as verified. Windows publisher signing requires an issued certificate and a signing-provider setup; it is not enabled by a Minisign key.

Depends on `cargo-packager-updater` (pinned `=0.2.3`, rustls), `minisign-verify`, `reqwest` (https only), `semver`, `sha2`, `fs2`, `tar`/`flate2`. Targets: `macos-{aarch64,x86_64}`, `windows-{x86_64,aarch64}`, `linux-{x86_64,aarch64}`.

## Policy and manifest

`UpdatePolicy`: `enabled`, `application_id` (default `com.qurkos.xylo`), `channel` (`stable`), `manifest_url`, `signature_url`, `public_key` (base64 of the whole minisign public-key file), `allowed_origins`, `max_artifact_bytes` (default 512 MiB; must be 1..=2 GiB), `macos_team_id`. Validation requires: enabled, non-empty identity and channel, a decodable key, and both feed URLs set.

**URL rule**: every URL (feed, artifact, each redirect) must be HTTPS, without credentials or fragment, and its origin must be exactly in `allowed_origins`.

`ReleaseManifest` (`deny_unknown_fields`): `schema` (must be 1), `application_id`, `channel`, `sequence`, `version`, `published_at`, `expires_at` (unix seconds), `notes` (max 65536 bytes), `platforms: map target -> ReleaseArtifact { url, signature, format, sha256 (64 lowercase hex), size_bytes }`. Formats: `app` (macOS), `appimage` (Linux), `nsis` or `wix` (Windows). **Sign the exact UTF-8 manifest bytes; feeds are not reserialized.**

## Update lifecycle (`Updater`)

1. **`check`**: takes the exclusive `update.lock` (`try_lock_exclusive`, else "Another update action is in progress"), reconciles the journal, fetches the manifest (limit 1 MiB) and signature (limit 16 KiB), then calls `verify_manifest`.
2. **`verify_manifest`** verifies the signature **before parsing** the JSON, then checks in order: stable channel rejects prereleases; schema, application id and channel match; `sequence > 0`, `published_at <= now + 300 s`, not expired, lifetime at most 31 days; version not below `max(current, journal highest)`; sequence not below the journal's; a reused sequence must carry the same manifest SHA-256; an artifact for this target; allowed artifact URL; size within `max_artifact_bytes`; digest shape; format matches platform. Returns `None` when the version equals the current one.
3. **`stage`**: revalidates, downloads to a temp file in the data directory (`UpdateProgress` phases `downloading`, `verifying`, `ready`; progress throttled to 100 ms), then `verify_file` streams SHA-256 and the minisign signature and checks the exact size.
4. **`install` / `install_staged`**: revalidates, rereads the staged bytes (bounded) and re-verifies digest and signature, resolves the install path, runs archive preflight, **writes a `pending` journal entry**, then installs. `install_staged` borrows so the host can keep the verified download after a native failure. Returns `InstallOutcome { version, restart_required: true }`; Windows installers may terminate the process.
5. `verify_artifact` re-checks an existing file offline (used by `xylo-release-verify`).

Install paths: macOS must run from `X.app/Contents/MacOS/...`; Linux must be an `.AppImage` (the `APPIMAGE` env path is used inside AppImage), otherwise "Use the system package manager to update this installation"; Windows uses the executable's directory.

## State journal (`state.rs`)

`<data>/installed.json` (max 16 KiB, written via temp file, fsync and rename) stores `highest_version`, `highest_sequence`, `highest_manifest` and `pending`. **Rollback protection**: versions and sequences cannot move backwards. `pending` is kept on install error, because replacement may have succeeded; `reconcile` (on construction and each `check`) promotes it once the running version is at least the pending one. A corrupt journal is an error and is **never silently reset**. The data directory is created with mode 0700 on Unix.

## Transport (`transport.rs`)

`reqwest` with `https_only`, 10 s connect timeout, 10 s read timeout, 300 s overall, at most 5 redirects, each redirect re-checked against the policy. Cancellation (`CancellationToken`) interrupts both connect and stalled body reads. Each call builds a private current-thread Tokio runtime. Downloads reject a `Content-Length` or total different from `size_bytes`.

## Archive and macOS checks

`archive::preflight` (for `app`): only normal path components, exactly one `.app` root, no duplicate entries, at most 100,000 entries and 2 GiB unpacked, no setuid/setgid/sticky bits, no special files, no absolute or bundle-escaping symlinks, and required files `Contents/Info.plist`, `Contents/MacOS/xylo`, `xylo-worker` and `xylo-helper`. A valid signature does not waive these.

`macos::install` extracts into a `.xylo-update-*` temp dir beside the destination, then verifies with `codesign --verify --deep --strict`, a matching `TeamIdentifier` (`macos_team_id` must be 10 characters of A-Z/0-9, required by `Updater::new` on macOS), plist `CFBundleIdentifier` and `CFBundleShortVersionString`, and `spctl --assess`, before an atomic `renameat_with` swap. **Failures never delete the installed bundle**; a non-writable folder says to install the package manually.

## Errors and security

Errors are `anyhow`. `product_error` maps an error to fixed user text (cancelled, could not be verified, older than installed, not configured, generic) so signature and path details stay in logs. Keys and feed origins are build-time trust anchors; the release workflow's signing variables are described in `sdk/README.md` ("CI and releases"). Testing: `cargo test --locked -p xylo-distribution` (not run when writing this page).

# Part 2: `xylo-notifications`

## Purpose and boundaries

"Opt-in native notifications." Host lifecycle events carry IDs and enums only, and **fixed product copy is chosen inside the crate, never model- or tool-produced content** (`lib.rs` header). `NoticeEvent { session_id, turn_id, event_id, kind, occurred_at_ms, background }` has deliberately no title, body, prompt or path field.

`Service::new` creates no permission dialog, OS notification, database or polling task. The SQLite file `notifications.sqlite3` (WAL, mode 0600 on Unix) is opened lazily.

## Module map

| Path under `crates/notifications/src` | Purpose |
| --- | --- |
| `lib.rs` | `Adapter` trait, `Service`, `observe`, permission and preference flows, `tag`, `now` |
| `types.rs` | `Preferences`, `Permission`, `Kind`, `NoticeEvent`, `Snapshot`, `Outcome`, `AppIdentity`, `UserAction`, `NativeNotice` |
| `store.rs` | Bounded metadata: preferences, per-turn terminal state, notice tags |
| `platform/mod.rs` | `native(identity)` selection |
| `platform/actor.rs` | One idle native thread for non-Send OS objects (macOS and Windows); command channel 16, activation channel 64 |
| `platform/macos.rs` | Apple UserNotifications |
| `platform/windows.rs` | WinRT toasts for the live process |
| `platform/linux.rs`, `platform/linux/` | `org.freedesktop.Notifications` over D-Bus (zbus) |
| `tests/mod.rs` | Tests |

## Preferences and kinds

`Preferences` (`deny_unknown_fields`): `enabled` (**default false**), `completion`, `approval`, `failure`, `schedule` (default true) and `background_only` (default true). `Kind`: `Started`, `Approval`, `Completed`, `Failed`, `Cancelled`, `ScheduleCompleted`, `ScheduleFailed`. Only `Completed`, `Failed`, `Approval` and the two schedule kinds have text (e.g. "Task complete", "Approval needed"); `Started` and `Cancelled` are recorded but never shown. `Permission` states: `unknown`, `not_determined`, `denied`, `allowed`, `quiet`, `not_required`, `unsupported`, `unavailable`; delivery is allowed for `allowed`, `quiet` and `not_required`.

## Delivery pipeline (`Service::observe`)

Suppressed unless notifications are enabled (or a journal already exists). Then:

1. Events older than **120 s** or more than 5 s in the future return `Outcome::Stale`, so recovered history is not announced.
2. The store **claims** the event in an immediate transaction. A terminal turn rejects later events (`Duplicate`); a terminal kind returns earlier tags of that turn for retraction (`dismiss`) and invalidates them. Tags are `wb-` plus a 32-hex-character SHA-256 prefix of `session:turn:event:kind`, which makes replay idempotent.
3. Preferences are checked (kind toggle, `background_only` requires `event.background`), then the OS permission is read and must allow delivery.
4. The notice is posted, and cancellation, policy change (`policy_epoch`) or a mid-flight disable dismisses it and returns `Cancelled`. Otherwise `Submitted`.

Retention: notices older than 30 days or beyond 1024 rows and turns beyond 4096 rows are pruned on claim. `next_activation` consumes a click's tag once (valid, not yet activated, at most 30 days old) and returns `Activation { session_id, turn_id }` so the app can focus its own window.

## Permission, test and settings flows

- Permission is requested **only** by `request_permission(UserAction::RequestPermission, ...)` (requires notifications enabled) and a toast test by `test_notification(UserAction::SendTest, ...)`. `observe` never requests permission or enables preferences.
- `initialize` loads preferences once; if enabled it does a read-only permission refresh (restores the activation delegate after restart).
- `set_preferences` runs as a detached task, so a dropped UI waiter does not abandon a write. **Tightening applies in memory first** (`enabled`/kind flags are AND-ed; `background_only` OR-ed), before the durable write; loosening takes effect only after the save succeeds. Disabling invalidates and dismisses all live notices. It uses a separate lock from native delivery, so turning off never waits on a permission dialog.
- `Snapshot` carries `Capabilities` (`request_permission`, `live_activation`, `cold_activation`, `retraction`) and a user-facing message for denied, unsupported or unavailable states.

## Platforms

| Platform | Notes (from source) |
| --- | --- |
| macOS | UserNotifications. Needs an application bundle (checks `bundleIdentifier`); otherwise `Unsupported` or "Install the application bundle to use native notifications". `Authorized` maps to `allowed`; some provisional states to `quiet` |
| Windows | WinRT toasts for the live process; requires installed app identity (registry entry under current user). No cold-start activation claimed |
| Linux | XDG D-Bus; `Permission::NotRequired` when the service responds, `Unavailable` otherwise; `request_permission` capability false |
| other | `native()` fails with "Native notifications are unavailable on this platform" |

The actor-based backends report `live_activation` and `retraction` true and `cold_activation` false; Linux reports `cold_activation` false and `retraction` true.

`AppIdentity.id` must be 1..=128 chars of ASCII alphanumerics, `.` or `-`.

## Concurrency and security

An `operation` mutex serialises delivery, permission and test calls; `observe` runs in a spawned task with a child cancel token that is cancelled if the caller drops it. Store access uses `spawn_blocking`. **No chat text, file paths or secrets are stored or shown**; notification metadata is IDs, category and timestamps.

## Testing

`cargo test --locked -p xylo-notifications` (not run when writing this page); tests use a fake adapter and an isolated D-Bus adapter compile with no daemon.

## Related

[runtime](runtime.md) (`updates.rs`, `notice_control.rs`, update orchestration), [core](core.md), [services](services.md), [cli](cli.md) and [sdk](sdk.md) (the CLI links these crates only transitively through `xylo-runtime` and never calls them).
