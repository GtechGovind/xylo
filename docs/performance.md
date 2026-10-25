# Performance measurement

Performance claims require raw samples and an identified build. The headless runner measures the real worker process, native file tool, durable journal and memory implementation with a local scripted provider. It never uses a paid provider or downloads model weights.

```sh
cargo build --locked --release -p workbench-worker
cargo run --locked --release -p workbench-runtime --example measure_overhead -- \
  --worker target/release/workbench-worker \
  --output work/performance/release-overhead.json \
  --samples 100 \
  --memories 10000
```

To measure semantic retrieval, explicitly supply an installed and activated embedding configuration:

```sh
cargo run --locked --release -p workbench-runtime --example measure_overhead -- \
  --worker target/release/workbench-worker \
  --output work/performance/release-semantic.json \
  --embedding-config /absolute/path/to/embedding.json \
  --samples 100 \
  --memories 10000
```

Index preparation embeds the full fixture corpus and is reported separately. Query text varies across timed samples so an embedding cache cannot substitute for query inference. Warm-up queries run before timing. Increase `--memories` to `100000` for the large-index workload. The runner accepts 10–1,000 samples and 100–100,000 memories to bound accidental workloads.

Reports contain raw millisecond samples, nearest-rank p50/p95, build profile, OS/architecture, hardware memory/CPU count, worker binary SHA-256, corpus size and embedding mode. The runner validates that normal turns execute a real file tool and that cancellation interrupts an active streaming response. A failed functional assertion fails the measurement run.

## Interpretation

- `cold_worker_ready` measures process launch to protocol readiness. It does not measure native GUI launch-to-usable time.
- `warm_worker_jsonl_roundtrip` includes framing, queueing and native pipes.
- `worker_turn_started` includes worker turn admission but excludes GPUI and the application host.
- `file_tool_roundtrip_including_journal` includes native file I/O, durable action bookkeeping and event delivery. It does not isolate broker dispatch overhead.
- `active_cancellation_ack` measures receipt of the control acknowledgement while the provider stream is active; task termination is a separate event.
- `warm_lexical_retrieval` excludes embeddings. It cannot establish the semantic retrieval target.
- `warm_hybrid_retrieval_including_embedding` includes the explicitly configured local embedding model, vector search and returned records.
- `warm_worker_rss_bytes` is one resident-memory observation. It is not attributable process-tree RAM or macOS physical footprint.
- `warm_worker_physical_footprint_bytes` reads the macOS process ledger when available. It covers that worker alone, not the application, host or child-process tree. Other platforms report no value rather than relabeling RSS.

The report also identifies the measurement executable, accepts an explicit `--source-fingerprint SHA256`, and records system load. A release binary measured while an IDE, indexer or build is busy is development evidence, not a controlled release acceptance run.

GUI launch/input/frame pacing, full process-tree memory, browser/inference/speech pressure, concurrent tasks, 100 connectors and task-quality evaluation require additional native OS runs. No headless measurement substitutes for these release gates. Debug reports are development evidence and must not be compared directly with release acceptance targets.

## Native rendering workload

The standalone validation workspace has an opt-in measurement executable using the production UI, native fonts and real offscreen Metal/WGPU rendering. It keeps the same release optimization settings as the application:

```sh
CARGO_TARGET_DIR="$PWD/target/performance" cargo build --locked --release \
  --manifest-path crates/desktop/validation/Cargo.toml --bin native-performance
target/performance/release/native-performance \
  --output .ecc/benchmarks/native-rendering.json --samples 100 --messages 10000
```

It warms ten iterations before recording virtual-history scrolling and eight-delta streaming batches. Every sample records CPU update/frame preparation separately from a fresh offscreen GPU render, target allocation, wait and pixel readback. One initialization sample includes the first rendered home screen and GPU rendering/readback. Large-history preparation is outside warm timing.

The dispatcher and window are test implementations. There is no connected application host or model. The runner does not measure native window creation, physical keyboard/IME delivery, display presentation or dropped frames. Its initialization sample cannot establish launch p95, and CPU frame throughput cannot establish 60 Hz. Record the executable hash and source manifest alongside each raw report.

## Recorded development run: 2026-10-25

