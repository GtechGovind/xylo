# Architecture

## Process boundaries

```text
GPUI desktop
  └─ Application host
       ├─ SQLite executor and OS credentials
       ├─ Resource scheduler and schedule dispatcher
       ├─ Supervised task workers
       │    └─ Hermes adapter → application ToolBroker
       └─ Native capability helper
            ├─ Browser / local inference / speech
            ├─ Document extraction and export
            ├─ Desktop capture and input
            └─ Wasmtime / SSH / container execution
```

The UI owns product state and renders changed views. Host commands are bounded and return immediately. Blocking work uses dedicated actors or blocking pools. Task workers exchange versioned JSONL messages through private pipes; standard output carries protocol messages and standard error carries logs. Frames, queues, output previews and model/tool concurrency have explicit bounds.

`core` is the replacement boundary for a future engine. Conversations, settings, action receipts, credentials, memory, skills and artifacts belong to the application. Community engine types do not enter UI or storage contracts. The helper client does not link Chromium, Wasmtime or inference engines into the desktop executable.

## Project discovery

Project metadata lives in a separate rebuildable SQLite catalogue. The host streams bounded scan batches and native event deltas; the native file browser and search retain one page each. No project-sized path list crosses the host/UI boundary. See [project files and search](native-workspace.md) for limits, continuation rules and remaining platform qualification.

## Durable execution

1. Register each user turn idempotently, preserving the conversation's original title.
2. Acquire a task resource lease before starting execution.
3. Record each tool's intent and approval before executing it.
4. Commit a successful result before advancing the agent checkpoint.
5. Commit final response, assistant history and completion receipt together.

On restart, completed results are reused. Interrupted writes become uncertain rather than silently retrying. A failure to persist a checkpoint stops execution. Child agents use namespaced call identifiers and the same tool broker; they cannot bypass approval by using an alternate registry.

Scheduled occurrences derive a stable turn identifier from the schedule and occurrence time. The scheduler uses file locks, atomic updates, expiring claims and renewal. Its current cron interpretation is UTC; the interface must make that visible until local timezone conversion is implemented.

## Resource scheduling

Admission considers live available memory, current CPU activity and reserved task demand. It does not map physical RAM to fixed task counts. A small system headroom remains available for the operating system and interaction. Waiting tasks can be cancelled before execution. Leases release on completion, cancellation, worker failure or startup error.

Application-owned single-generation inference endpoints use a shared lease. Existing external model servers keep their own concurrency policy. Heavy models are lazy and can unload when idle. Browser memory, model weights and caches must be included in active-memory measurements.

## Security and user intent

Provider keys are referenced through platform credential stores, never stored as plaintext settings. Local and remote connectors share application policy. File operations are constrained to canonical project roots; generated artifacts are additionally readable. Explicit user actions may configure native helpers without spending a model turn.

Learned facts and procedures retain scope, provenance and versions. Promotion follows evaluation, and rollback preserves history. Learning does not grant tool permissions or train model weights. Native desktop capture/input requires actual platform permissions; library support alone does not establish Linux Wayland compatibility.

## Maintenance rules

- Keep UI presentation separate from engine and infrastructure details.
- Preserve bounded memory and cancellation when adding a tool or provider.
- Use native argv APIs; never interpolate user arguments into a shell command.
- Record source revisions, compatibility patches and dependency licenses.
- Keep profile data and credentials out of source control and diagnostic exports.
- Verify new behavior with real process or protocol fixtures where isolation matters.
