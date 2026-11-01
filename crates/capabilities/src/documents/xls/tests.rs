use super::*;
use crate::documents::workbook::{read, read_xls};
use serde_json::{Value, json};
use std::io::Write;
use std::time::{Duration, Instant};

const SECTOR: usize = 512;
const FAT_SECTOR: u32 = 0xFFFF_FFFD;

fn record(out: &mut Vec<u8>, kind: u16, data: &[u8]) {
    out.extend(kind.to_le_bytes());
    out.extend((data.len() as u16).to_le_bytes());
    out.extend(data);
}

fn bytes_of(kind: u16, data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    record(&mut out, kind, data);
    out
}

/// A cell record: row, column, a zero XF index, then `rest`.
fn cell(kind: u16, row: u16, column: u16, rest: &[u8]) -> Vec<u8> {
    let mut data = row.to_le_bytes().to_vec();
    data.extend(column.to_le_bytes());
    data.extend(0u16.to_le_bytes());
    data.extend(rest);
    bytes_of(kind, &data)
}

fn number(row: u16, column: u16, value: f64) -> Vec<u8> {
    cell(NUMBER, row, column, &value.to_le_bytes())
}

fn rk_cell(row: u16, column: u16, raw: u32) -> Vec<u8> {
    cell(RK, row, column, &raw.to_le_bytes())
}

fn mulrk(row: u16, first: u16, raws: &[u32]) -> Vec<u8> {
    let mut data = row.to_le_bytes().to_vec();
    data.extend(first.to_le_bytes());
    for raw in raws {
        data.extend(0u16.to_le_bytes());
        data.extend(raw.to_le_bytes());
    }
    data.extend((first + raws.len() as u16 - 1).to_le_bytes());
    bytes_of(MULRK, &data)
}

fn label_sst(row: u16, column: u16, index: u32) -> Vec<u8> {
    cell(LABELSST, row, column, &index.to_le_bytes())
}

/// An `XLUnicodeString`, compressed when every character fits a byte.
fn xl_string(text: &str) -> Vec<u8> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let wide = units.iter().any(|unit| *unit > 0xFF);
    let mut out = (units.len() as u16).to_le_bytes().to_vec();
    out.push(u8::from(wide));
    for unit in units {
        if wide {
            out.extend(unit.to_le_bytes());
        } else {
            out.push(unit as u8);
        }
    }
    out
}

fn label(row: u16, column: u16, text: &str) -> Vec<u8> {
    cell(LABEL, row, column, &xl_string(text))
}

fn boolerr(row: u16, column: u16, value: u8, is_error: bool) -> Vec<u8> {
    cell(BOOLERR, row, column, &[value, u8::from(is_error)])
}

fn formula(row: u16, column: u16, result: [u8; 8]) -> Vec<u8> {
    let mut rest = result.to_vec();
    rest.extend([0; 8]); // flags, chn, empty formula
    cell(FORMULA, row, column, &rest)
}

fn dimensions(rows: u32, columns: u16) -> Vec<u8> {
    let mut data = 0u32.to_le_bytes().to_vec();
    data.extend(rows.to_le_bytes());
    data.extend(0u16.to_le_bytes());
    data.extend(columns.to_le_bytes());
    data.extend(0u16.to_le_bytes());
    bytes_of(0x0200, &data)
}

/// An `SST` record declaring `unique` strings and holding `strings`.
fn sst(unique: u32, strings: &[&str]) -> Vec<u8> {
    let mut data = unique.to_le_bytes().to_vec();
    data.extend(unique.to_le_bytes());
    for text in strings {
        data.extend(xl_string(text));
    }
    bytes_of(SST, &data)
}

fn bof(kind: u16) -> Vec<u8> {
    let mut data = BIFF8.to_le_bytes().to_vec();
    data.extend(kind.to_le_bytes());
    data.extend([0; 12]);
    bytes_of(BOF, &data)
}

/// A BIFF8 `Workbook` stream: the globals with `globals` records, then
/// each sheet's records between its BOF and EOF.
fn biff(globals: &[u8], sheets: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = bof(GLOBALS);
    let mut patches = Vec::new();
    for (name, _) in sheets {
        patches.push(out.len() + 4);
        let mut data = vec![0, 0, 0, 0, 0, 0, name.len() as u8, 0];
        data.extend(name.as_bytes());
        record(&mut out, BOUNDSHEET, &data);
    }
    out.extend(globals);
    record(&mut out, EOF, &[]);
    for ((_, body), patch) in sheets.iter().zip(patches) {
        let offset = out.len() as u32;
        out[patch..patch + 4].copy_from_slice(&offset.to_le_bytes());
        out.extend(bof(WORKSHEET));
        out.extend(body);
        record(&mut out, EOF, &[]);
    }
    out
}

