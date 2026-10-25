//! Opt-in scheduling A/B replay. This measures the old publish-per-read pattern
//! and the production coalescer with identical parsing, bytes and viewport. It
//! is not a PTY transport, GPU, interactive latency or release benchmark.
use super::{coalescing_tests::*, *};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Default)]
struct CaptureStats {
    frames: usize,
    construction_ns: u128,
}

fn capture(
    state: &Mutex<State>,
    updates: &watch::Sender<TerminalSnapshot>,
    stats: &Mutex<CaptureStats>,
) {
    let state = state.lock();
    let start = Instant::now();
    let frame = snapshot(&state);
    let elapsed = start.elapsed().as_nanos();
    drop(state);
    updates.send_replace(frame);
    let mut stats = stats.lock();
    stats.frames += 1;
    stats.construction_ns += elapsed;
}

fn replay(bytes: &[u8], coalesced: bool) -> (Value, TerminalSnapshot) {
    let (state, listener) = terminal_state(120, 40);
    let signal = Arc::new(FrameSignal::default());
    let stats = Arc::new(Mutex::new(CaptureStats::default()));
    let (updates, receiver) = watch::channel(TerminalSnapshot::default());
    let publisher = coalesced.then(|| {
        let state = state.clone();
        let signal = signal.clone();
        let stats = stats.clone();
        let updates = updates.clone();
        thread::spawn(move || {
            signal.publish(FRAME_INTERVAL, || capture(&state, &updates, &stats));
        })
    });
    let (commands, _incoming) = mpsc::sync_channel(64);
    let mut parser = alacritty_terminal::vte::ansi::Processor::new();
    let start = Instant::now();
    let mut parsing_ns = 0;
    for packet in bytes.chunks(8192) {
        let parse_start = Instant::now();
        parse_output(&mut parser, &state, &listener, packet, &commands, &signal).unwrap();
        parsing_ns += parse_start.elapsed().as_nanos();
        if !coalesced {
            capture(&state, &updates, &stats);
        }
    }
    let producer_ns = start.elapsed().as_nanos();
    state.lock().exited = true;
    if let Some(publisher) = publisher {
        signal.request(true);
        signal.close();
        publisher.join().unwrap();
    } else {
        capture(&state, &updates, &stats);
    }
    let complete_ns = start.elapsed().as_nanos();
    let stats = stats.lock();
    let frame = receiver.borrow().clone();
    (
        json!({
            "producer_elapsed_ns": producer_ns,
            "complete_elapsed_ns": complete_ns,
            "parser_and_dispatch_elapsed_ns": parsing_ns,
            "snapshot_construction_elapsed_ns": stats.construction_ns,
            "snapshots": stats.frames,
        }),
        frame,
    )
}

fn nearest_rank(samples: &[Value], field: &str, percentile: usize) -> u64 {
    let mut values: Vec<_> = samples
        .iter()
        .map(|sample| sample[field].as_u64().unwrap())
        .collect();
    values.sort_unstable();
    values[(values.len() * percentile).div_ceil(100).saturating_sub(1)]
}

fn file_hash(path: &Path) -> String {
    let mut file = std::fs::File::open(path).unwrap();
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    format!("{:x}", hash.finalize())
}

#[test]
#[ignore = "Opt-in matched scheduling measurement; run alone with --nocapture"]
fn measure_snapshot_output_burst() {
    let line = "\x1b[32mcompile\x1b[0m source.rs:42 界e\u{301} output progress 0123456789\r\n";
    let mut bytes = line.repeat((2 * 1024 * 1024) / line.len()).into_bytes();
    bytes.extend_from_slice(b"\x1b]2;Measured tail\x07\r\nCOMPLETE\r\n");
    let mut baseline = Vec::new();
    let mut optimized = Vec::new();
    // Warm both paths once, then alternate their order in matched pairs.
    for pair in 0..7 {
        let ((before, before_frame), (after, after_frame)) = if pair % 2 == 0 {
            (replay(&bytes, false), replay(&bytes, true))
        } else {
            let after = replay(&bytes, true);
            (replay(&bytes, false), after)
        };
        assert_same_frame(&after_frame, &before_frame);
        assert!(after_frame.exited);
        assert_eq!(after_frame.title, "Measured tail");
        if pair != 0 {
            baseline.push(before);
            optimized.push(after);
        }
    }
    let summary = |samples: &[Value]| {
        let mut value = serde_json::Map::new();
        for field in [
            "producer_elapsed_ns",
            "complete_elapsed_ns",
            "parser_and_dispatch_elapsed_ns",
            "snapshot_construction_elapsed_ns",
            "snapshots",
        ] {
            value.insert(
                field.into(),
                json!({
                    "p50":nearest_rank(samples, field, 50),
                    "p95":nearest_rank(samples, field, 95)
                }),
            );
        }
        Value::Object(value)
    };
    let mut system = sysinfo::System::new();
    system.refresh_memory();
    let report = json!({
        "schema":1,
        "kind":"native terminal snapshot scheduling A/B replay",
        "qualification":"debug test binary; synthetic parser workload, not PTY/GPU/input-to-paint latency",
        "baseline":"publish-per-read reconstruction pattern; identical current parser and reliable reply dispatch",
        "optimized":"production event-driven 16ms snapshot coalescer with final flush",
        "hardware":{
            "os":std::env::consts::OS,
            "arch":std::env::consts::ARCH,
            "os_version":sysinfo::System::long_os_version(),
            "cpu_threads":std::thread::available_parallelism().map(usize::from).unwrap_or(1),
            "memory_bytes":system.total_memory(),
        },
        "source_sha256":format!("{:x}",Sha256::digest(include_bytes!("../terminal.rs"))),
        "cargo_lock_sha256":format!("{:x}",Sha256::digest(include_bytes!("../../../../Cargo.lock"))),
        "test_binary_sha256":file_hash(&std::env::current_exe().unwrap()),
        "workload":{
            "sha256":format!("{:x}",Sha256::digest(&bytes)),
            "bytes":bytes.len(),
            "packet_bytes":8192,
            "packets":bytes.len().div_ceil(8192),
            "columns":120,
            "rows":40,
            "warmup_pairs":1,
            "sample_pairs":baseline.len(),
            "order":"alternating; parser state recreated each run",
        },
        "baseline_summary":summary(&baseline),
        "optimized_summary":summary(&optimized),
        "baseline_raw":baseline,
        "optimized_raw":optimized,
        "final_frame_equivalence_verified":true,
        "limits":[
            "The baseline is an explicit replay of old snapshot scheduling, not an old release binary.",
            "Parser-and-dispatch elapsed time includes synchronization; snapshot construction excludes terminal-lock wait.",
            "Accumulated timings may include scheduler preemption and are not thread CPU counters.",
            "Burst wall time and snapshot cadence do not establish steady interactive latency or an application speedup."
        ],
    });
    let json = serde_json::to_string_pretty(&report).unwrap();
    if let Some(path) = std::env::var_os("XYLO_TERMINAL_SNAPSHOT_REPORT") {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        file.write_all(json.as_bytes()).unwrap();
    }
    eprintln!("{json}");
}
