<div align="center">
  <h1>Xylo</h1>
  <h3>Your projects. Your models. One workspace.</h3>
  <p>A native desktop workspace for AI conversations, coding and everyday tools.</p>
  <p>
    <a href="#what-you-can-do">Features</a> ·
    <a href="#get-started">Get started</a> ·
    <a href="https://github.com/GtechGovind/xylo/releases">Releases</a> ·
    <a href="#contribute-and-get-help">Contribute</a> ·
    <a href="#license">License</a>
  </p>
  <p><strong>Open source · Development preview · macOS, Windows and Linux targets</strong></p>
</div>

Xylo keeps conversations, project files and practical tools together. Bring a local model or connect your preferred provider, work with your project in context, and return to saved chats and drafts.

## What you can do

| Workspace                  | Capabilities                                                                                               |
|----------------------------|------------------------------------------------------------------------------------------------------------|
| **Work with your project** | Browse and search files, use the native editor and terminal, and review Git diffs alongside chat.          |
| **Choose your model**      | Connect local model servers, cloud providers or a compatible endpoint. Choose models explicitly.           |
| **Keep useful context**    | Save project facts, search conversation history, and review reusable skills.                               |
| **Stay in control**        | Review tool approvals and inspect interrupted actions before recording an observed outcome.                |
| **Work with documents**    | Edit supported DOCX content and multi-sheet XLSX files, save a separate copy, and recover retained drafts. |
| **Connect your tools**     | Add MCP connections, review plugin permissions, and configure SSH or container environments.               |

Browser actions, images, voice and local model tools extend the workspace through services that start when used. Some features need an explicit asset download, a configured service or an operating system permission. The implementation and qualification status are described below and in the project source.

Xylo uses a native interface, background execution and automatic local resource scheduling. Responsiveness is a design priority; performance measurements are still being qualified.

## Local or cloud, by your choice

Use an existing **Ollama, LM Studio, llama.cpp or vLLM** server, or configure a supported local model through Xylo's asset setup. Connect **OpenAI, Anthropic, Gemini** and other supported cloud providers in Settings. Custom endpoints are supported where the provider protocol matches.

Choose the provider and model under **Settings → Model**. Credentials use the operating system credential store. Available tools, image support and context limits depend on the selected model and provider.

## Get started

**Xylo is in development preview. The first downloadable release is being prepared.** Follow the [Releases page](https://github.com/GtechGovind/xylo/releases) for published files and release notes, or build from source with standard Cargo commands:

```sh
cargo build --locked --workspace --bins
npm ci --prefix crates/services/sandbox-helper --ignore-scripts --no-audit --no-fund
cargo run --locked
```

The pinned Rust toolchain is selected by `rust-toolchain.toml`. Application builds do not require packaging scripts, icon files, or updater configuration. Automatic updates are disabled until a trusted release feed is configured.

Local executable tools use a pinned OS sandbox helper and require Node.js 22.12 or later. Xylo uses a bundled runtime first, then `XYLO_SANDBOX_NODE` if set, then `node` from `PATH` (including nvm, volta, fnm or asdf installs under your home directory) as long as the binary and its parent directories are owned by you or root and are not writable by other users; otherwise sandboxed tools report which Node.js was rejected and why. Install its locked dependencies with the command above. Missing sandbox dependencies fail closed; Xylo does not fall back to unrestricted execution. The native UI and Rust backend do not run in Node.js. macOS sandbox execution has local regression coverage; Linux packaging and Windows execution remain unqualified.

Portable skills and plugins are managed in **Settings → Plugins**. Installation preserves package contents without executing them. Enable packages explicitly; configure a project, credentials and network access before running executable components. Readiness and verification belong to each component and installed version, not the entire plugin ecosystem.

Current extension qualification uses isolated local fixtures:

| Component | Current execution coverage |
|-----------|----------------------------|
| Skills and commands | Explicit/automatic invocation, arguments, package resources, Codex invocation policy and approved dynamic context |
| Agent definitions | Named roles, supervised forks, restricted tools, inherited provider/model and cancellation |
| Hooks | Approved before/after-tool events; session lifecycle events are not automatically delivered |
| Local MCP | Initialization, tool discovery/calls, configured credentials, cancellation and failure recovery |
| Language servers | Initialization, bounded read requests and shutdown; automatic editor integration is not qualified |
| WASM modules | Existing Wasmtime execution and permission boundary retained |

Unbound hosted dependencies, unsupported host directives and command-pattern tool restrictions are reported explicitly. Live account integrations, arbitrary Git-host federation and packaged cross-platform runtimes still require qualification. Post-tool events currently have a narrow crash window between saving the tool result and creating the hook action; they are not guaranteed to be delivered across that interruption. Never infer ecosystem-wide compatibility from a successful import or one verified component.

| Platform | Target architectures     | Planned preview formats |
|----------|--------------------------|-------------------------|
| macOS    | Apple Silicon, Intel x64 | DMG, app archive        |
| Windows  | ARM64, x64               | Installer EXE           |
| Linux    | ARM64, x64               | AppImage, DEB           |

These are release targets, not a claim that every platform is already qualified. Packaging, signing and installation limits should be checked in the release artifacts and workflow before installing a preview.

Once the app is running:

1. Open a project folder.
2. Choose a provider and model in **Settings → Model**.
3. Start a conversation and review requested actions as you work.

## Contribute and get help

Bug reports, documentation, platform testing and focused pull requests are welcome. [Open an issue](https://github.com/GtechGovind/xylo/issues) with steps to reproduce and your platform/model details. Remove credentials and private content before sharing logs.

## Support Xylo

GitHub Sponsors for **GtechGovind** is being prepared; **activation is pending**. If you would like to support the work, [express sponsorship interest](https://github.com/GtechGovind/xylo/issues/new?title=Sponsorship%20interest) or help with testing, issues and documentation.

## License

Xylo's application-owned code is licensed under [Apache-2.0](LICENSE). Dependencies retain their own licenses and notices; see [NOTICE](NOTICE) and the notices distributed with third-party components.