/// Appends `bytes` as a chain of whole sectors and returns the first.
fn chain(fat: &mut [u32], sectors: &mut Vec<Vec<u8>>, bytes: &[u8]) -> u32 {
    let first = sectors.len();
    let chunks: Vec<&[u8]> = bytes.chunks(SECTOR).collect();
    for (index, chunk) in chunks.iter().enumerate() {
        let id = first + index;
        fat[id] = if index + 1 == chunks.len() {
            END_OF_CHAIN
        } else {
            id as u32 + 1
        };
        let mut sector = chunk.to_vec();
        sector.resize(SECTOR, 0);
        sectors.push(sector);
    }
    first as u32
}

fn entry(name: &str, kind: u8, start: u32, size: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; DIRECTORY_ENTRY];
    let units: Vec<u16> = name.encode_utf16().chain([0]).collect();
    for (index, unit) in units.iter().enumerate() {
        bytes[index * 2..index * 2 + 2].copy_from_slice(&unit.to_le_bytes());
    }
    bytes[0x40..0x42].copy_from_slice(&((units.len() * 2) as u16).to_le_bytes());
    bytes[0x42] = kind;
    bytes[0x43] = 1;
    for at in [0x44, 0x48, 0x4C] {
        bytes[at..at + 4].copy_from_slice(&FREE_SECTOR.to_le_bytes());
    }
    if kind == 5 {
        bytes[0x4C..0x50].copy_from_slice(&1u32.to_le_bytes());
    }
    bytes[0x74..0x78].copy_from_slice(&start.to_le_bytes());
    bytes[0x78..0x7C].copy_from_slice(&size.to_le_bytes());
    bytes
}

