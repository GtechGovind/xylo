//! Offline native XLSX paging workload; no GUI, helper IPC, or comparison claims.
//!
//! CARGO_TARGET_DIR="$PWD/target/performance" cargo run --locked --release \
//!   -p xylo-capabilities --example measure_documents -- \
//!   --output /absolute/path/documents.json --source-fingerprint SHA256 --samples 80
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use xylo_capabilities::{CapabilityConfig, documents};
use xylo_core::{DocumentCellValue, DocumentContent, DocumentSnapshot, DocumentSource, now_ms};

const SMALL_ROWS: u32 = 100;
const LARGE_ROWS: u32 = 12_000;
const FILLED_COLUMNS: u16 = 8;
const PAGE_ROWS: u32 = 64;
const PAGE_COLUMNS: u32 = 16;
const EMBEDDED_LOCK: &[u8] = include_bytes!("../../../Cargo.lock");
const EMBEDDED_HARNESS: &[u8] = include_bytes!("measure_documents.rs");

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
fn file_hash(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(hex::encode(hash.finalize()))
}
fn arguments() -> Result<(PathBuf, String, usize)> {
    let mut output = None;
    let mut fingerprint = None;
    let mut samples = 80;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .with_context(|| format!("{flag} requires a value"))?;
        match flag.as_str() {
            "--output" => {
                ensure!(output.is_none(), "Duplicate --output");
                output = Some(PathBuf::from(value));
            }
            "--source-fingerprint" => {
                ensure!(fingerprint.is_none(), "Duplicate --source-fingerprint");
                fingerprint = Some(value);
            }
            "--samples" => samples = value.parse()?,
            _ => anyhow::bail!("Unknown argument: {flag}"),
        }
    }
    let output = output.context("--output PATH is required")?;
    ensure!(
        !output.exists(),
        "Evidence output already exists; choose a new path"
    );
    let fingerprint = fingerprint.context("--source-fingerprint SHA256 is required")?;
    ensure!(
        fingerprint.len() == 64 && fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Source fingerprint must be a SHA-256 identifier from the reviewed source manifest"
    );
    ensure!(
        matches!(samples, 80 | 100),
        "Choose 80 or 100 total warm page samples"
    );
    Ok((output, fingerprint, samples))
}
fn fixture(path: &Path, cancel: &CancellationToken) -> Result<()> {
    let mut workbook = rust_xlsxwriter::Workbook::new();
    let small = workbook.add_worksheet();
    small.set_name("Small")?;
    for row in 0..SMALL_ROWS {
        ensure!(!cancel.is_cancelled(), "Workload deadline reached");
        small.write_string(row, 0, format!("Small row {row}: café 🧾"))?;
        small.write_number(row, 1, f64::from(row * 8 + 1))?;
        small.write_boolean(row, 2, row.is_multiple_of(2))?;
        let formula = format!("=B{}+1", row + 1);
        small.write_formula(row, 3, formula.as_str())?;
        small.write_string(row, 4, "=literal text, never a formula")?;
        small.write_number(row, 5, -f64::from(row + 1))?;
        small.write_string(row, 6, format!("record-{row}"))?;
        small.write_number(row, 7, f64::from(row) + 0.5)?;
    }
    let large = workbook.add_worksheet();
    large.set_name("Large")?;
    for row in 0..LARGE_ROWS {
        ensure!(!cancel.is_cancelled(), "Workload deadline reached");
        for column in 0..FILLED_COLUMNS {
            large.write_number(row, column, f64::from(row * 8 + u32::from(column)))?;
        }
    }
    workbook.save(path)?;
    Ok(())
}
fn expected(sheet: &str, row: u32, column: u32) -> DocumentCellValue {
    use DocumentCellValue as Cell;
    if column >= u32::from(FILLED_COLUMNS) {
        return Cell::Blank;
    }
    if sheet == "Large" {
        return Cell::Number {
            value: (row * 8 + column).to_string(),
        };
    }
    match column {
        0 => Cell::Text {
            text: format!("Small row {row}: café 🧾"),
        },
        1 => Cell::Number {
            value: (row * 8 + 1).to_string(),
        },
        2 => Cell::Boolean {
            value: row.is_multiple_of(2),
        },
        3 => Cell::Formula {
            expression: format!("=B{}+1", row + 1),
            cached: Some("0".into()),
        },
        4 => Cell::Text {
            text: "=literal text, never a formula".into(),
        },
        5 => Cell::Number {
            value: format!("-{}", row + 1),
        },
        6 => Cell::Text {
            text: format!("record-{row}"),
        },
        7 => Cell::Number {
            value: format!("{row}.5"),
        },
        _ => unreachable!(),
    }
}
fn assert_page(snapshot: &DocumentSnapshot, sheet: &str, start: u32) -> Result<()> {
    let DocumentContent::Spreadsheet {
        sheets,
        selected_sheet,
        row_start,
        row_count,
        column_start,
        column_count,
        cells,
        total_rows,
        total_columns,
    } = &snapshot.content
    else {
        anyhow::bail!("Native document returned a non-spreadsheet result")
    };
    ensure!(sheets.len() == 2, "Fixture sheet metadata is incomplete");
    for (name, rows) in [("Small", SMALL_ROWS), ("Large", LARGE_ROWS)] {
        ensure!(
            sheets
                .iter()
                .any(|s| s.name == name && s.rows == rows && s.columns == 8),
            "Wrong metadata for sheet {name}"
        );
    }
    ensure!(
        selected_sheet == sheet
            && *row_start == start
            && *row_count == PAGE_ROWS
            && *column_start == 0
            && *column_count == PAGE_COLUMNS,
        "Native paging returned the wrong range"
    );
    ensure!(
        *total_rows
            == if sheet == "Small" {
                SMALL_ROWS
            } else {
                LARGE_ROWS
            }
            && *total_columns == 8
            && cells.len() == (PAGE_ROWS * PAGE_COLUMNS) as usize,
        "Native paging returned incomplete dimensions or cells"
    );
    for (index, cell) in cells.iter().enumerate() {
        let row = start + index as u32 / PAGE_COLUMNS;
        let column = index as u32 % PAGE_COLUMNS;
        ensure!(
            cell.row == row
                && cell.column == column
                && cell.editable
                && cell.value == expected(sheet, row, column),
            "Native cell assertion failed in {sheet} at row {row}, column {column}: {:?}",
            cell.value
        );
    }
    Ok(())
}
async fn page(
    config: &CapabilityConfig,
    path: &Path,
    source: Option<&DocumentSource>,
    sheet: &str,
    start: u32,
    cancel: &CancellationToken,
) -> Result<(DocumentSnapshot, Value)> {
    ensure!(!cancel.is_cancelled(), "Workload deadline reached");
    let args = json!({"path":path,"expected_revision":source.map(|s|&s.revision),
        "sheet":sheet,"row_start":start,"row_count":PAGE_ROWS,
        "column_start":0,"column_count":PAGE_COLUMNS});
    let timer = Instant::now();
    let result = documents::execute(config, "document_open", &args, cancel.child_token()).await?;
    let elapsed_ms = timer.elapsed().as_secs_f64() * 1000.;
    let snapshot: DocumentSnapshot = serde_json::from_str(&result.summary)?;
    assert_page(&snapshot, sheet, start)?;
    if let Some(source) = source {
        ensure!(
            &snapshot.source == source,
            "Source identity changed during measurement"
        );
    }
    Ok((
        snapshot,
        json!({"sheet":sheet,"row_start":start,"row_count":PAGE_ROWS,
        "column_start":0,"column_count":PAGE_COLUMNS,"native_ms":elapsed_ms,
        "response_bytes":result.summary.len(),"asserted_cells":PAGE_ROWS*PAGE_COLUMNS}),
    ))
}
fn statistics(samples: &[Value]) -> Value {
    let mut sorted: Vec<f64> = samples
        .iter()
        .map(|sample| sample["native_ms"].as_f64().unwrap())
        .collect();
    sorted.sort_by(f64::total_cmp);
    let rank = |percentage: f64| sorted[(sorted.len() as f64 * percentage).ceil() as usize - 1];
    json!({"unit":"ms","count":sorted.len(),"p50":rank(0.5),"p95":rank(0.95),
        "min":sorted[0],"max":sorted[sorted.len()-1],"raw":samples})
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let (output, fingerprint, samples) = arguments()?;
    let started = Instant::now();
    let cancel = CancellationToken::new();
    let deadline_cancel = cancel.clone();
    let deadline = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(55)).await;
        deadline_cancel.cancel();
    });
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?;
    let path = root.join("two-sheet-fixture.xlsx");
    let generation = Instant::now();
    fixture(&path, &cancel)?;
    let generation_ms = generation.elapsed().as_secs_f64() * 1000.;
    let config = CapabilityConfig {
        workspace: root.clone(),
        artifacts: root,
        operation_timeout_seconds: 15,
        ..Default::default()
    };
    let (first, first_sample) = page(&config, &path, None, "Small", 0, &cancel).await?;
    let (_, large_preparation) =
        page(&config, &path, Some(&first.source), "Large", 0, &cancel).await?;
    let mut small = Vec::new();
    let mut large = Vec::new();
    for index in 0..samples {
        let (sheet, start) = if index % 2 == 0 {
            (
                "Small",
                (index as u32 / 2 * 17) % (SMALL_ROWS - PAGE_ROWS + 1),
            )
        } else {
            (
                "Large",
                (index as u32 / 2 * 353 + 127) % (LARGE_ROWS - PAGE_ROWS + 1),
            )
        };
        let (_, observation) =
            page(&config, &path, Some(&first.source), sheet, start, &cancel).await?;
        if sheet == "Small" {
            small.push(observation);
        } else {
            large.push(observation);
        }
    }
    ensure!(!cancel.is_cancelled(), "Workload exceeded its deadline");
    deadline.abort();
    let mut system = sysinfo::System::new_all().expect("system monitor");
    system.refresh_memory();
    let load = sysinfo::System::load_average().expect("load average");
    let report = json!({
        "schema_version":1,"measured_at_ms":now_ms(),"source_fingerprint":fingerprint,
        "source_fingerprint_origin":"Explicit reviewed manifest identifier supplied by caller; not regenerated by harness",
        "harness_binary_sha256":file_hash(&std::env::current_exe()?)?,
        "compiled_harness_source_sha256":sha256(EMBEDDED_HARNESS),
        "compiled_workspace_cargo_lock_sha256":sha256(EMBEDDED_LOCK),
        "profile":if cfg!(debug_assertions){"debug"}else{"release"},
        "hardware":{"os":std::env::consts::OS,"os_version":sysinfo::System::long_os_version().ok(),
            "architecture":std::env::consts::ARCH,"cpu_threads":std::thread::available_parallelism()?.get(),
            "cpu_brand":system.cpus().first().map(|cpu|cpu.brand()),"total_memory_bytes":system.total_memory(),
            "load_average":{"one":load.one,"five":load.five,"fifteen":load.fifteen}},
        "fixture":{"format":"xlsx","small_rows":SMALL_ROWS,"large_rows":LARGE_ROWS,
            "filled_columns":FILLED_COLUMNS,"filled_cells":(SMALL_ROWS+LARGE_ROWS)*u32::from(FILLED_COLUMNS),
            "archive_bytes":std::fs::metadata(&path)?.len(),"archive_sha256":file_hash(&path)?,
            "generation_ms":generation_ms,"payload_budget_bytes":config.max_output_bytes},
        "preparation":{"first_small_open_metadata_miss":first_sample,"first_large_selected_page":large_preparation},
        "warm_queries":{"total":samples,"order":"Alternating Small/Large, varied row starts; no measured query changes source bytes",
            "small_sheet_with_large_sibling":statistics(&small),"large_selected_sheet":statistics(&large)},
        "total_workload_ms":started.elapsed().as_secs_f64()*1000.,
        "timing_region":"documents::execute including revision-safe file read/hash, native parse/admission and result JSON serialization; typed decoding/assertions are outside each timed sample",
        "limitations":["Direct native API excludes helper process startup/IPC, host admission, UI input/rendering and save-copy operations.",
            "First open is one process-cache miss observation, not cold filesystem latency or a launch percentile.",
            "Warm varied pages retain metadata/shared-string caches but still parse the selected worksheet and verify source bytes.",
            "No formula evaluation, live documents, cross-platform acceptance or Rust-versus-Python speedup is measured.",
            "Hardware/load are point-in-time observations; background developer activity is not controlled."]
    });
    if let Some(parent) = output.parent().filter(|path| !path.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&output)?;
    serde_json::to_writer_pretty(&mut file, &report)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    println!(
        "{}",
        json!({"output":output,"warm_samples":samples,
        "small_p95_ms":report["warm_queries"]["small_sheet_with_large_sibling"]["p95"],
        "large_p95_ms":report["warm_queries"]["large_selected_sheet"]["p95"]})
    );
    Ok(())
}
