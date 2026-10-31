use super::*;
use std::io::Write;
use zip::{ZipWriter, write::SimpleFileOptions};

fn xlsx(write: impl FnOnce(&mut rust_xlsxwriter::Worksheet)) -> tempfile::NamedTempFile {
    let file = tempfile::Builder::new().suffix(".xlsx").tempfile().unwrap();
    let mut book = rust_xlsxwriter::Workbook::new();
    write(book.add_worksheet());
    book.save(file.path()).unwrap();
    file
}

fn zip(parts: &[(&str, &[u8])]) -> tempfile::NamedTempFile {
    let file = tempfile::NamedTempFile::new().unwrap();
    let mut zip = ZipWriter::new(file.reopen().unwrap());
    for (name, bytes) in parts {
        zip.start_file(*name, SimpleFileOptions::default()).unwrap();
        zip.write_all(bytes).unwrap();
    }
    zip.finish().unwrap();
    file
}

fn read_file(file: &tempfile::NamedTempFile, extension: &str, limit: usize) -> Result<Value> {
    read(
        file.as_file(),
        extension,
        None,
        limit,
        &CancellationToken::new(),
    )
}

#[test]
fn opposite_corners_stream_without_allocating_the_bounding_box() {
    let file = xlsx(|sheet| {
        sheet.write_string(0, 0, "first").unwrap();
        sheet.write_number(1_048_575, 16_383, 7).unwrap();
    });
    let value = read_file(&file, "xlsx", 3).unwrap();
    assert_eq!(value["total_rows"], 1_048_576);
    assert_eq!(value["total_columns"], 16_384);
    assert_eq!(value["truncated"], true);
    let rows = value["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].as_array().unwrap().len(), MAX_COLUMNS as usize);
    assert_eq!(rows[0][0], "first");
    assert_eq!(rows[1][0], "");
}

#[test]
fn the_window_starts_at_the_first_used_cell() {
    let file = xlsx(|sheet| {
        sheet.write_string(2, 1, "corner").unwrap();
        sheet.write_number(4, 3, 2).unwrap();
    });
    let value = read_file(&file, "xlsx", 200).unwrap();
    assert_eq!(value["total_rows"], 3);
    assert_eq!(value["total_columns"], 3);
    assert_eq!(value["truncated"], false);
    assert_eq!(
        value["rows"],
        json!([["corner", "", ""], ["", "", ""], ["", "", "2"]])
    );
}

#[test]
fn declared_shared_string_counts_must_fit_their_part() {
    assert_eq!(
        max_unique_count(b"<sst count=\"3\" uniqueCount = '12'>"),
        Some(12)
    );
    assert_eq!(
        max_unique_count(b"<sst uniqueCount=\"99999999999999999999999\">"),
        Some(u64::MAX)
    );
    assert_eq!(max_unique_count(b"<sst uniqueCount=\"x\">"), None);
    let honest = xlsx(|sheet| {
        sheet.write_string(0, 0, "a").unwrap();
    });
    assert!(read_file(&honest, "xlsx", 10).is_ok());
    // The same workbook with an inflated count is refused before calamine
    // reserves the strings.
    let mut archive = zip::ZipArchive::new(honest.reopen().unwrap()).unwrap();
    let mut parts = Vec::new();
    for index in 0..archive.len() {
        let mut part = archive.by_index(index).unwrap();
        let mut bytes = Vec::new();
        part.read_to_end(&mut bytes).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        let text = if part.name().unwrap().ends_with("sharedStrings.xml") {
            text.replacen("uniqueCount=\"1\"", "uniqueCount=\"10000000000000\"", 1)
        } else {
            text
        };
        parts.push((part.name().unwrap().into_owned(), text));
    }
    let parts: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(name, text)| (name.as_str(), text.as_bytes()))
        .collect();
    let inflated = zip(&parts);
    let error = read_file(&inflated, "xlsx", 10).unwrap_err().to_string();
    assert!(error.contains("shared strings"), "{error}");
}

#[test]
fn legacy_xls_is_refused() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let error = read_file(&file, "xls", 10).unwrap_err().to_string();
    assert!(error.contains(".xlsx"), "{error}");
}

fn table(rows: &str) -> String {
    format!(
        "<office:document-content><office:body><office:spreadsheet><table:table table:name=\"S\">{rows}</table:table></office:spreadsheet></office:body></office:document-content>"
    )
}

fn cells(content: &str) -> Result<u64> {
    ods_cells(content.as_bytes(), &CancellationToken::new())
}

#[test]
fn ods_trailing_empty_repeats_cost_nothing() {
    // LibreOffice pads every sheet like this.
    let content = table(
        "<table:table-row><table:table-cell office:value-type=\"float\" office:value=\"1\"><text:p>1</text:p></table:table-cell><table:table-cell table:number-columns-repeated=\"1023\"/></table:table-row>\
         <table:table-row table:number-rows-repeated=\"1048575\"><table:table-cell table:number-columns-repeated=\"1024\"/></table:table-row>",
    );
    assert_eq!(cells(&content).unwrap(), 2);
}

#[test]
fn ods_repeated_filled_cells_beyond_the_budget_are_refused() {
    let row = "<table:table-row table:number-rows-repeated=\"1000\"><table:table-cell table:number-columns-repeated=\"16384\" office:value-type=\"float\" office:value=\"1\"/></table:table-row>";
    assert!(cells(&table(row)).is_err());
    // Pushed cells alone also count: many distinct wide rows.
    let wide = "<table:table-row><table:table-cell table:number-columns-repeated=\"16384\" office:value-type=\"float\" office:value=\"1\"/></table:table-row>";
    assert!(cells(&table(&wide.repeat(200))).is_err());
    // An empty run is materialized when a filled cell follows it.
    let gap = "<table:table-row><table:table-cell table:number-columns-repeated=\"9\"/><table:table-cell office:value-type=\"string\"><text:p>x</text:p></table:table-cell></table:table-row>";
    assert_eq!(cells(&table(gap)).unwrap(), 20);
}

#[test]
fn ods_bombs_are_refused_before_calamine_opens_them() {
    let row = "<table:table-row table:number-rows-repeated=\"1048576\"><table:table-cell table:number-columns-repeated=\"16384\" office:value-type=\"float\" office:value=\"1\"/></table:table-row>";
    let content = table(row);
    let file = zip(&[
        (
            "mimetype",
            b"application/vnd.oasis.opendocument.spreadsheet",
        ),
        ("content.xml", content.as_bytes()),
    ]);
    let error = read_file(&file, "ods", 10).unwrap_err().to_string();
    assert!(error.contains("cells"), "{error}");
}
