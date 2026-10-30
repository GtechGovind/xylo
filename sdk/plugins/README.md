# Xylo Rust plugin SDK

This workspace supplies Rust guest bindings, a component packager and a working
text-report extension. It is separate from the desktop application dependency
graph. Its developer tools require Rust; installed components run inside the
native Wasmtime host and require no Python, Node, WASI or external guest process.

The [WIT contract](wit/xylo.wit) describes the existing structural ABI:
three root host imports and `run(string) -> result<string,string>`. The package
version identifies this contract; the host currently checks the structural
signature and supported imports, rather than negotiating a protocol version.
Bindings are generated with pinned `wit-bindgen 0.57.1`; the native packaging tool
uses pinned `wit-component 0.261.0`. Both are covered by this workspace's lockfile.
See the [upstream binding documentation](https://github.com/bytecodealliance/wit-bindgen)
for the module-to-component build model.

## Build and verify the real example

From the repository root with its selected Rust toolchain installed:

```sh
bash scripts/test-plugin-sdk.sh
```

The script installs the matching `wasm32-unknown-unknown` standard library if
needed, checks the SDK, builds the guest and creates a new package under
`target/plugin-sdk-validation.*/text-report/`. It then invokes the actual native
installed-plugin service using temporary private storage. The validation checks
Unicode counts, actual artifact bytes, no-clobber repeated execution, read/write
permission revocation and workspace escape refusal. No network request, account
or device operation is performed. The compiler/registry may need network access
for dependencies; that is separate from guest execution.

The generated `plugin.json` can be selected in **Settings → Plugins**. Inspect
the package, explicitly grant its requested permissions, install and enable it.
Give it JSON input such as:

```json
{"path":"notes.txt","output_name":"notes-report"}
```

The path identifies a UTF-8 file in the selected project, up to 16 KiB for this
example. It reports bytes, Unicode scalar values, lines and case-sensitive
alphanumeric word counts, and publishes the statistics as a new text artifact.
Top words are ordered by descending frequency, then lexical order. The vocabulary
is bounded at 4,096 distinct words and reports when that limit was reached; total
word count still includes every word. It does not send the file to a model or
change the source. Artifact links come from the host's committed write receipt.
Choose a new `output_name` for another report; an existing name fails without
changing its file. Names use letters, digits, `-` or `_`, without an extension.

## Create an extension

Create a Rust `cdylib` with a path dependency on `guest/`, implement the generated
`Guest` trait and export the implementation:

```rust
use xylo_plugin_sdk::Guest;

struct MyPlugin;
impl Guest for MyPlugin {
    fn run(input: String) -> Result<String, String> {
        Ok(input)
    }
}
xylo_plugin_sdk::export!(MyPlugin);
```

Compile for `wasm32-unknown-unknown`. Avoid operating-system, socket, thread and
clock APIs: this host supplies only `read_text`, `write_text` and `http_get` through
the SDK. No WASI adapter is included. The native packager wraps the resulting
core module and embedded WIT metadata as a validated WebAssembly component:

```sh
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$PWD/target" cargo build --locked --release \
  --manifest-path sdk/plugins/Cargo.toml -p xylo-text-report \
  --target wasm32-unknown-unknown
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$PWD/target" cargo run --locked \
  --manifest-path sdk/plugins/Cargo.toml -p xylo-plugin-pack -- \
  target/wasm32-unknown-unknown/release/xylo_text_report.wasm \
  sdk/plugins/examples/text-report/package.json /absolute/new-package-directory
```

Use a nonexistent output directory. The packager computes the component checksum
and creates `component.wasm` and `plugin.json` without replacing existing files.
It does not grant permissions, install or execute the package. Installation
independently checks the application's manifest rules and exact supported ABI;
successful component encoding alone does not establish install compatibility.

Requested capabilities and resource budgets belong in a package template, not
in agent input. A user may grant only a subset. Import availability never bypasses
stored grants, private-storage exclusion or quotas. Host errors are ordinary
`Result` values; an error after artifact publication does not roll back the
committed file. See [native plugin behavior](../../docs/native-plugins.md) for
permissions, cancellation, retained artifacts and platform qualifications.

Our SDK and example use Apache-2.0. Preserve dependency licenses and notices when
distributing compiled guests or developer tools; the SDK lockfile identifies its
separate build closure. Passing this local example does not qualify arbitrary
third-party extensions or cross-platform release behavior.
