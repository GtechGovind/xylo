# Native work admission

This lightweight crate is shared by the desktop host, helper and task worker. It retains the existing `ResourceManager`, `ResourceSnapshot` and `TaskDemand` API and adds a profile-wide pool for heavy native work. Estimates are scheduling reservations, not measured RSS or OS-enforced memory limits.

`capability_demand(name)` defines the common estimates: document open/copy/new Office export reserves 384 MiB and one CPU share; plugin inspection/install and installed-plugin runs reserve 512 MiB and one CPU share. Other operations return `None`. Actual parse, archive, guest, fuel, IO and compilation bounds remain the responsibility of their native service.

Acquire the shared reservation from the canonical app-owned `data/native-management/admission` directory. The host's local manager still accounts for its own concurrent work; the native service acquires this shared pool exactly once rather than each process independently spending the same available headroom:

```rust,ignore
let lease = xylo_resources::shared::acquire(directory, demand, cancel).await?;
xylo_resources::scope_shared(std::sync::Arc::new(lease), async {
    xylo_resources::spawn_blocking(move || bounded_native_work()).await?
}).await
```

`scope_shared` installs a task-local `Arc` guard. This crate's `spawn_blocking` captures that guard inside the actual native closure. Dropping or timing out the caller/JoinHandle does not release accounting while the closure is still compiling, parsing or publishing. Ordinary `tokio::spawn` does not inherit task-local state; explicitly scope a new task when it owns native work. Ordinary `tokio::task::spawn_blocking` does not capture this guard and must not replace the wrapper on admitted work.

The pool maintains at most 128 active records of at most 1 KiB. Each UUID record has a separately held exclusive OS lease file. A short global metadata lock serializes admission/reaping. Memory and CPU demand are summed only for records whose lease is actually held. A dead process or dropped guard releases the OS lock; the next admission safely removes its stale metadata. Liveness never depends on a PID, avoiding PID reuse errors. Corrupt live metadata fails closed; dead incomplete records are reaped. Unknown files, oversize records, traversal, symlinks and special files are refused. Storage should reside on the application's local filesystem, where native file locks have normal process-lifetime semantics.

Current memory availability, CPU usage, thread count and the existing automatic headroom calculation decide admission. There are no RAM tiers or fixed worker counts. Admission is first come, first served across processes: a caller that cannot start takes a queue ticket (an OS-locked `<id>.queuelock` plus an `<id>.queue` record with its sequence number), and nobody is admitted while a live ticket with a lower sequence exists. Tickets of crashed or cancelled waiters are reaped by the same lock probe as leases. Queued work rechecks every 250 ms and cancellation does not wait for filesystem operations. If another process holds the metadata lock for more than 30 seconds, admission fails with an explicit error and nothing is started. Snapshot, registry parsing, file IO and lock acquisition all run on blocking workers. A dropped lease only closes its OS handle on the calling thread; reaping performs later filesystem cleanup off the UI thread.

Run `CARGO_INCREMENTAL=0 cargo test --locked --offline -p xylo-resources --all-targets`. Controlled snapshots exercise shared accounting, changing pressure, cancellation, liveness reaping, corrupt metadata, path refusal and preservation of a blocking reservation after cancellation of its outer async task, without allocating model-sized memory.

## macOS physical-page estimate

On macOS only, snapshots use the public `host_statistics64(HOST_VM_INFO64)` counters and `_SC_PAGESIZE` to estimate reclaimable physical memory as `(free_count + inactive_count) * page_size`, clamped to installed memory. The pinned sysinfo 0.31.4 Apple implementation subtracts `compressor_page_count` from free + inactive + purgeable pages. Compressor storage already occupies a separate physical-page category, so subtracting it again can understate available capacity substantially. The correction does not add purgeable pages, which can overlap other categories, or speculative pages, which the SDK explicitly includes in `free_count`.

The native estimate is used only when a read-only `sysctlbyname("kern.memorystatus_vm_pressure_level")` succeeds with exactly the normal pressure flag, `1`. Warning, critical, unrecognized flags, unavailable queries, failed/short Mach responses and invalid page sizes retain the existing sysinfo estimate, bounded by installed memory. This kernel query is optional and may be absent or restricted on other macOS versions; that condition conservatively selects the fallback. Each `mach_host_self` send right has an RAII guard that calls `mach_port_deallocate` on success and failure paths. No process-memory allowance API is used: the local SDK marks `os_proc_available_memory` unavailable on macOS.

This is an admission estimate rather than a guarantee that inactive pages can all be reclaimed immediately. Snapshots and pressure can change between sampling and allocation. The existing 5%/minimum 256 MiB OS headroom, active local/shared reservations, cancellation and native workload limits remain in force. Linux and Windows continue using their existing sysinfo path.

The implementation was checked against the local macOS 27.0 SDK's public `mach/vm_statistics.h`, `mach/host_info.h`, `mach/mach_init.h`, `mach/mach_host.h`, `mach/mach_port.h`, `sys/sysctl.h` and `dispatch/source.h`. Apple's [kernel pressure query implementation](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_memorystatus_notify.c) converts its internal level to the public dispatch pressure flags before returning it. No private syscall, entitlement or memory-pressure simulation is required.

A live comparison on macOS 27.0.1 arm64, with 34,359,738,368 bytes of installed memory, recorded the following in `/tmp/xylo-macos-memory-tests.log`. These are one observation, not a performance or capacity guarantee:

| Counter or result | Value |
| --- | ---: |
| Page size | 16,384 bytes |
| Free pages | 114,250 |
| Inactive pages | 591,440 |
| Purgeable pages (not added) | 4,657 |
| Speculative pages (not added again) | 8,410 |
| Compressor physical pages (not subtracted) | 506,822 |
| Native pressure query | `Some(1)` / normal |
| sysinfo fallback | 3,334,635,520 bytes |
| Old formula using the same Mach counters | 3,334,553,600 bytes |
| Free + inactive estimate, selected | 11,562,024,960 bytes |
| Fallback used | `false` |

The small difference between sysinfo's value and the same-counter old formula reflects separate live samples. Reproduce the comparison without allocating workload-sized memory using `CARGO_INCREMENTAL=0 cargo test --locked --offline -p xylo-resources macos_memory::tests::live_counter_comparison -- --exact --nocapture`. Arithmetic, overflow/clamping, missing counters, elevated/unknown pressure and retained headroom/reservation behavior have deterministic unit coverage.

This independently verified counter correction does not establish the cause of the earlier parallel engine-fixture timeout. That historical timeout had no phase checkpoints and did not reproduce in the isolated or full-engine diagnostic runs. Its 20-second bound and cancellation cleanup remain unchanged.
