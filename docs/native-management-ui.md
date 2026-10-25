# Native Plugins and Environments

Both pages open from Settings. They use the same native, left-aligned navigation as the rest of the application. Package installation, permission changes, connections and commands require explicit actions; opening a page or restoring saved settings does not run a plugin or start a connection.

## Plugins

Choose **Plugins → Choose plugin package** and select the package's `plugin.json`. The application reviews its name, version and requested permissions before installation. The entrypoint is resolved from that manifest; there is no separate module-path override. Installation is bound to the exact package that was reviewed. Changed files require another review.

Permission switches initially grant nothing, and **Enable after installation** initially stays off. Installation and updating are separate explicit actions; replacing an installed ID requires an **Update plugin** confirmation. The saved list displays installed versions. Selecting a plugin exposes its saved enable state and editable permission choices. **Save permissions** acknowledges the actual saved result, **Revoke permissions** requires confirmation, and removing a plugin requires confirmation. Existing artifacts are retained.

**Run plugin** uses the selected installed package and the current project, with the input field's text. Results appear as text and actual artifact links. Failed and uncertain runs remain failures or uncertain results even when they have already produced an artifact. Revoking a permission changes the next run's access; it does not claim to undo earlier files or network requests.

An interrupted management request retains its original operation ID and exact arguments. **Check previous change** replays that same idempotent control, rather than constructing a second installation or revocation. Refreshing the installed catalog shows the confirmed state. Dismissing a local uncertain request is explicit and does not undo a change that may already have completed.

The bundled [Echo example](../examples/plugins/echo/README.md) requires no model, account or external compiler. See [native plugin contracts and boundaries](native-plugins.md) for package and permission details.

## Environments

Choose **Environments → New environment** and select SSH, Docker or Podman. A saved profile has an explicit name and target. Available client programs are detected without starting them; a native file picker can select a different client. Saving a profile does not contact a host, create a container or run a command.

SSH uses an existing agent or an optional selected identity file, plus a selected verified hosts file. The page does not ask for a password or silently trust a new host key. Container profiles can refer to an existing container or explicitly manage a new container from an image and optional startup command. The setup form uses product labels; native file selections are shown by filename.

Saved profiles expose **Connect**, **Disconnect** and **Check saved connection**. Managed container profiles also expose **Create container**, **Start container**, **Stop container** and **Remove container**. A stored connected flag alone never enables **Run command** after controller restoration or a project change; a fresh connection result or status check must confirm it. Commands are parsed into arguments, and are not executed through the local shell.

Removing a managed container or profile requires a native confirmation. A profile must be disconnected, and its managed container removed, before the profile itself can be removed. Removing a container does not request force removal or volume deletion.

Cancellation means the request is being stopped, not that a remote action had no effect. Unconfirmed action IDs remain in **Previous actions needing a result check**, including after reopening the page. **Check result** reconciles that original ID. Prepared or running actions remain blocked; a definitively cancelled, failed or successful receipt resolves the local pending state. When the helper confirms an unknown result, **Record observed result** requires an observation note and confirmation. Checking or recording never repeats a command. See [native environment contracts and platform limits](native-environments.md).

## Editing, scope and close behavior

Saved packages and environment profiles belong to the application profile. Each plugin run uses the project selected for that run. Active changes and runs block project switching and Quit/update installation until their response arrives or the cancelled request has settled. Read-only results from a retired project are ignored by their dedicated controller; even late responses beyond the small retired-ID cache cannot overwrite another page's messages.

Environment typing can continue while Save is waiting. A successful response acknowledges exactly the submitted form; text typed afterward remains visible and unsaved. Unsaved setup choices stay in memory across project switches. Changing an environment selection or quitting/installing an update requires a discard confirmation when setup edits remain. These unsaved setup forms are not yet durable across an unexpected process failure; saved profiles, package state and operation receipts are durable.

The package picker and setup confirmations are explicit native user flows. Automated validation supplies isolated picker results and uses test prompts; it never opens a real permission dialog, SSH connection or container daemon. Native UI interaction evidence is recorded separately from live-platform acceptance. The focused `management-interactions` target exercises the actual Host and helper processes with the shipped Echo package, permission-scoped components, temporary profiles, and native-client protocol fixtures.

## Focused native evidence

On macOS arm64, the final focused run on 2026-10-25 passed **15 tests, 0 failures, 0 ignored**: seven new management interaction cases and eight shared state/terminal/file utility cases. The management cases use GPUI controls, actual Host/helper processes and isolated profile storage.

| Native flow | What is exercised |
| --- | --- |
| Shipped Echo package | Review, explicit enable/install, native input, actual component execution and restored catalog without automatic execution |
| Reader and writer packages | Actual project reads, confirmed permission revocation causing access denial, actual output artifact and file contents |
| Saved SSH profile | Save without connecting, explicit local fixture connection/command/disconnection, restored saved profile without a new launch |
| Delayed Save acknowledgment | Real profile lease blocks Save while native typing continues; project/Quit guards; cancellation and original-context receipt check; later explicit Save preserves the new text |
| Managed container | Create/start/connect, cancel a dispatched command, restore its original operation ID, check and explicitly record its observed result without replay, disconnect/stop, cancel and confirm resource removal |
| Restored global pages | Plugins and Environments initially load even when no project is selected |
| Retired results and unsaved setup | Late feature results cannot replace unrelated product messages; Keep editing preserves a dirty setup form |

The integrated run is recorded in [build validation](build-validation.json), with [raw native test output](evidence/native-ui-tests.log). Reproduce the focused target from the repository root after building the sibling helper/worker binaries:

```sh
CARGO_INCREMENTAL=0 CARGO_TARGET_DIR="$PWD/target" cargo test --offline --locked \
  --manifest-path crates/desktop/validation/Cargo.toml \
  --test management-interactions -- --test-threads=1 --nocapture
```

The real native-client fixtures are Unix-only and do not qualify live OpenSSH/Docker/Podman versions, Windows behavior, unlocked visual appearance, accessibility tools, or a real account/daemon. The desktop was locked during this validation, so no visual acceptance or native file-picker dialog is claimed. The native fixture passes the selected package/client paths into the same production controllers and tests confirmations through the GPUI test prompt interface.

The [Plugins](native-preview/plugins.png) and [Environments](native-preview/environments.png) captures use actual offscreen native rendering in a controlled no-Host setup. They retain the resulting request-failure notices and loading/setup views; they do not represent a connected installation. Heading instrumentation belongs only to the screenshot harness. Loaded, saved and completed states are exercised by the real Host/helper interaction fixtures.
