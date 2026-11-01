# Xylo patch to block 0.1.6

`block` 0.1.6 (reached through `metal`) is vendored here and applied with
`[patch.crates-io]` in the workspace `Cargo.toml`.

Changes from the published crate, all in `src/lib.rs`:

- `enum Class { }` is now `#[repr(C)] struct Class { _private: [u8; 0] }`.
  The upstream type is uninhabited, and a static of an uninhabited type
  (`_NSConcreteStackBlock`) is a future-incompatibility error
  (rust-lang/rust#74840). Only the static's address is used, so callers see
  no difference.
- Bare `extern` blocks and `extern fn` types now spell out `extern "C"`,
  which is the ABI they already used.

Drop this patch once `metal` depends on a `block` release (or `block2`)
without the uninhabited static.
