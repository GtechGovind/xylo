# Native validation

Run these commands from the repository root. The checked-in Cargo configuration uses the root `target` directory for the app and this standalone validation workspace. Different dependency versions, features, or profiles can still require separate artifacts; sharing a directory does not guarantee identical compilation.

```sh
# Fast compile feedback without linking the app.
cargo check -p xylo --locked

# Build the desktop, worker, helper, and CLI; no evaluation binary.
cargo app

# Run only the native checks relevant to the change.
cargo native-test --test composer-interactions
cargo native-test --test draft-interactions

# Capture only the relevant UI fixture.
cargo native-test --test composer-rendering

# Ordinary desktop unit tests run once here.
cargo test -p xylo --locked
```

The native harnesses compile production desktop code with the `validation` feature. Embedded unit-test modules are excluded from these harnesses because the normal desktop test target owns them. Test-only input/state accessors remain available. The terminal test module retains three unit tests because it also supplies renderer fixtures; the rest of the desktop suites are not repeated in each native executable.

Validation executables omit debug information and incremental output. Shared Xylo libraries retain the app's development settings so artifacts can be reused when features match. App development remains incremental. Run one focused harness while iterating; run the appropriate broader checks before release or when shared behavior changes. Avoid repeatedly changing `RUSTFLAGS`, target directories, or profile environment overrides, which create incompatible cache variants.

## Generated files and cleanup

Test source is small. Large test executables, object files, dependency variants, and incremental compiler caches account for most disk use. Do not delete useful tests to remove their compiled outputs.

After all Cargo/rustc processes have stopped, remove only this package's generated executables with:

```sh
cargo clean --manifest-path crates/desktop/validation/Cargo.toml -p xylo-native-validation
```

This keeps shared dependency artifacts. A full `cargo clean` is appropriate when deliberately resetting the build cache, but makes the next build cold. Existing pre-configuration `crates/desktop/validation/target` build artifacts are obsolete once the root target is used and may be removed when not in use.

Screenshots default to `crates/desktop/validation/target/native-preview` even with a shared compiler target. Each capture harness replaces its previous images, so this small preview folder can reappear. Set `XYLO_RENDER_DIR` to a chosen evidence directory when preserving a reviewed capture. Do not treat those images as proof of real provider/account execution.

Lockfiles remain separate. Consolidating them or the harness executables is a later structural change; do not casually add all native capture targets to ordinary `cargo test --workspace`.

Build a local macOS bundle with `python3 scripts/bundle-macos.py` from the repository root
(`--profile debug` is available for development). The bundle contains copies of the desktop,
worker, helper and CLI programs from the same profile, so clearing `target` cannot break it.
Plain `cargo build` and `cargo build --release` now build all four required programs.