fn put(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// A version 3 Compound File holding `stream` as `name`: sector 0 is the
/// FAT and sector 1 the directory. Streams under 4096 bytes go to the mini
/// stream, with the mini FAT in sector 2.
fn compound_named(name: &str, stream: &[u8]) -> Vec<u8> {
    let mut fat = vec![FREE_SECTOR; SECTOR / 4];
    fat[0] = FAT_SECTOR;
    fat[1] = END_OF_CHAIN;
    let mut sectors = vec![Vec::new(), Vec::new()];
    let (root_start, root_size, start, first_mini_fat, mini_fat_sectors);
    if stream.len() < MINI_CUTOFF as usize {
        let count = stream.len().div_ceil(MINI_SECTOR);
        let mut mini_fat = vec![FREE_SECTOR; SECTOR / 4];
        for (index, next) in mini_fat.iter_mut().take(count).enumerate() {
            *next = if index + 1 == count {
                END_OF_CHAIN
            } else {
                index as u32 + 1
            };
        }
        first_mini_fat = chain(
            &mut fat,
            &mut sectors,
            &mini_fat
                .iter()
                .flat_map(|e| e.to_le_bytes())
                .collect::<Vec<_>>(),
        );
        mini_fat_sectors = 1;
        let mut container = stream.to_vec();
        container.resize(count * MINI_SECTOR, 0);
        root_start = chain(&mut fat, &mut sectors, &container);
        root_size = container.len() as u32;
        start = 0;
    } else {
        start = chain(&mut fat, &mut sectors, stream);
        (root_start, root_size, first_mini_fat, mini_fat_sectors) =
            (END_OF_CHAIN, 0, END_OF_CHAIN, 0);
    }
    sectors[0] = fat.iter().flat_map(|e| e.to_le_bytes()).collect();
    let mut directory = entry("Root Entry", 5, root_start, root_size);
    directory.extend(entry(name, 2, start, stream.len() as u32));
    directory.resize(SECTOR, 0);
    sectors[1] = directory;
    let mut header = vec![0u8; SECTOR];
    header[..8].copy_from_slice(&SIGNATURE);
    header[0x18..0x1A].copy_from_slice(&0x3Eu16.to_le_bytes());
    header[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes());
    header[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
    header[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
    header[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
    put(&mut header, 0x2C, 1);
    put(&mut header, 0x30, 1);
    put(&mut header, 0x38, 4096);
    put(&mut header, 0x3C, first_mini_fat);
    put(&mut header, 0x40, mini_fat_sectors);
    put(&mut header, 0x44, END_OF_CHAIN);
    put(&mut header, 0x4C, 0);
    for slot in 1..HEADER_DIFAT {
        put(&mut header, 0x4C + slot * 4, FREE_SECTOR);
    }
    header.extend(sectors.concat());
    header
}

fn compound(stream: &[u8]) -> Vec<u8> {
    compound_named("Workbook", stream)
}

/// Raw RK values: an integer, an integer over 100, and the high half of a
/// double.
fn rk_int(value: i32) -> u32 {
    ((value << 2) as u32) | 2
}

fn sample_stream() -> Vec<u8> {
    let data = [
        dimensions(5, 4),
        label_sst(0, 0, 0),
        label_sst(0, 1, 1),
        label(0, 2, "note"),
        label_sst(1, 0, 2),
        number(1, 1, 1.5),
        label(1, 2, "ünïcødé ✓"),
        label_sst(2, 0, 3),
        rk_cell(2, 1, rk_int(42)),
        boolerr(2, 2, 1, false),
        mulrk(3, 0, &[rk_int(7), rk_int(123) | 1, 0x4004_0000]),
        formula(4, 0, 3.25f64.to_le_bytes()),
        formula(4, 1, [0, 0, 0, 0, 0, 0, 0xFF, 0xFF]),
        bytes_of(STRING, &xl_string("joined")),
        formula(4, 2, [1, 0, 1, 0, 0, 0, 0xFF, 0xFF]),
        boolerr(4, 3, 0x07, true),
    ]
    .concat();
    let second = number(2, 1, 9.0);
    biff(
        &sst(4, &["name", "value", "alpha", "beta"]),
        &[("Data", data), ("Second", second)],
    )
}

/// A small workbook, held in the mini stream.
fn sample() -> Vec<u8> {
    compound(&sample_stream())
}

/// A workbook of `rows` numbered rows, large enough for regular sectors.
fn tall(rows: u16) -> Vec<u8> {
    let body: Vec<u8> = (0..rows)
        .flat_map(|row| number(row, 0, f64::from(row)))
        .collect();
    let stream = biff(&[], &[("Tall", body)]);
    assert!(stream.len() >= MINI_CUTOFF as usize);
    compound(&stream)
}

fn read_bytes(bytes: &[u8], sheet: Option<&str>, limit: usize) -> anyhow::Result<Value> {
    read_xls(bytes, sheet, limit, &CancellationToken::new())
}

fn error_of(bytes: &[u8]) -> String {
    read_bytes(bytes, None, 50).unwrap_err().to_string()
}

#[test]
fn reads_text_number_bool_error_and_formula_cells() {
    let value = read_bytes(&sample(), None, 200).unwrap();
    assert_eq!(
        value,
        json!({
            "sheet": "Data",
            "sheets": ["Data", "Second"],
            "rows": [
                ["name", "value", "note", ""],
                ["alpha", "1.5", "ünïcødé ✓", ""],
                ["beta", "42", "true", ""],
                ["7", "1.23", "2.5", ""],
                ["3.25", "joined", "true", "#DIV/0!"],
            ],
            "total_rows": 5,
            "total_columns": 4,
            "truncated": false,
        })
    );
    let second = read_bytes(&sample(), Some("Second"), 200).unwrap();
    assert_eq!(second["rows"], json!([["9"]]));
    assert!(read_bytes(&sample(), Some("Missing"), 200).is_err());
}

#[test]
fn output_matches_the_xlsx_path() {
    let data = [
        label_sst(0, 0, 0),
        label(0, 2, "note"),
        number(1, 1, 1.5),
        rk_cell(2, 1, rk_int(42)),
        boolerr(2, 2, 0, false),
        number(3, 0, -0.25),
    ]
    .concat();
    let xls = compound(&biff(&sst(1, &["name"]), &[("Data", data)]));
    let from_xls = read_bytes(&xls, None, 3).unwrap();

    let file = tempfile::Builder::new().suffix(".xlsx").tempfile().unwrap();
    let mut book = rust_xlsxwriter::Workbook::new();
    let sheet = book.add_worksheet().set_name("Data").unwrap();
    sheet.write_string(0, 0, "name").unwrap();
    sheet.write_string(0, 2, "note").unwrap();
    sheet.write_number(1, 1, 1.5).unwrap();
    sheet.write_number(2, 1, 42).unwrap();
    sheet.write_boolean(2, 2, false).unwrap();
    sheet.write_number(3, 0, -0.25).unwrap();
    book.save(file.path()).unwrap();
    let from_xlsx = read(file.as_file(), "xlsx", None, 3, &CancellationToken::new()).unwrap();
    assert_eq!(from_xls, from_xlsx);
    assert_eq!(from_xls["truncated"], true);
}

#[test]
fn reads_xls_files_through_document_read() {
    let mut file = tempfile::Builder::new().suffix(".xls").tempfile().unwrap();
    file.write_all(&sample()).unwrap();
    let value = read(
        file.as_file(),
        "xls",
        Some("Second"),
        10,
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(value["rows"], json!([["9"]]));
    let empty = tempfile::NamedTempFile::new().unwrap();
    assert!(read(empty.as_file(), "xls", None, 10, &CancellationToken::new()).is_err());
}

#[test]
fn regular_sector_streams_respect_max_rows() {
    let value = read_bytes(&tall(300), None, 10).unwrap();
    assert_eq!(value["total_rows"], 300);
    assert_eq!(value["total_columns"], 1);
    assert_eq!(value["truncated"], true);
    let rows = value["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 10);
    assert_eq!(rows[9], json!(["9"]));
}

#[test]
fn self_referencing_fat_chains_are_refused() {
    // The Workbook stream's first sector points at itself.
    let mut file = tall(300);
    put(&mut file, SECTOR + 2 * 4, 2);
    assert!(error_of(&file).contains("loops"));
    // So does the directory's.
    let mut file = sample();
    put(&mut file, SECTOR + 4, 1);
    assert!(error_of(&file).contains("loops"));
    // A chain into a FAT entry past the end of the file.
    let mut file = tall(300);
    put(&mut file, SECTOR + 2 * 4, 120);
    assert!(error_of(&file).contains("leaves the file"));
}

#[test]
fn difat_loops_are_refused() {
    let mut file = sample();
    let looping = (file.len() / SECTOR - 1) as u32;
    let mut sector = vec![0xFFu8; SECTOR];
    put(&mut sector, SECTOR - 4, looping);
    file.extend(sector);
    put(&mut file, 0x44, looping);
    put(&mut file, 0x48, 2);
    assert!(error_of(&file).contains("DIFAT"));
    put(&mut file, 0x48, u32::MAX);
    assert!(error_of(&file).contains("more sectors"));
}

#[test]
fn declared_dimensions_are_ignored() {
    let body = [dimensions(u32::MAX, u16::MAX), number(0, 0, 1.0)].concat();
    let file = compound(&biff(&[], &[("Big", body)]));
    let started = Instant::now();
    let value = read_bytes(&file, None, 2000).unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(value["total_rows"], 1);
    assert_eq!(value["total_columns"], 1);
    assert_eq!(value["rows"], json!([["1"]]));
}

#[test]
fn shared_string_counts_must_fit_the_stream() {
    let body = label_sst(0, 0, 0);
    let huge = compound(&biff(&sst(u32::MAX, &["a"]), &[("S", body.clone())]));
    assert!(error_of(&huge).contains("shared strings"));
    // A count within the bound but beyond the strings present.
    let short = compound(&biff(&sst(5, &["a"]), &[("S", body.clone())]));
    assert!(error_of(&short).contains("malformed"));
    // A cell naming a string the table lacks.
    let missing = compound(&biff(&sst(1, &["a"]), &[("S", label_sst(0, 0, 1))]));
    assert!(error_of(&missing).contains("missing shared string"));
}

#[test]
fn shared_strings_continue_across_records() {
    let mut first = 3u32.to_le_bytes().to_vec();
    first.extend(3u32.to_le_bytes());
    // "abcdéf": compressed "abc", then the rest wide in a CONTINUE.
    first.extend([6, 0, 0]);
    first.extend(b"abc");
    let mut second = vec![1];
    second.extend("déf".encode_utf16().flat_map(u16::to_le_bytes));
    second.extend([2, 0, 0]);
    second.extend(b"ok");
    // A rich string with phonetic data split across a third record.
    second.extend([1, 0, 0x0C, 1, 0, 3, 0, 0, 0]);
    second.push(b'z');
    second.extend([0; 4]);
    second.extend([0; 2]);
    let third = vec![0];
    let globals = [
        bytes_of(SST, &first),
        bytes_of(CONTINUE, &second),
        bytes_of(CONTINUE, &third),
    ]
    .concat();
    let body = [label_sst(0, 0, 0), label_sst(0, 1, 1), label_sst(0, 2, 2)].concat();
    let file = compound(&biff(&globals, &[("S", body)]));
    let value = read_bytes(&file, None, 10).unwrap();
    assert_eq!(value["rows"], json!([["abcdéf", "ok", "z"]]));
}

#[test]
fn truncated_streams_are_refused() {
    // The file ends halfway through the Workbook stream's sectors.
    let file = tall(300);
    assert!(read_bytes(&file[..file.len() / 2], None, 50).is_err());
    // The directory declares more bytes than the mini chain holds.
    let mut file = sample();
    let size_at = 2 * SECTOR + DIRECTORY_ENTRY + 0x78;
    let size = u32::from_le_bytes(file[size_at..size_at + 4].try_into().unwrap());
    put(&mut file, size_at, size + 200);
    assert!(error_of(&file).contains("truncated"));
    // A sheet without its EOF, and a record cut short.
    let stream = sample_stream();
    for cut in [4, 7] {
        let file = compound(&stream[..stream.len() - cut]);
        let error = read_bytes(&file, Some("Second"), 50)
            .unwrap_err()
            .to_string();
        assert!(error.contains("malformed"), "{error}");
    }
}

#[test]
fn unsupported_workbooks_are_refused() {
    assert!(error_of(&compound_named("Book", &sample_stream())).contains(".xlsx"));
    let encrypted = compound(&biff(&bytes_of(FILEPASS, &[0; 6]), &[]));
    assert!(error_of(&encrypted).contains("Encrypted"));
    assert!(error_of(b"not a compound file").contains(".xls"));
}

/// Mutating bytes of valid workbooks must always end promptly in a value
/// or an error.
#[test]
fn random_mutations_end_promptly() {
    let bases = [sample(), tall(300)];
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let started = Instant::now();
    let mut outcomes = [0usize; 2];
    for iteration in 0..4000 {
        let mut bytes = bases[iteration % 2].clone();
        for _ in 0..1 + next() % 8 {
            // Half the edits land in the header, FAT and directory.
            let span = if next() % 2 == 0 {
                bytes.len().min(4 * SECTOR)
            } else {
                bytes.len()
            };
            let at = (next() % span as u64) as usize;
            bytes[at] = next() as u8;
        }
        if next() % 16 == 0 {
            let keep = (next() % bytes.len() as u64) as usize;
            bytes.truncate(keep);
        }
        let result = read_bytes(&bytes, None, 50);
        outcomes[usize::from(result.is_ok())] += 1;
    }
    assert!(outcomes[0] > 0 && outcomes[1] > 0, "{outcomes:?}");
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn sheet_name_floods_are_listed_up_to_the_cap() {
    // 2,000 extra BOUNDSHEET records after the real sheet; the last one
    // points at the real sheet's BOF.
    const MARKER: u32 = 0xDEAD_BEEF;
    let mut globals = Vec::new();
    for index in 0..2000u32 {
        let offset = if index == 1999 { MARKER } else { 0 };
        let name = format!("X{index:04}");
        let mut data = offset.to_le_bytes().to_vec();
        data.extend([0, 0, name.len() as u8, 0]);
        data.extend(name.as_bytes());
        globals.extend(bytes_of(BOUNDSHEET, &data));
    }
    let mut stream = biff(&globals, &[("Real", number(0, 0, 5.0))]);
    let real = stream[24..28].to_vec();
    let at = stream
        .windows(4)
        .position(|window| window == MARKER.to_le_bytes())
        .unwrap();
    stream[at..at + 4].copy_from_slice(&real);
    let file = compound(&stream);

    let value = read_bytes(&file, None, 10).unwrap();
    let sheets = value["sheets"].as_array().unwrap();
    assert_eq!(sheets.len(), crate::documents::workbook::MAX_SHEETS);
    assert_eq!(sheets[0], "Real");
    assert_eq!(value["sheets_truncated"], true);
    assert_eq!(value["total_sheets"], 2001);
    assert_eq!(value["rows"], json!([["5"]]));
    // A sheet past the listed names can still be chosen by name.
    let last = read_bytes(&file, Some("X1999"), 10).unwrap();
    assert_eq!(last["sheet"], "X1999");
    assert_eq!(last["rows"], json!([["5"]]));
    // One pointing elsewhere than a worksheet is refused.
    let error = read_bytes(&file, Some("X0000"), 10)
        .unwrap_err()
        .to_string();
    assert!(error.contains("not a BIFF8 worksheet"), "{error}");
    // An ordinary workbook carries no truncation keys.
    let sample = read_bytes(&sample(), None, 10).unwrap();
    assert!(sample.get("sheets_truncated").is_none());
    assert!(sample.get("total_sheets").is_none());
}
