# Native work admission (`xylo-resources`)

## Purpose and boundaries

`xylo-resources` decides whether a task may start given current memory, CPU and already-reserved demand. It has **no user-selected RAM tiers and no fixed worker counts**; admission follows live hardware headroom. Estimates are **scheduling reservations, not measured RSS and not OS-enforced limits**. Parse, archive, guest, fuel, IO and compile bounds stay with the native service that does the work.

Dependencies (`crates/resources/Cargo.toml`): `anyhow`, `serde`, `serde_json`, `tokio`, `tokio-util`, `uuid`, `tracing`, `fs2`, `sysinfo` and `libc`. It depends on no other Xylo crate. Direct dependents (from `Cargo.toml` files): `capabilities`, `engine` and `runtime`. See [capabilities](capabilities.md), [engine](engine.md) and [runtime](runtime.md). This page extends `crates/resources/README.md`, which holds the long-form rationale and the recorded macOS measurement.

## Module map

| Path under `crates/resources/src` | Purpose |
| --- | --- |
| `lib.rs` | `ResourceManager`, `ResourceLease`, `ResourceSnapshot`, `TaskDemand`, `AdmissionCancelled`, `capability_demand`, `scope_shared`, `spawn_blocking`, the headroom and `fits` rules |
| `shared.rs` | Profile-wide cross-process pool: `acquire`, `SharedResourceLease`, queue tickets, OS-lock ledger |
| `action_lease.rs` | `scope_action`, `current_action_lease`, `ActionLeaseScope`: carries an `Arc<File>` lease into late blocking jobs |
| `macos_memory.rs` | macOS-only reclaimable-page estimate (`cfg(target_os = "macos")`) |
| `tests/`, `shared/tests.rs`, `macos_memory/tests.rs` | Unit tests |

## Key types and public API

- `ResourceSnapshot { total_memory_bytes, available_memory_bytes, cpu_threads, cpu_busy_percent }` (`Clone, Copy, Debug`). `cpu_threads` is the larger of the monitor's CPU count and `available_parallelism`; `cpu_busy_percent` is clamped to 0..=100.
- `TaskDemand { memory_bytes, cpu_share }`. `TaskDemand::interactive(context_tokens)` is `64 MiB + context_tokens * 2048` bytes and `0.25` CPU. The doc comment says the subsystem that knows the real workload should set demand; native inference should include model and KV memory.
- `ResourceManager` (`Clone`, `Default`) is the in-process pool:
  - `snapshot()` refreshes memory and CPU counters and returns a `ResourceSnapshot`.
  - `acquire(demand, cancel)` returns a `ResourceLease`, queuing until the demand fits.
  - `suggested_parallelism(per_job_memory)` is `min(memory_slots, max(cpu_slots, 1))`, never below 1, where memory slots are available memory minus headroom divided by `max(per_job_memory, 1 MiB)`. The engine calls it with 64 MiB (`crates/engine/src/engine.rs`).
- `ResourceLease` frees its reservation on `Drop` and wakes waiters.
- `AdmissionCancelled` is a distinct error ("Task cancelled while queued") so hosts can keep a user's Stop separate from invalid estimates or monitor failures.
- `capability_demand(name) -> Option<TaskDemand>` (below).
- `scope_shared`, `spawn_blocking` and the `shared` module (below).

## Admission rule

Both pools use the same `fits` test (`lib.rs`):

- Headroom is `max(total / 20, 256 MiB)`, i.e. 5% with a 256 MiB floor.
- Memory fits when `demand.memory_bytes <= available - headroom - reserved_memory` (saturating).
- CPU headroom is `max(cpu_threads * (1 - busy% / 100), 1.0)`; it fits when `reserved_cpu + demand.cpu_share <= headroom`. The floor of one share means an idle-enough host can always run one share-1 task.
- A demand with `memory_bytes == 0`, or a non-finite or non-positive `cpu_share`, is rejected (`"Invalid task resource estimate"`, or `"Invalid native resource estimate"` for the shared pool).

