# Echo plugin

This small component returns the text supplied to it. It requires no file,
network, model, or process permissions. The application can load its WAT source
directly; no separate compiler, Python, or Node runtime is required.

1. Open **Settings → Plugins** in the development preview.
2. Choose **Choose plugin package** and select `plugin.json` from this directory.
3. Review the package name and permissions, then install and enable it.
4. Enter a short message and run the plugin. Its result should match your input.

Use at most 8,192 UTF-8 bytes. This deliberately minimal example uses one fixed
memory region; it is a demonstration of the component contract, not a general
allocator or an SDK template for larger plugins. Other plugins need an allocator
appropriate to their own component bindings.

Changing `echo.wat` changes the package identity. Update the manifest's SHA-256
checksum and review/install the changed package before running it.

The example is application-owned Apache-2.0 code; see the repository LICENSE.
