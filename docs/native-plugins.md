# Native component plugins

Plugins are reviewed local component packages installed in the application's private profile storage. Installation does not execute guest code. Running a plugin requires explicit stored permissions and the reviewed package SHA256 identity. There is no Python, Node, WASI, subprocess, ambient filesystem or ambient network dependency in this runtime.

The [Rust plugin SDK](../sdk/plugins/README.md) supplies the WIT contract, generated guest bindings, an offline native component packager and a text-report extension that reads a project file and publishes an artifact. Its validation command compiles the actual Rust guest and runs it through the installed-plugin service, including permission revocation. The SDK is a separate developer workspace and adds no desktop runtime dependency.

## Package format

Choose a package's JSON manifest through the native file picker. The component may be a binary WebAssembly component or component WAT; neither requires an external compiler process. Its exact bytes must match the manifest's lowercase SHA256:

```json
{
  "format": 1,
  "id": "example.echo",
  "name": "Example echo",
  "version": "1.0.0",
  "entrypoint": "component.wasm",
  "sha256": "<64 lowercase hexadecimal characters>",
  "capabilities": [],
  "budgets": {
    "memory_mib": 32,
    "fuel": 10000000,
    "timeout_seconds": 15,
    "max_input_bytes": 262144,
    "max_output_bytes": 262144
  }
}
```

The entrypoint is a regular relative file below the manifest directory. Parent traversal, absolute entrypoints and symbolic links in the entrypoint are rejected. Unknown manifest fields, duplicate capabilities, malformed IDs/checksums and unsupported budget values fail validation. Package IDs are portable lowercase identifiers of at most 64 bytes; Windows device names are reserved. Names are at most 120 UTF-8 bytes, versions 64 bytes and entrypoint paths 512 bytes. Manifest input is capped at 32 KiB; the exact original component input is capped at 16 MiB.

Inspection returns the parsed manifest, component size and a separate `package_sha256`. That identity hashes a domain separator, the exact reviewed manifest bytes and the component hash. Install/update requires the same identity, so changing either reviewed file requires another review. Installation compiles and validates the run signature and supported imports before publication. A changed package under an existing ID must declare a different version and use an explicit Update action.

The component exports `run(input: string) -> result<string,string>`. The self-contained [echo fixture](../crates/capabilities/src/plugins/fixtures/echo.wat) demonstrates this ABI. Only these root imports are available:

- `read-text(path: string) -> result<string,string>` requires `read_workspace`. On Unix, reads use opened directory handles from filesystem root through the workspace, then resolve each relative component using no-follow `openat`. Windows uses the native relative-handle implementation described below. The bounded reader uses the same final regular-file handle, so swapping a checked file or an ancestor cannot redirect it to outside/private text. Application data is excluded by directory identity, including when its parent is selected as a workspace. Symbolic links, hard-link aliases, special files, parent traversal, paths over 4,096 bytes and more than 256 relative components are refused.
- `write-text(name: string, text: string) -> result<string,string>` requires `write_artifacts`. It publishes a new UTF-8 `.txt` artifact using the existing safe basename rules, with no overwrite.
- `http-get(url: string) -> result<string,string>` requires `https:exact.lowercase.host`. HTTPS uses port 443, rejects embedded URL credentials/fragments, follows no redirects and supplies no application credentials or cookies. A host grant does not grant subdomains or alternate ports. Explicitly approving a private host permits that exact host; grants are not a public-internet-only policy.

Permissions are exact strings. A user can grant a subset of the manifest's requested capabilities; an invocation cannot add grants or override package budgets. Revocation sets `grants: []`; disabling also blocks execution.

## Windows file authority and qualification

The Windows implementation uses the existing pinned `windows-sys` dependency and public native Windows file APIs, without a filesystem runtime or another executable. A trusted configured disk root is opened once, then qualified from that same handle as a local disk volume and an actual volume root. Network mappings, SUBST directory mappings and roots without supported volume identification fail closed. A bounded volume-GUID name query qualifies only the held root; its result never authorizes a later path reopen. See Microsoft's [handle path query](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-getfinalpathnamebyhandlew) and [device information](https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/wdm/ns-wdm-_file_fs_device_information) contracts.

