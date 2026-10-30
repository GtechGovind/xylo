//! Real native text/Metal rendering with a deterministic test dispatcher.
//! Measures CPU frame preparation and offscreen GPU rendering/readback separately. This cannot
//! establish WindowServer presentation, physical input latency or dropped frames.
#![allow(dead_code, unused_imports)]
extern crate gpui_kit as gpui;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/terminal_view.rs"]
mod terminal_view;
#[path = "../src/terminal_workspace.rs"]
mod terminal_workspace;
#[path = "../src/ui.rs"]
mod ui;
mod xylo {
    include!("../src/xylo.rs");
    impl Xylo {
        pub(crate) fn performance_prepare(&mut self, cx: &mut Context<Self>) {
            self.error = None;
            cx.notify();
        }
        pub(crate) fn seed_performance_history(&mut self, count: usize, cx: &mut Context<Self>) {
            self.messages = (0..count).map(|index| message(self.session,
                if index % 2 == 0 { "user" } else { "assistant" },
                format!("Message {index}\n\n{}\n\n```rust\nfn example() {{ println!(\"Hello\"); }}\n```", "A bounded paragraph with **formatted text**, Unicode αβ and a link. ".repeat(8))
            )).collect();
            self.error = None;
            self.sync_history_list(cx);
            cx.notify();
        }
        pub(crate) fn performance_git_fixture(&mut self, count: usize, cx: &mut Context<Self>) {
            use xylo_runtime::workspace::git::{GitStatus,GitChange,ChangeGroup};
            let root = std::path::PathBuf::from("/synthetic-performance-project");
            let changes = (0..count).map(|index| GitChange { path: format!("src/group-{}/file-{index}.rs",index%50).into(), original:None, index:'M', worktree:'M' }).collect();
            self.git_panel = work_panel::GitPanel::prepare(root, GitStatus {branch:"main".into(), changes, truncated:false},Default::default());
            self.git_panel.selected = Some(("src/group-0/file-0.rs".into(),ChangeGroup::Unstaged));
            self.diff_lines = (0..count).map(|index|format!("+ added line {index}" )).collect();
            self.settings.ui.work_panel_open=true;
            self.settings.ui.work_panel="changes".into();
            cx.notify();
        }
        pub(crate) fn performance_git_scroll(&mut self, index:usize,cx:&mut Context<Self>) {
            self.diff_scroll.scroll_to_item(index,gpui_kit::ScrollStrategy::Top);
            cx.notify();
        }
        pub(crate) fn performance_scroll(&mut self, index: usize, cx: &mut Context<Self>) {
            self.histories[&self.session].list.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            });
            cx.notify();
        }
        pub(crate) fn performance_stream(&mut self, cx: &mut Context<Self>) {
            let turn = self.turns.entry(self.session).or_default();
            if !turn.running {
                turn.apply(EngineEvent::Started);
            }
            for _ in 0..8 {
                turn.apply(EngineEvent::Delta {
                    text: "A streamed token batch αβ. ".into(),
                });
            }
            self.sync_history_list(cx);
            self.histories[&self.session].list.scroll_to_end();
            cx.notify();
        }
        pub(crate) fn performance_long_stream(&mut self, cx: &mut Context<Self>) {
            let turn = self.turns.entry(self.session).or_default();
            turn.apply(EngineEvent::Started);
            let mut text = "A long live response with **formatting** and Unicode αβ.\n\n".repeat(2048);
            let mut end = 102400.min(text.len());
            while !text.is_char_boundary(end) { end -= 1; }
            text.truncate(end);
            turn.apply(EngineEvent::Delta { text });
            self.sync_history_list(cx);
            self.histories[&self.session].list.scroll_to_end();
            cx.notify();
        }
        pub(crate) fn performance_composer_edit(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
            self.composer.update(cx, |input, cx| input.set_value(format!("Follow-up message {index} αβ"), window, cx));
            self.sync_history_list(cx);
            cx.notify();
        }
    }
}
use anyhow::{Context as _, Result, ensure};
use gpui_kit::{
    component::{Theme, ThemeMode},
    prelude::*,
    test::{TestQueryExt, TestWindowExt},
    *,
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Instant};