`ResourceManager::new()` builds a `sysinfo::System`. **If the monitor cannot start, `snapshot` and `acquire` fail closed** with `Hardware resource monitor unavailable` rather than guessing free memory.

## Capability estimates

`capability_demand(name)` returns a fixed demand with `cpu_share = 1.0`:

| Operation names | Memory reservation |
| --- | --- |
| `document_open`, `document_save_copy`, `document_export_docx`, `document_export_xlsx` | 384 MiB |
| `helper_plugin_inspect`, `helper_plugin_install`, `plugin_run_installed` | 512 MiB |
| anything else | `None` (no admission) |

Callers found in the source: `crates/capabilities/src/lib.rs`, `crates/runtime/src/host.rs` and `crates/engine/src/tools/mod.rs`. In `capabilities`, the result is passed to the shared acquire and then `scope_shared`.

## Shared admission pool (`shared.rs`)

`shared::acquire(directory, demand, cancel)` returns a `SharedResourceLease` (not `Clone`; exposes `directory()` and `id()`). README: the host's local `ResourceManager` still accounts for its own work, and the native service acquires the shared pool **exactly once**. The canonical directory is `data/native-management/admission` under the app profile.

```rust,ignore
let lease = xylo_resources::shared::acquire(directory, demand, cancel).await?;
xylo_resources::scope_shared(std::sync::Arc::new(lease), async {
    xylo_resources::spawn_blocking(move || bounded_native_work()).await?
}).await
```

### Ledger and liveness

- Each reservation is a UUID-named set of files: `<id>.lock` (held OS exclusive lock), `<id>.partial` then atomically renamed to `<id>.json` (record `{format: 1, id, memory_bytes, cpu_share}`).
- Queue tickets are `<id>.queuelock` (held lock) and `<id>.queue` (sequence, demand).
- Sums count only records whose lock is actually held. **Liveness never uses PIDs or timestamps**; a dead process or dropped lease releases its OS lock and the next admission reaps the files. No `fsync` is used because a power loss also releases every OS lock.
- A short `metadata.lock` serializes admission and reaping.

### Limits

| Limit | Value |
| --- | --- |
| Active reservations | 128 (`MAX_ACTIVE`); at the limit callers queue |
| Queued tickets | 256 (`MAX_WAITERS`); beyond that, error "Too many native operations are queued" |
| Record size | 1 KiB (`MAX_RECORD_BYTES`) |
| Directory entries | `128*3 + 256*2 + 1` (`MAX_DIRECTORY_ENTRIES`), else "exceeds its metadata budget" |
| Metadata lock wait | 30 s, then an explicit error and nothing is started |
| Queue recheck | every 250 ms |

### Queueing

Admission is first come, first served across processes. A caller that cannot start takes a ticket with the next sequence number and is admitted only when its demand fits **and** no earlier live ticket could start once current reservations end (`fits_when_idle`: available memory plus reserved memory, capped at total, with CPU treated as idle). A waiter that could not start even then (other programs hold the memory) does not block later requests; it keeps its place. This stops a large request being starved by small ones. Tickets of crashed or cancelled waiters are reaped by the same lock probe.

### Refusals

`prepare_directory` and `open_regular` fail closed on: relative paths or `.`/`..` components; symbolic links or non-directory parents; (Unix) a directory not owned by the current user, which is otherwise tightened to `0700` when group/other bits are set; files with more than one hard link or special files; unknown file names or extensions, non-canonical or nil UUIDs; oversize records; unsupported `format`; unknown JSON fields (`deny_unknown_fields`); active-count or accounting overflow. Corrupt metadata on a live lease fails closed; dead incomplete records are reaped. Files are created with mode `0600` and opened with `O_NOFOLLOW | O_NONBLOCK` on Unix.

## Keeping a reservation alive