Workspace, application-data and requested directories are resolved one component at a time using `NtCreateFile` with `OBJECT_ATTRIBUTES.RootDirectory` set to the retained parent handle. Every relative open uses `OBJ_DONT_REPARSE`, `FILE_OPEN_REPARSE_POINT`, an explicit directory/non-directory requirement and a read-only sharing policy. Handle attributes independently reject reparse points, devices and offline placeholders; the final regular file must have exactly one link. Directory identities use a volume serial number and 128-bit file identifier, excluding the application-data boundary even through alternative directory names or when a workspace is selected inside application data. Ancestor handles remain held until the final file is opened and checked. These are handle-relative operations, with no canonicalize-then-reopen step. See [NtCreateFile](https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-ntcreatefile), [root directory semantics](https://learn.microsoft.com/en-us/windows/win32/api/ntdef/ns-ntdef-_object_attributes) and [handle information](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getfileinformationbyhandleex).

User paths can be relative to the workspace or ordinary absolute drive paths whose components exactly match the configured workspace prefix. Application-owned canonical roots may contain Windows' verbatim disk prefix; that namespace is not accepted from plugin input. UNC/device namespaces, drive-relative paths, alternate data streams, traversal, empty segments, reserved device names, trailing-dot/space aliases and components over 255 UTF-16 units are rejected. Canonical root paths are bounded at 32,767 UTF-16 units and 256 components. Exact absolute-prefix matching can conservatively refuse a differently cased or abbreviated spelling; relative project paths avoid that ambiguity. Cloud/junction/symlink-backed projects, unsupported file identifiers or volume queries, and sharing conflicts are reported as failures rather than weakening the policy.

The reader receives the already verified final file handle. Existing per-read/cumulative byte limits, UTF-8 validation, cancellation checks and retained resource/run leases apply unchanged. Read-only sharing refuses concurrent data-write/delete opens, and oplock conflicts are returned without waiting for a break to complete. This does not promise to freeze every metadata-only operation or later hard-link creation; multi-link aliases are rejected at the final open, and security relies on reading that same approved handle. Cancellation cannot forcibly interrupt a kernel or filesystem-driver call already executing; the admitted blocking closure keeps its leases until it actually returns. Other targets without Unix or Windows handle support still fail closed.

**Qualification limit:** this session runs on arm64 macOS. The actual Windows production module and its eight Windows fixture bodies passed `x86_64-pc-windows-msvc` cross-target typechecking and strict Clippy; they have not been linked or executed on Windows here. That is not Windows filesystem, installer or release acceptance. The native fixture checks actual Unicode reads, private/hard-link aliases, deterministic post-validation substitution, ancestor rename and final-write fences, cancellation, real directory junctions, streams, wrong file types and path aliases. A file-symlink subcase reports the missing privilege if the runner has neither Developer Mode nor symlink permission; the directory-junction case remains required.

The independent validation manifest includes the actual production reader and its test file, using only a minimal path configuration. It allows focused checks without building browser/audio/Wasmtime for a second platform and does not qualify the complete application's Windows integration:

```sh
# Cross-target API/type validation from this repository on macOS; no Windows execution.
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$PWD/target" cargo check --locked --offline --manifest-path crates/capabilities/validation/windows-read/Cargo.toml --target x86_64-pc-windows-msvc --all-targets
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$PWD/target" cargo clippy --locked --offline --manifest-path crates/capabilities/validation/windows-read/Cargo.toml --target x86_64-pc-windows-msvc --all-targets -- -D warnings

# Actual focused filesystem tests: run on Windows with its native toolchain.
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$PWD/target" cargo test --locked --manifest-path crates/capabilities/validation/windows-read/Cargo.toml --target x86_64-pc-windows-msvc
```

## Native controls and agent tools

The profile-wide native management channel exposes the following controls. They are deliberately absent from agent tool discovery and ordinary tool execution:

| Control | Arguments | Result |
| --- | --- | --- |
| `helper_plugin_list` | `{}` | `PluginCatalog {revision, plugins}` |
| `helper_plugin_inspect` | `{manifest_path}` | `PluginInspection {manifest,module_bytes,package_sha256}` |
| `helper_plugin_install` | `{operation_id,expected_revision,manifest_path,expected_package_sha256,grants,enabled,replace}` | `PluginManagementResult` |
| `helper_plugin_set_enabled` | `{operation_id,expected_revision,id,enabled}` | `PluginManagementResult` |
| `helper_plugin_set_grants` | `{operation_id,expected_revision,id,grants}` | `PluginManagementResult` |
| `helper_plugin_remove` | `{operation_id,expected_revision,id}` | `PluginManagementResult` |

`operation_id` is a non-nil UUID generated for the user's exact action. `expected_revision` is the opaque revision from the last catalog review. A concurrent catalog change rejects the mutation. `replace:false` installs only a new ID; `replace:true` updates only an existing ID. `PluginManagementResult` contains `operation_id`, the current `catalog`, `changed`, and a display `message`. Each installed entry contains the manifest, immutable package hash, enablement, effective grants and Unix-second installation/update times.

Lost-response reconciliation reuses the same operation ID and exact arguments. An accepted request replays its recorded outcome without loading the original picked files or applying the mutation again. Reusing an ID for different arguments is rejected. The returned catalog reflects current profile state, which can include subsequent actions. Accepted operation identities are never silently forgotten: storage is bounded at 4,096 identities and 2 MiB, after which further mutations fail without applying an action. This is a documented profile-management limit rather than an automatic destructive history reset.

Agent discovery exposes `plugin_list` and `plugin_run_installed`. Running requires `{id,input,package_sha256}`; the package pin must match the currently installed reviewed package. Enablement and grants are loaded again immediately before execution. `plugin_run` remains available for explicitly approved workspace components with explicit per-invocation grants and valid resource bounds; it cannot access the private installed catalog through tool paths.

Ordinary run results use `ToolObservation {status,summary,artifacts}`. Successful `summary` JSON contains `output`, `fuel_remaining`, `memory_budget_bytes` and `plugin:{id,version,package_sha256}`. Committed artifacts are returned separately. If execution fails after publishing an artifact, its failure result retains those paths. If an admitted artifact publication is still completing after the bounded cleanup wait, the result is `outcome_unknown`; inspect artifacts before retrying. Publication uses no-clobber filenames, so a retry cannot silently replace a previous result.

## Storage, leases and cancellation

The host supplies `CapabilityConfig.management_root`; management refuses an absent root. Production uses `data/native-management/plugins`, separate from artifacts. Immutable packages live under `packages/<package_sha256>`; staged copies are synced before being referenced by the atomic catalog. Catalog transactions reread under an exclusive cross-process lock, check revision and operation identity, then commit one atomic state file. Failed validation/update leaves the existing entry intact. Removing an entry removes its authority after acquiring the lease and reclaims its unreferenced immutable package. Updates reclaim the replaced package only after the catalog commit. Garbage collection runs under the exclusive catalog lock, preserving every live catalog reference; a running guest prevents its package from becoming unreferenced. Crash-orphaned staging/package directories are collected on the next mutation. Package storage is capped at 1 GiB, including the old and staged new version during an update; an installation that would exceed the quota is refused before copying. A failed or uncertain catalog publication retains any package still referenced by the reread catalog.

A run acquires a shared per-plugin lease while holding the catalog read lock, checks its package hash and current authority, then releases the catalog lock. Update, permission changes, disable and removal require an exclusive per-plugin lease. They return a busy error while a run or its admitted blocking write/compile is active; they do not report a successful revocation while old authority is still executing. Cancel/stop the run, wait for cleanup and retry. Revocation does not forcibly interrupt a currently executing guest.

Compilation admits one blocking compile per helper process, with an additional private profile file lock across processes. A two-entry in-memory compiled-component cache uses exact source hashes; no untrusted serialized native code is loaded. Preflight caps expanded binary size at 32 MiB, nested component depth at 32, functions at 10,000 and code operators at 1,000,000. Fuel and the wall-clock deadline cover compile admission and execution; cancellation/deadline can stop waiting, but an already admitted native compiler cannot be forcibly interrupted. Its admission and run lease remain held until that bounded source compile returns, and cancelled compilation never proceeds to guest execution.

Guest linear memory is limited to one memory of 8–256 MiB, with bounded instances/tables, 1–100,000,000 fuel and a 1–60 second deadline. Input/output package budgets are 1 byte–1 MiB. Output additionally reserves the configured response budget for worst-case JSON escaping, and final JSON is checked. These limits bound guest resources; they are not an OS-enforced RSS limit for the native compiler or an operating-system process sandbox.

Each run additionally caps host calls at 128, cumulative file/HTTP body reads at 4 MiB, pending/committed artifacts at 32 and cumulative artifact bytes at 4 MiB. Reservations precede writes and release on precommit failure. All file/network reads count exact bounded chunks, including files that grow during reading. Artifact writes stage in the artifact directory, sync, recheck cancellation and publish atomically without clobbering. Blocking writes retain their execution lease; cancellation waits up to two seconds for cleanup and reports uncertainty if publication is still in flight.

## Verification

The offline focused suite uses real Wasmtime WAT components, not mocked execution. It covers installation and restart after removing the original package, reviewed hash changes, package tampering, grant escalation rejection, stale catalog revisions, explicit update/revoke/disable/remove, active-run lease fencing, exact operation replay, private-profile/outer-workspace reads, deterministic post-validation file/ancestor substitutions, hard-link/FIFO refusal, symlink rejection, actual atomic artifact publication/no clobber, deterministic cancellation after staging before commit, repeated guest writes hitting artifact count/byte limits, fuel exhaustion, unsupported imports, exact HTTPS grant validation and management-tool exclusion from discovery. Actual blocking file reads retain both their native resource reservation and plugin execution lease until the closure finishes.

Run `CARGO_INCREMENTAL=0 cargo test --offline -p workbench-capabilities --lib plugins::` with the repository-pinned toolchain. Device/account/network acceptance is separate: this suite makes no real HTTPS requests and does not establish third-party plugin compatibility, signed release acceptance or a full product performance claim.