fn statistics(raw: &[f64]) -> Value {
    let mut sorted = raw.to_vec();
    sorted.sort_by(f64::total_cmp);
    let percentile = |fraction: f64| {
        sorted[((sorted.len() as f64 * fraction).ceil() as usize)
            .saturating_sub(1)
            .min(sorted.len() - 1)]
    };
    json!({"unit":"ms","count":raw.len(),"p50":percentile(0.5),"p95":percentile(0.95),"min":sorted[0],"max":sorted[sorted.len()-1],"raw":raw})
}
fn record(metrics: &mut BTreeMap<String, Vec<f64>>, name: &str, start: Instant) {
    metrics
        .entry(name.into())
        .or_default()
        .push(start.elapsed().as_secs_f64() * 1000.);
}
fn main() -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        measure()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        anyhow::bail!(
            "This offscreen native renderer is currently qualified only on macOS and Linux"
        )
    }
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn measure() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let option = |name: &str| {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|index| args.get(index + 1))
    };
    let output = PathBuf::from(option("--output").context("--output PATH is required")?);
    let samples: usize = option("--samples")
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(100);
    let count: usize = option("--messages")
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(10000);
    ensure!(
        (10..=1000).contains(&samples) && (100..=100000).contains(&count),
        "Use 10–1000 samples and 100–100000 messages"
    );
    let mut metrics = BTreeMap::new();
    let cold = Instant::now();
    let mut cx = HeadlessAppContext::with_platform(
        platform::current_platform(true).text_system(),
        Arc::new(gpui_kit::assets::Assets),
        platform::current_headless_renderer,
    );
    cx.update(|cx| {
        gpui_kit::init(cx);
        Theme::change(ThemeMode::Light, None, cx);
    });
    let (handle, view) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Default::default(),
                    size: size(px(1380.), px(880.)),
                })),
                focus: false,
                show: false,
                ..Default::default()
            },
            cx,
            |window, cx| {
                cx.new(|cx| {
                    xylo::Xylo::new(
                        Err(anyhow::anyhow!("Isolated rendering measurement")),
                        window,
                        cx,
                    )
                })
            },
        )
    })?;
    view.update(&mut cx, |view, cx| view.performance_prepare(cx));
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find_by_label("Add context").is_some(),
            "Initial production UI did not render"
        );
    })?;
    let _ = cx.capture_screenshot(handle)?.into_raw();
    record(
        &mut metrics,
        "offscreen_initialization_through_first_gpu_render_and_readback",
        cold,
    );
    view.update(&mut cx, |view, cx| view.seed_performance_history(count, cx));
    cx.update_window(handle, |_, window, cx| window.render_frame(cx))?;
    let _ = cx.capture_screenshot(handle)?.into_raw();
    for index in 0..(samples + 10) {
        let start = Instant::now();
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.performance_scroll(1 + ((index * 97) % count), cx)
            });
            window.render_frame(cx);
        })?;
        if index >= 10 {
            record(
                &mut metrics,
                "virtual_history_scroll_update_and_cpu_frame",
                start,
            );
        }
        // Capture renders the prepared scene into a fresh offscreen texture,
        // then waits and copies pixels to the CPU. It is not pure GPU timing
        // and neither this nor enqueue time establishes displayed frame rate.
        let start = Instant::now();
        let _ = cx.capture_screenshot(handle)?.into_raw();
        if index >= 10 {
            record(
                &mut metrics,
                "virtual_history_gpu_render_and_readback",
                start,
            );
        }
    }
    for index in 0..(samples + 10) {
        let start = Instant::now();
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |view, cx| view.performance_stream(cx));
            window.render_frame(cx);
        })?;
        if index >= 10 {
            record(&mut metrics, "stream_8_deltas_update_and_cpu_frame", start);
        }
        let start = Instant::now();
        let _ = cx.capture_screenshot(handle)?.into_raw();
        if index >= 10 {
            record(&mut metrics, "stream_gpu_render_and_readback", start);
        }
    }
    // Exercise composer renders with accumulated live output, beyond the short-stream baseline.
    view.update(&mut cx, |view, cx| view.performance_long_stream(cx));
    cx.update_window(handle, |_, window, cx| window.render_frame(cx))?;
    for index in 0..(samples + 10) {
        let start = Instant::now();
        cx.update_window(handle, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.performance_composer_edit(index, window, cx);
                view.performance_stream(cx);
            });
            window.render_frame(cx);
        })?;
        if index >= 10 {
            record(&mut metrics, "long_stream_100k_composer_edit_and_cpu_frame", start);
        }
    }
    // Additional current-design stress case; historical comparable metrics above stay unchanged.
    view.update(&mut cx, |view,cx|view.performance_git_fixture(5000,cx));
    for index in 0..(samples+10) {
        let start=Instant::now();
        cx.update_window(handle, |_,window,cx| { view.update(cx,|view,cx|view.performance_git_scroll((index*31)%5000,cx)); window.render_frame(cx); })?;
        if index>=10 { record(&mut metrics,"git_5000_changes_scroll_and_cpu_frame",start); }
    }
    let metrics: BTreeMap<_, _> = metrics
        .into_iter()
        .map(|(name, raw)| (name, statistics(&raw)))
        .collect();
    let report = json!({"schema_version":1,"profile":if cfg!(debug_assertions){"debug"}else{"release"},"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"width":1380,"height":880,"history_messages":count,"warmup_samples":10,"native_text_and_gpu":true,"host_connected":false,"git_fixture_changes":5000,"live_stream_seed_bytes":102400,"metrics":metrics,"limitations":["The production UI uses a deterministic test dispatcher and offscreen native renderer.","No real display presentation, physical keyboard/IME or WindowServer latency is measured.","CPU preparation and a fresh offscreen GPU render/texture allocation/wait/pixel readback are distinct metrics; neither proves 60Hz frame pacing or dropped-frame acceptance.","The initial sample is one process and excludes the host, workers, tray and native OS window creation.","Fixtures contain synthetic history and direct streaming state updates; there is no model or network timing."]});
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&output, serde_json::to_vec_pretty(&report)?)?;
    println!("Saved {}", output.display());
    Ok(())
}
