# Vendored block 0.1.6

Upstream: Steven Sheldon, https://github.com/SSheldon/rust-block
Release: 0.1.6, commit 47178790cfc9d4a8b092051d8b413b78bd31254a.
License: MIT, as declared by the retained upstream Cargo.toml license field.
The distributed crate did not include a separate license file. LICENSE-MIT
reproduces the standard MIT permission text with the upstream author attribution.

Library source, test module, and README are from crates.io block 0.1.6.
The missing upstream C-interop test_utils directory is from the pinned release.

Local changes:
- Replace the uninhabited private Class marker with an inhabited opaque repr(C)
  marker. Only its address is stored in the existing isa pointer; no symbol
  storage is read and no public layout or signature changes.
- Spell the existing default Rust 2015 edition explicitly in both manifests.
- Spell the existing default C ABI explicitly at extern declarations to avoid
  missing_abi warnings exposed when this dependency is built from a local path.
- Use maintained cc instead of obsolete gcc only in the upstream C test fixture
  build script. Original C code and six upstream library test bodies unchanged.

Remove this patch when the native toolkit adopts a compatible maintained Blocks
binding. Both workspace and independent native-validation manifests select it.
