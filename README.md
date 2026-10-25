<div align="center">
  <h1><img src="packaging/branding/xylo-lockup.png" alt="Xylo" width="480"></h1>
  <h3>Your projects. Your models. One workspace.</h3>
  <p>A native desktop workspace for AI conversations, coding and everyday tools.</p>
  <p>
    <a href="#get-started">Get started</a> ·
    <a href="https://github.com/GtechGovind/xylo/releases">Releases</a> ·
    <a href="#documentation">Documentation</a> ·
    <a href="docs/sponsorship.md">Support Xylo</a>
  </p>
  <p><strong>Open source · Development preview · macOS, Windows and Linux targets</strong></p>
</div>

Xylo keeps conversations, project files and practical tools together. Bring a local model or connect your preferred provider, work with your project in context, and return to saved chats and drafts.

## What you can do

| Workspace | Capabilities |
| --- | --- |
| **Work with your project** | Browse and search files, use the native editor and terminal, and review Git diffs alongside chat. |
| **Choose your model** | Connect local model servers, cloud providers or a compatible endpoint. Choose models explicitly. |
| **Keep useful context** | Save project facts, search conversation history, and review reusable skills. |
| **Stay in control** | Review tool approvals and inspect interrupted actions before recording an observed outcome. |
| **Work with documents** | Edit supported DOCX content and multi-sheet XLSX files, save a separate copy, and recover retained drafts. |
| **Connect your tools** | Add MCP connections, review plugin permissions, and configure SSH or container environments. |

Browser actions, images, voice and local model tools extend the workspace through services that start when used. Some features need an explicit asset download, a configured service or an operating system permission. [Explore capabilities →](docs/capabilities.md)

Xylo uses a native interface, background execution and automatic local resource scheduling. Responsiveness is a design priority; [performance notes](docs/performance.md) describe the scope of the available measurements.

## Local or cloud, by your choice

Use an existing **Ollama, LM Studio, llama.cpp or vLLM** server, or configure a supported local model through Xylo's asset setup. Connect **OpenAI, Anthropic, Gemini** and other supported cloud providers in Settings. Custom endpoints are supported where the provider protocol matches.

Choose the provider and model under **Settings → Model**. Credentials use the operating system credential store. Available tools, image support and context limits depend on the selected model and provider. See the [provider guide](docs/providers.md) for setup and compatibility.

## Get started

**Xylo is in development preview. The first downloadable release is being prepared.** Follow the [Releases page](https://github.com/GtechGovind/xylo/releases) for published files and release notes, or [build the preview from source](docs/development.md#run-the-preview) now.

| Platform | Target architectures | Planned preview formats |
| --- | --- | --- |
| macOS | Apple Silicon, Intel x64 | DMG, app archive |
| Windows | ARM64, x64 | Installer EXE |
| Linux | ARM64, x64 | AppImage, DEB |

These are release targets, not a claim that every platform is already qualified. Preview signing and installation limits are documented in the [release guide](docs/preview-release.md).

Once the app is running:

1. Open a project folder.
2. Choose a provider and model in **Settings → Model**.
3. Start a conversation and review requested actions as you work.

## Documentation

- [Models and providers](docs/providers.md) · [Native assets](docs/assets.md)
- [Project memory](docs/memory-management.md) · [Saved chats and drafts](docs/drafts.md)
- [Documents and recovery](docs/native-documents.md) · [Interrupted action review](docs/action-review-ui.md)
- [Plugins](docs/native-plugins.md) · [Execution environments](docs/native-environments.md)
- [Modern native terminal direction](docs/terminal-roadmap.md)
- [Build, contribute and browse the full docs index](docs/development.md)
- [Current verification and remaining acceptance](docs/completion-audit.md)

## Contribute and get help

Bug reports, documentation, platform testing and focused pull requests are welcome. [Open an issue](https://github.com/GtechGovind/xylo/issues) with steps to reproduce and your platform/model details, or read the [contribution guide](docs/development.md#contributing). Remove credentials and private content before sharing logs.

## Support Xylo

GitHub Sponsors for **GtechGovind** is being prepared; **activation is pending**. If you would like to support the work, [express sponsorship interest](https://github.com/GtechGovind/xylo/issues/new?title=Sponsorship%20interest) or help with testing, issues and documentation. [Sponsorship details and setup →](docs/sponsorship.md)

## License

Xylo's application-owned code is licensed under [Apache-2.0](LICENSE). Dependencies retain their own licenses and notices; see [NOTICE](NOTICE), the vendored notices and [dependency license documentation](docs/license-supplements.md).
