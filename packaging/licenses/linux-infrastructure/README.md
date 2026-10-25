# AppImage infrastructure notices and source

These files preserve upstream notices for the exact AppRun and AppImage runtime identities in `catalogue.json`. They do not replace the application's Cargo inventory or the separate inventory of libraries copied into its AppImage. The original upstream text bytes are retained under `texts/` with hashes and primary source URLs.

## AppRun

The launcher is the AppImageKit `src/AppRun.c` implementation at commit [`5735cc5bed206497cddfbd2a75e1982c2606c35d`](https://github.com/AppImage/AppImageKit/tree/5735cc5bed206497cddfbd2a75e1982c2606c35d). The AppImageKit license and the complete source file are included; the latter retains the additional RazZziel copyright notice in its header. The Tauri `apprun-old` release identifies itself as a mirror of the official AppImageKit release. Both architectures are accepted only when their complete bytes match the reviewed hashes.

## Runtime source and rebuild links

The runtime is the official AppImage `type2-runtime` build at commit [`fedfad51acc2860b2916a1eef32ba4a500ff31ef`](https://github.com/AppImage/type2-runtime/tree/fedfad51acc2860b2916a1eef32ba4a500ff31ef). Its source is available as an [exact-commit source archive](https://github.com/AppImage/type2-runtime/archive/fedfad51acc2860b2916a1eef32ba4a500ff31ef.tar.gz).

The repository includes the [Docker build recipe](https://github.com/AppImage/type2-runtime/blob/fedfad51acc2860b2916a1eef32ba4a500ff31ef/scripts/docker/build-with-docker.sh), [Dockerfile](https://github.com/AppImage/type2-runtime/blob/fedfad51acc2860b2916a1eef32ba4a500ff31ef/scripts/docker/Dockerfile), [dependency build script](https://github.com/AppImage/type2-runtime/blob/fedfad51acc2860b2916a1eef32ba4a500ff31ef/scripts/common/install-dependencies.sh), [runtime build script](https://github.com/AppImage/type2-runtime/blob/fedfad51acc2860b2916a1eef32ba4a500ff31ef/scripts/build-runtime.sh), and [link command](https://github.com/AppImage/type2-runtime/blob/fedfad51acc2860b2916a1eef32ba4a500ff31ef/src/runtime/Makefile). The dependency script pins the libfuse and squashfuse source archives by SHA256 and includes the [libfuse patch](https://github.com/AppImage/type2-runtime/blob/fedfad51acc2860b2916a1eef32ba4a500ff31ef/patches/libfuse/mount.c.diff).

The [official build run](https://github.com/AppImage/type2-runtime/actions/runs/39320578745) records native x64 and ARM64 builds and the release upload. The release publishes runtime executables, debug files and signatures. It does not publish a separately identified complete corresponding-source or relinking-object bundle.

The release build used Alpine 3.21 with image digest `sha256:ce64758a109eb420d874a118f87920e625e12d3634e03b4a5573fd9f6e5d3507` and the following linked dependencies:

| Component | Observed version | Source and distribution modifications |
| --- | --- | --- |
| musl | 1.2.5-r11 | [Official 1.2.5 source](https://musl.libc.org/releases/musl-1.2.5.tar.gz); [Alpine recipe and patches](https://github.com/alpinelinux/aports/tree/8999cf56154b9bd09c995f293d8dfd3aa8001e3a/main/musl) |
| libfuse | 3.15.0 | [Official source](https://github.com/libfuse/libfuse/releases/download/fuse-3.15.0/fuse-3.15.0.tar.xz); runtime's pinned patch linked above |
| squashfuse | 0.5.2 | [Official source](https://github.com/vasi/squashfuse/archive/0.5.2.tar.gz) |
| Zstandard | 1.5.6-r2 | [Upstream source](https://github.com/facebook/zstd/tree/794ea1b0afca0f020f4e57b6732332231fb23c70); [Alpine recipe](https://github.com/alpinelinux/aports/tree/5c2ddf18f193dafdd25ae7391d96c7e30766eecd/main/zstd) |
| zlib | 1.3.2-r1 | [Upstream source](https://github.com/madler/zlib/tree/da607da739fa6047df13e66a2af6b8bec7c2a498); [Alpine recipe and patch](https://github.com/alpinelinux/aports/tree/c973d294927355eac1d2a6e97d86e41fbab8ffda/main/zlib) |
| mimalloc | 2.1.7-r0 | [Upstream source](https://github.com/microsoft/mimalloc/tree/8c532c32c3c96e5ba1f2283e032f69ead8add00f); [Alpine recipe and patch](https://github.com/alpinelinux/aports/tree/5a1a5dc3216a69e3cd66a57ee07cb5ef01b8be39/community/mimalloc2) |

The upstream Dockerfile uses an Alpine tag and live package repositories. These links and observed versions improve source traceability; they do not establish a fully frozen, bit-for-bit reproducible build environment. No source rebuild was performed as part of this notice preparation.

## Scope and limits

The runtime is MIT licensed and statically links the libraries listed above. Its upstream license explicitly names musl, libfuse, squashfuse, Zstandard and zlib. The pinned link command also includes mimalloc; its original MIT notice is retained here. The BSD license branch is used for Zstandard.

libfuse's library is LGPL 2.1; its original license selection, full LGPL text and authors file are included. These notice files and source links are not a legal approval or a qualification of complete corresponding-source and relinking distribution obligations. musl's original COPYRIGHT retains its third-party exceptions and source-file notice references.

Runtime download URLs use the upstream continuous release. Downloaded bytes must match the complete pinned size and SHA256 before use. The packaged runtime must match again after zeroing only its verified 16-byte `.digest_md5` ELF section, which [appimagetool writes during packaging](https://github.com/AppImage/appimagetool/blob/854e19e951900cf141245b384d45735942521abe/src/appimagetool.c#L1077-L1119). Other differences are rejected; an upstream update requires a new reviewed catalogue.
