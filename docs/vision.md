# Native image attachments

Workbench imports images only after an explicit native picker, dropped-file or
**Paste image** action. Native clipboard image pixels and copied file entries
are imported through the bounded native service. No provider, model or image
download starts on render. The user selects a provider and model; a provider
rejection never selects another model or paid service.

## Storage and import

`workbench_core::ImageAttachment` is a small session-owned reference with UUID,
name, media type, SHA-256, byte size, dimensions, source and import timestamp.
`TurnRequest.attachments` and `ChatMessage.attachments` default to an empty vector
for old records. The reference contains no executable path, remote URL, base64 or
original source path. Atomic user-turn registration includes references in its
idempotency comparison. Display history and engine checkpoints preserve them.

`workbench_services::attachments::AttachmentStore` accepts native user-selected
paths on the host's blocking executor. It sniffs the bytes, decodes with the Rust
`image` library, applies EXIF orientation and imports into
`data/artifacts/images/SESSION/IMAGE_ID/`. Receipts and hashes bind each reference
to its original, provider variant and preview. Reads reject altered receipts,
corrupt files, symbolic links and a different session. Regular imported-file
names are generated internally; the user-facing name never forms a path.

Each import stages and syncs complete files before atomically promoting its
directory. Failed or cooperatively cancelled imports remove their staging
directory. Cancellation is checked while reading and between decode/resize/
encode operations. Individual native codec calls are bounded by the input and
decode limits; they are not forcibly interrupted mid-call. The host serializes
imports and reserves 384 MiB of resource admission per import. A cancellable
app-data file lease also serializes native decode across UI and worker processes;
waiting is bounded to 30 seconds and does not hold a lock during provider HTTP.

| Boundary | Product limit |
| --- | --- |
| Images per user message | 4 |
| Original encoded file | 8 MiB; PNG, JPEG, WebP or GIF |
| Original image | 40 megapixels, 16,384 pixels per edge |
| Decoded image buffer | 192 MiB ceiling before allocation; codec internal allocation limits also apply |
| Provider variant | First frame, at most 4,096 pixels per edge and 3 MiB |
| Preview | First frame, at most 256 pixels per edge and 512 KiB |
| Hydrated request | At most 16 images and 16 MiB of encoded image content |
| Native preview cache | 128 paths, at most 32 in-flight preview/import requests |

Provider copies preserve PNG/alpha as PNG, otherwise use JPEG quality 90. Images
are never enlarged. Oversized variants are reduced progressively until they fit.
Animation is represented by its first frame. Originals remain unchanged for
durable integrity and future export; providers receive only the bounded variant,
without the original metadata. Removing a composer card removes that draft
reference. It does not delete previously sent image history or its managed file.

## Provider and protocol path

The engine marks a user message with typed `workbench_image_attachment` metadata.
`AttachmentProvider` loads verified managed copies and base64-encodes them on a
blocking worker only for the actual HTTP request. The agent's original message
list remains reference-only. An 8 MiB worker JSONL limit therefore applies to
small references, rather than image payloads. A conservative visual patch reserve
is included in the configured context budget; it is not a billing estimate.

Native adapters send actual images using their selected API:

| API family | Wire content |
| --- | --- |
| OpenAI Chat, compatible/custom endpoints, Azure | `image_url` with a managed inline data URI |
| OpenAI Responses, eligible ChatGPT plan authentication | `input_image` with the same inline bytes |
| Anthropic | `image` block with a base64 `source` |
| Gemini and Google Vertex | `inlineData` with MIME type and base64 data |
| Amazon Bedrock | Converse `image` block with format and base64 `source.bytes` |

The selected server/model must accept image input. Native adapters reject remote
image URLs rather than asking a provider to fetch them. The attachment reader
has no remote fetch operation and cannot turn a model-supplied path into an
outside-file read. Generation is a separate provider operation and is not
implemented by the image-input path.

Native UI controls correlate every import and preview result to its request,
session and image UUID. Late imports for another chat are ignored. Send is held
while an image import is pending. Draft persistence owns attachment references;
the attachment UI owns only request state and bounded managed preview paths.

## Verification

Offline native tests cover all four image formats, actual decode/copy/preview,
signature sniffing despite a wrong extension, corrupt/tampered/symbolic-link
rejection, cross-session rejection, oversized input/dimensions, cancelled import
cleanup and SQLite reopen/idempotency. Deterministic HTTP fixtures exercise
streamed image requests for all seven API families, including Bedrock's binary
event stream, and compare the transmitted decoded bytes with the managed copy.
Dedicated worker and native UI regression targets qualify process restart,
clipboard-file/drop/remove, native clipboard pixels, restored previews and stale import correlation.
These fixtures do not establish an external paid-account or model's visual
accuracy, and native platform desktop acceptance remains separately recorded.

Focused results on the macOS host (2026-10-25): native attachment service 7/7;
engine 40/40, including all seven streamed API cases and ref-only checkpoint
resume; real worker/restart 1/1; native vision UI target 10/10 (two new image
interaction tests and eight included regressions). The UI fixture recreates
draft/history/preview state under one profile host, deletes the original selected
file and reopens the conversation from SQLite. The separate worker fixture
actually terminates and starts another process. Native UI strict Clippy passes.

```sh
CARGO_INCREMENTAL=0 cargo +stable test -p workbench-services attachments::tests
CARGO_INCREMENTAL=0 cargo +stable test -p workbench-engine --lib
CARGO_INCREMENTAL=0 cargo +stable test -p workbench-worker --test vision_protocol
CARGO_TARGET_DIR="$PWD/target" CARGO_INCREMENTAL=0 cargo +stable test \
  --manifest-path crates/desktop/validation/Cargo.toml --test vision-interactions
```

## Primary API references

- [OpenAI images and vision](https://developers.openai.com/api/docs/guides/images-vision)
- [Anthropic vision](https://platform.claude.com/docs/en/build-with-claude/vision)
- [Gemini Generate Content image input](https://ai.google.dev/gemini-api/docs/generate-content/image-understanding)
- [Bedrock Converse image input](https://docs.aws.amazon.com/bedrock/latest/userguide/conversation-inference.html)
- [Bedrock Message limits](https://docs.aws.amazon.com/bedrock/latest/APIReference/API_runtime_Message.html)
- [Rust image decoder limits](https://docs.rs/image/0.25.10/image/struct.Limits.html)

## Native clipboard pixels

The explicit **Paste image** action reads GPUI's native clipboard entries. It
accepts copied image files or a single encoded image entry; it does not decode
or re-encode pixels on the UI thread. The host moves at most 8 MiB to its blocking
image importer under the shared resource/decode gate. PNG, JPEG, WebP and GIF
retain their original encoded bytes; clipboard BMP and TIFF are decoded with the
same dimension/allocation limits, oriented and encoded to bounded PNG before
managed import. SVG and other unimplemented clipboard formats fail explicitly.
File-picker/drop imports retain the four-format contract above. Clipboard
provenance is persisted in the attachment reference. No clipboard read happens
on page load.

Image generation now has its own explicit provider operation; see
[image-generation.md](image-generation.md). Image-input adapters do not infer an
image-generation model or initiate generation.