`scope_shared(Arc<SharedResourceLease>, future)` installs a task-local guard. This crate's `spawn_blocking` clones it, and the current action lease, into the actual closure. **The OS lock is released only when the closure finishes**, even if the `JoinHandle` or the outer future is dropped or times out. Ordinary `tokio::spawn` does not inherit task-local state, so scope a new task explicitly. Plain `tokio::task::spawn_blocking` does not capture the guard and must not replace the wrapper for admitted work.

`action_lease.rs` does the same for an `Arc<File>` action lease: `scope_action` for async work and `ActionLeaseScope::enter` (thread-local, not `Send`) for synchronous broker dispatch. Its docs state this context does not cover native subprocess lifetimes, since clients can close inherited handles.

## macOS memory estimate (`macos_memory.rs`)

On macOS only, `snapshot` replaces sysinfo's `available_memory` with `macos_memory::available_memory(total, fallback)`:

- Counters come from `host_statistics64(HOST_VM_INFO64)` plus `sysconf(_SC_PAGESIZE)`. The estimate is `(free_count + inactive_count) * page_size`, saturating and **clamped to installed memory**. Purgeable pages are not added (they may overlap other categories), speculative pages are not added again (already in `free_count`), and compressor pages are not subtracted.
- It is used **only** when `sysctlbyname("kern.memorystatus_vm_pressure_level")` succeeds with exactly `1` (normal). Pressure is sampled after the counters. Warning, critical, other values, an unavailable query, a failed or short Mach response, or a page size that is not a power of two all return the sysinfo value, `min(total)`.
- The `mach_host_self` send right is wrapped in a guard that calls `mach_port_deallocate` on every exit path. The code uses no private syscall or entitlement and does not use `os_proc_available_memory`.
- This is an admission estimate, not a promise that inactive pages can be reclaimed at once. Headroom, reservations and cancellation still apply. Linux and Windows use the sysinfo value unchanged. The README records one live macOS comparison; it is an observation, not a capacity guarantee.

## Concurrency and error behavior

- `ResourceManager` state is behind a `std::sync::Mutex`; a poisoned lock yields `Resource monitor unavailable`. Queued `acquire` calls wake on a lease drop or after a 500 ms timer, and exit when the token is cancelled with `AdmissionCancelled`.
- `shared::acquire` runs each attempt (hardware sample, directory preparation, locks, file IO) on `tokio::task::spawn_blocking`, so it **does not block async or UI threads**. Cancellation is observed during the wait and between steps and returns an error "Native work cancelled while queued". This is an ordinary `anyhow` error, not `AdmissionCancelled`. Dropping the acquire future cancels its child token.
- Dropping a `SharedResourceLease` only closes its handle on the calling thread. Later admissions delete the stale files.
- The shared pool keeps a process-wide `sysinfo::System` behind a `Mutex`, and also fails closed when the monitor is unavailable.

## Security

- The directory must be app-owned, absolute and symlink-free; contents are validated strictly (see Refusals). The README advises a local filesystem so file locks have normal process-lifetime semantics.
- Records contain only an ID and numeric demand; no prompts, paths or credentials.
- Estimates do not sandbox anything; native code still needs its own limits.

## Testing

`cargo test --locked -p xylo-resources` (not run while writing this page). The README's offline invocation is `CARGO_INCREMENTAL=0 cargo test --locked --offline -p xylo-resources --all-targets`. Tests use controlled snapshots (shared accounting, changing pressure, cancellation, reaping, corrupt metadata, path refusal, reservation surviving cancellation of the outer task) rather than allocating large memory. The macOS live comparison is `macos_memory::tests::live_counter_comparison`.

## Related crates

[runtime](runtime.md) and [capabilities](capabilities.md) acquire admission for native operations; [engine](engine.md) uses `capability_demand` and `suggested_parallelism`. Shared types live in [core](core.md). The helper process is described in [helper](helper.md).

## Known documentation gaps

The README mentions "pinned sysinfo 0.31.4" while `Cargo.toml` declares `sysinfo = "0.40"`; treat the README version number as historical.
