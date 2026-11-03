# Native Xylo design implementation plan

**Goal:** Apply the installed `apple-design` and `emil-design-eng` guidance to Xylo while keeping the implementation native Rust and reducing unnecessary work.

**Architecture:** Keep GPUI, the existing semantic palette, platform fonts and existing actions. Change shared presentation rather than adding a UI framework. Remove decorative navigation animations and their bookkeeping; preserve useful loading, task feedback and reduced-motion support.

**Constraints:** Preserve existing uncommitted work, app features, credential handling and sandbox boundaries. No new dependencies, web UI or runtime. Use the existing `--no-default-features` build for UI iteration so it does not build the bundled browser. No automatic commits or installation over the user's running app.

- [x] Refine shared light/dark surfaces, control states and focus treatment. Verified 30 text/surface pairs with a minimum 4.85:1 contrast.
- [x] Refine the chat home, starter hierarchy, composer elevation and compact toolbar. Preserve starter prompts, drafts and all action IDs. Use compact starter rows below 620 px and a separate welcome scroll area to keep the composer visible.
- [x] Remove sidebar entrance/retitle/pulse animation bookkeeping and navigation delays in settings, search and task details.
- [x] Verify with Rust formatting, desktop compilation/unit tests, focused native interactions, and a rebuilt desktop preview. Reviewed light/dark appearance, starter draft population, keyboard search and compact split panes.

**Review focus:** Long project/chat names must truncate; starters must only fill editable drafts; model/permission controls must fit split views; keyboard search must open immediately; running chats remain visibly identified without continuous sidebar animation.

**Verification:** `cargo build -p xylo --locked --no-default-features` passed; `cargo test -p xylo --locked --no-default-features` passed all 206 tests; `cargo native-test --test composer-interactions` passed all 46 tests; `cargo fmt -p xylo -- --check` and `git diff --check` passed. Five existing warnings in services remain outside this UI change. The Windows preview uses synthetic settings and project content under `target/design-preview`, without executing a model request. The installed app was not replaced.

**Verification fixes:** Made the startup argument fixture use a platform-native absolute path. Replaced a stale private-module test reference with the turn's public phase and explicitly supplied its intended Thinking event in the synthetic fixture.