On a 32 GiB, 10-thread ARM64 Mac running macOS 27.0.1, the debug harness completed 100 scripted turns: 80 real file-tool turns and 20 active cancellations. The run exposed an immediate-next-turn busy race; the worker now releases its busy state after engine cleanup and before delivering its buffered terminal event. A regression replays 32 completed turns without an intervening delay or process restart.

| Headless measurement | Samples | p95 |
| --- | ---: | ---: |
| Warm worker JSONL round trip | 100 | 0.072 ms |
| Active cancellation acknowledgement | 20 | 0.156 ms |
| File tool plus journal/event delivery | 80 | 0.387 ms |
| Complete scripted file-tool turn | 80 | 12.691 ms |
| Warm lexical retrieval, 10,000 memories | 100 | 33.666 ms |
| Warm hybrid retrieval including BGE embedding, 10,000 memories | 100 | 82.677 ms |

The semantic run used `BAAI/bge-small-en-v1.5@5c38ec7c405ec4b44b94cc5a9bb96e735b38267a`, CLS pooling, 384 dimensions and native ONNX Runtime 1.31.0. Preparing the 10,000-vector corpus took 111.7 seconds and is excluded from warm query timing. The reports include verified model/runtime hashes and raw samples.

Cold worker readiness had a p95 of 2,068 ms across ten fresh data profiles in the first run, with a median of 8.37 ms. This includes the first execution of the development binary and does not establish the app's cold-launch target. The warm worker RSS observation was 36.7 MB; it excludes the UI, host and embedding process and is not the attributable-memory acceptance metric.

These are uncontrolled development measurements; other development checks may have been active. The generated corpus measures retrieval cost, not LongMemEval quality. The debug source and the later packaged binary can differ; binary hashes identify each run. Release, GUI, sustained load and cross-platform qualification remain open.

Raw evidence: [worker and lexical run](evidence/2026-10-25-debug-overhead.json), [semantic run](evidence/2026-10-25-debug-semantic-overhead.json).

## Recorded release development run: 2026-10-25

The optimized binaries were measured on the same 32 GiB, 10-thread ARM64 Mac. Each headless report contains 100 scripted turns: 80 real file-tool turns and 20 cancellations during an active HTTP response stream. No Cargo build was active when these runs started. The IDE and ordinary system activity remained active; the reports include system load, and these are uncontrolled development baselines. They do not establish a performance gain over another implementation or language.

| Measurement | Samples | p95 |
| --- | ---: | ---: |
| Warm worker JSONL round trip | 100 | 0.035 ms |
| Active cancellation acknowledgement | 20 | 0.063 ms |
| File tool plus journal/event delivery | 80 | 0.271 ms |
| Complete scripted file-tool turn | 80 | 3.739 ms |
| Warm lexical retrieval, 10,000 memories | 100 | 7.585 ms |
| Warm hybrid retrieval including BGE query embedding, 10,000 memories | 100 | 27.192 ms |
| Warm lexical retrieval, 100,000 memories | 100 | 113.738 ms |

The first five rows come from the 10,000-memory lexical report; the remaining retrieval rows come from their separately identified reports. The semantic run used the pinned BGE model described above, 384 dimensions and CLS pooling. Its configuration requests automatic CPU-thread selection. Corpus preparation took 101.6 seconds for 10,000 embedded records; the separate 100,000-record lexical corpus took 33.9 seconds. Preparation is excluded from warm queries. The 100,000-record run did not enable embeddings and cannot establish semantic retrieval latency or recall quality at that scale.

The first lexical run recorded worker readiness p95 of 748.223 ms across ten fresh data profiles, including a 748.223 ms first-execution observation and a 6.627 ms median. Later semantic and large-index runs recorded 6.181 ms and 8.585 ms p95 respectively. Preserve the first-run observation; binary validation and filesystem caches were not controlled. None of these measures native application launch. macOS physical-footprint observations ranged from 8,339,984 to 8,389,160 bytes for the worker alone. They exclude the UI, host, embedding model/runtime and all other helpers.

The native rendering executable separately exercised 10,000 synthetic history messages in a 1,380 × 880 logical viewport. After ten warm-up iterations, it recorded 100 samples per scrolling/streaming measurement:

| Offscreen native rendering measurement | Samples | p95 |
| --- | ---: | ---: |
| Scroll state update, layout and CPU frame submission | 100 | 1.338 ms |
| Eight streaming deltas, state update and CPU frame submission | 100 | 0.616 ms |
| Fresh scroll GPU render, allocation, wait and pixel readback | 100 | 6.431 ms |
| Fresh streaming GPU render, allocation, wait and pixel readback | 100 | 5.877 ms |

Initialization through the first offscreen GPU render/readback took 311.460 ms in one observation. The runner emits percentile fields even for that single observation; it is not a launch-p95 estimate. Physical raster scale was not recorded. CPU preparation and GPU readback are different timed operations; adding their percentiles does not produce a display frame budget. Native window presentation, physical input, IME, sustained frame pacing and dropped frames remain unmeasured.

The release exercise found a cancellation race: a typed provider interruption could reach the generic worker-error path before cancellation handling. The engine now accepts that interruption as cancellation only when its own cancellation token is set, cleanup succeeds and the retained checkpoint is valid. Other provider, cleanup and checkpoint errors remain errors, and uncertain external writes remain unresolved. A real worker regression performs 64 active-stream cancellations, with deterministic engine cases covering both paths. The failed pre-fix run is retained separately from the four successful reports.

The 10,000-record semantic fixture meets the numerical retrieval target on this machine and build. Full product acceptance still requires the complete application route, resource-pressure and cross-platform runs, process-tree memory, 100,000-record semantic retrieval, 100 configured connectors and task-quality evaluation. The historical debug table is a separate build and workload history, not an optimization comparison.

Evidence: [worker and lexical run](evidence/2026-10-25-release-overhead.json), [semantic run](evidence/2026-10-25-release-semantic-overhead.json), [100,000-record lexical run](evidence/2026-10-25-release-100k-overhead.json), [native rendering](evidence/2026-10-25-release-native-rendering.json), [source, binaries and measurement manifest](evidence/2026-10-25-release-benchmark-manifest.json). The manifest links the retained pre-fix failure and exact lock/source hashes.

## Recorded native document paging run: 2026-10-25

The document workload uses the native API with a generated two-sheet XLSX: a 100-row sheet beside a 12,000-row sheet, eight filled columns each and 96,800 filled cells in total. It checks all 1,024 typed coordinates returned by every 64 × 16 page, including blanks, Unicode, booleans, numeric lexical values and formula/text distinction. Validation and typed decoding happen outside timing. Each timed operation includes source-file reading and revision hashing, native document admission/parsing, and result JSON serialization.

```sh
CARGO_TARGET_DIR="$PWD/target/performance" cargo build --locked --release \
  -p workbench-capabilities --example measure_documents
target/performance/release/examples/measure_documents \
  --output /absolute/path/document-paging.json \
  --source-fingerprint REVIEWED_SOURCE_MANIFEST_SHA256 --samples 80
```

| Varied warm page workload | Samples | p50 | p95 |
| --- | ---: | ---: | ---: |
| Small selected sheet beside the large sibling | 40 | 1.556 ms | 1.660 ms |
| Large selected sheet | 40 | 34.788 ms | 38.557 ms |

Fixture generation took 59.459 ms. The first small-sheet open, including the process metadata/shared-string cache miss, took 4.800 ms; the first large-sheet page took 51.788 ms. Those are single preparation observations, not cold percentiles. The complete fixture and query workload took 1.630 seconds. Warm requests retain revision-keyed metadata/shared strings while still parsing the selected worksheet and checking source content. This is not a comparison against the earlier implementation.

The run used an Apple M5, 10 threads, 32 GiB and macOS 27.0.1. No compiler was active at measurement start; ordinary developer/system activity remained, with one-minute load 2.403. The report identifies the reviewed source manifest, compiled harness and lock hashes, binary hash, fixture hash and individual sample windows. These direct API results exclude helper startup/IPC, host routing/resource admission, the document UI, display/input timing and save-copy operations. They do not prove full application responsiveness, Office formula calculation, live-document behavior or cross-platform acceptance.

Evidence: [raw document paging report](evidence/2026-10-25-release-document-paging.json), [identified source](evidence/2026-10-25-document-validated-source-manifest.json), [optimized harness build](evidence/document-performance-build.log).

## Recorded installed-plugin run: 2026-10-25

The native installed-plugin workload executes one permission-free component-model echo package through `CapabilityService::execute`. Each timed call includes shared cross-process resource admission, catalog/lease/package checks, cached component instantiation, fuel-bounded guest execution and JSON serialization. Returned input is checked after timing; all 80 warm inputs vary.

```sh
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$PWD/target/performance" cargo build --locked --release \
  -p workbench-capabilities --example measure_plugins
target/performance/release/examples/measure_plugins \
  --output /absolute/path/plugin-execution.json \
  --source-fingerprint REVIEWED_SOURCE_MANIFEST_SHA256
```

| Installed-component warm workload | Samples | p50 | p95 |
| --- | ---: | ---: | ---: |
| Direct native echo, including admission and package checks | 80 | 0.473 ms | 0.627 ms |

Installation validates and compiles the component and may populate its cache. Service construction, package inspection, installation and the first post-install call are separate single observations in the raw report; they are not cold percentiles. No compiler was active when measurement started; ordinary system activity remained uncontrolled.

This tiny package has no host imports, network or model. The measurement excludes helper startup/IPC, host admission, approvals, editor/terminal work and UI/display/input. It measures neither representative extension quality, concurrent load, process-tree memory, full application responsiveness nor a gain over another language or implementation. It cannot substitute for the internal-dispatch or full-product acceptance targets.

macOS admission now uses native free plus inactive physical pages only under normal kernel pressure, retaining the prior conservative fallback otherwise. The arithmetic, fallback, lease ownership and headroom tests are functional evidence; this change does not establish a measured speed improvement. Its SDK rationale and point-in-time live counter comparison are documented in [native admission](../crates/resources/README.md). The earlier plugin-fixture timeout was not reproduced and is not attributed to this correction.

Evidence: [raw plugin report](evidence/2026-10-25-release-plugin-execution.json), [source and binary manifest](evidence/2026-10-25-plugin-benchmark-manifest.json), [current build validation](build-validation.json). Earlier worker, memory, rendering and document reports retain their original source identities.

## Recorded recovery milestone baseline: 2026-10-25

The current optimized worker and measurement executable were rebuilt from source fingerprint `497a50dea191c4785427d0d78304dd2f9f4c358ee1c777b9b6bd4a904ee4eea1`. On the same 32 GiB, 10-thread ARM64 Mac, the scripted provider completed 80 actual file-read turns and 20 cancellations during an active response stream. No Cargo or Rust compiler process was active at measurement start. Ordinary development and system activity remained uncontrolled; the recorded one-minute load was 4.148.

| Headless operation | Samples | p50 | p95 |
| --- | ---: | ---: | ---: |
| Fresh worker launch to protocol readiness | 10 | 7.515 ms | 762.812 ms |
| Warm worker JSONL round trip | 100 | 0.024 ms | 0.035 ms |
| Worker turn admission | 100 | 0.832 ms | 0.978 ms |
| File tool plus journal/event delivery | 80 | 0.281 ms | 0.349 ms |
| Complete scripted file-tool turn | 80 | 3.466 ms | 4.242 ms |
| Active cancellation acknowledgement | 20 | 0.053 ms | 0.092 ms |
| Warm lexical retrieval, 10,000 memories | 100 | 7.482 ms | 7.638 ms |

The readiness result retains the first 762.812 ms observation. Ten fresh data profiles do not control filesystem caches or first-execution binary validation, and this is not native application launch timing. Corpus preparation took 2.044 seconds and is excluded from warm retrieval. Embeddings were disabled. One warm worker observation recorded 25,935,872 bytes RSS and 8,569,360 bytes macOS physical footprint; neither value covers the UI, host or helper processes.

This is a separate development baseline, not a controlled comparison with earlier milestones or another language. It exercises worker pipes, storage and a real file-read tool, while excluding application-host routing, GUI/display timing, live models, model quality, semantic retrieval and full process-tree resource use. Cancellation acknowledgement does not establish task termination latency. The existing plugin, document, semantic and offscreen-rendering reports retain their original source identities.

Evidence: [raw recovery baseline](evidence/2026-10-25-recovery-release-overhead.json), [source and binary manifest](evidence/2026-10-25-recovery-benchmark-manifest.json), [current validation record](build-validation.json).
