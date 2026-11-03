//! Bounded spreadsheet extraction.
//!
//! calamine sizes several allocations from numbers inside the file, so a
//! small workbook can ask for gigabytes:
//!
//! - `worksheet_range` allocates the bounding box of the used cells, so an
//!   XLSX with only `A1` and `XFD1048576` asks for about 1.7e10 cells.
//!   XLSX and XLSB are therefore streamed with `worksheet_cells_reader` and
//!   only the requested window is kept.
//! - XLSX reserves shared strings from the `uniqueCount` attribute, so that
//!   attribute is checked against the size of the part declaring it.
//! - ODS materializes every table when the workbook opens, expanding
//!   repeated cells, so `content.xml` is scanned first and the workbook is
//!   refused when the cells calamine would build exceed [`CELL_BUDGET`].
//! - Legacy `.xls` is not given to calamine at all: its compound-file
//!   reader follows sector chains without cycle checks, reserves from
//!   declared dimensions and builds every sheet's bounding box on open.
//!   [`super::xls`] reads it with bounded chains and streams one sheet's
//!   cells through the same window as XLSX.

use super::{MAX_ARCHIVE, MAX_INPUT, check, preflight_archive, xls};
use anyhow::{Context, Result, bail, ensure};
use calamine::{Data, DataRef, Ods, Reader, Xlsb, Xlsx};
use quick_xml::events::Event;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
};
use tokio_util::sync::CancellationToken;

/// Most sheet names listed in a result.
pub(super) const MAX_SHEETS: usize = 1024;
/// Most columns returned per row.
pub(super) const MAX_COLUMNS: u32 = 128;
/// Most cells calamine may materialize when opening an ODS workbook. Each
/// costs a value and a formula string, so this keeps the open near 100 MiB.
pub(super) const CELL_BUDGET: u64 = 2_000_000;
/// Smallest serialized shared string (`<si><t/></si>`), bounding how many
/// strings a part of a given size can declare.
const MIN_SHARED_STRING_BYTES: u64 = 12;
/// calamine's own row and column caps for ODS tables.
const ODS_MAX_ROWS: u64 = 1_048_576;
const ODS_MAX_COLUMNS: u64 = 16_384;

/// The first `limit` rows and [`MAX_COLUMNS`] columns of `sheet` (or the
/// first sheet), with the sheet's used size.
pub(super) fn read(
    file: &File,
    extension: &str,
    sheet: Option<&str>,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Value> {
    match extension {
        "xls" => {
            let mut bytes = Vec::new();
            reader(file)?.take(MAX_INPUT + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= MAX_INPUT,
                "Document exceeds 64MiB input budget"
            );
            read_xls(&bytes, sheet, limit, cancel)
        }
        "xlsx" | "xlsm" => {
            preflight_archive(file)?;
            check_shared_strings(file, cancel)?;
            let mut workbook = Xlsx::new(openxml_reader(file)?)?;
            let (sheet, sheets) = choose(workbook.sheet_names().iter().map(String::as_str), sheet)?;
            let window = stream(&mut workbook, &sheet, limit, cancel)?;
            Ok(window.json(&sheet, &sheets))
        }
        "xlsb" => {
            preflight_archive(file)?;
            let mut workbook = Xlsb::new(openxml_reader(file)?)?;
            let (sheet, sheets) = choose(workbook.sheet_names().iter().map(String::as_str), sheet)?;
            let window = stream(&mut workbook, &sheet, limit, cancel)?;
            Ok(window.json(&sheet, &sheets))
        }
        "ods" => {
            preflight_archive(file)?;
            check_ods_cells(file, cancel)?;
            let mut workbook = Ods::new(reader(file)?)?;
            let (sheet, sheets) = choose(workbook.sheet_names().iter().map(String::as_str), sheet)?;
            let range = workbook.worksheet_range(&sheet)?;
            let rows: Vec<Vec<String>> = range
                .rows()
                .take(limit)
                .map(|row| {
                    row.iter()
                        .take(MAX_COLUMNS as usize)
                        .map(ToString::to_string)
                        .collect()
                })
                .collect();
            let mut value = json!({"sheet":sheet,"rows":rows,
                "total_rows":range.height(),"total_columns":range.width(),
                "truncated":range.height() > limit || range.width() > MAX_COLUMNS as usize});
            sheets.describe(&mut value);
            Ok(value)
        }
        other => bail!("Unsupported spreadsheet type: {other}"),
    }
}

/// The first `limit` rows and [`MAX_COLUMNS`] columns of `sheet` (or the
/// first sheet) of a legacy `.xls` workbook held in `bytes`.
pub(super) fn read_xls(
    bytes: &[u8],
    sheet: Option<&str>,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Value> {
    let mut workbook = xls::Workbook::parse(bytes, cancel)?;
    let (sheet, sheets) = choose(workbook.sheet_names(), sheet)?;
    let window = stream(&mut workbook, &sheet, limit, cancel)?;
    Ok(window.json(&sheet, &sheets))
}

/// A buffered reader over the already opened input, from its start.
fn reader(file: &File) -> Result<BufReader<File>> {
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    Ok(BufReader::new(file))
}

/// A ZIP polyglot can also have a Compound File header. Reject that wrapper
/// before calamine probes it for encryption using its separate CFB parser.
fn openxml_reader(file: &File) -> Result<BufReader<File>> {
    let mut reader = reader(file)?;
    let mut header = [0; 8];
    reader.read_exact(&mut header)?;
    ensure!(
        header != [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1],
        "Encrypted or malformed Office spreadsheets are not supported; save an unencrypted workbook"
    );
    reader.rewind()?;
    Ok(reader)
}

/// The sheet names a result lists; the rest are only counted.
#[derive(Debug, PartialEq)]
pub(super) struct Sheets {
    /// The first [`MAX_SHEETS`] names.
    listed: Vec<String>,
    /// Every sheet in the workbook.
    total: usize,
}

impl Sheets {
    /// Sets `sheets` on a result, with `sheets_truncated` and
    /// `total_sheets` when names were left out.
    fn describe(&self, value: &mut Value) {
        value["sheets"] = json!(self.listed);
        if self.total > self.listed.len() {
            value["sheets_truncated"] = json!(true);
            value["total_sheets"] = json!(self.total);
        }
    }
}

/// The requested sheet (or the first) and the listed sheet names. A sheet
/// beyond the listed ones can still be requested by name.
pub(super) fn choose<'n>(
    names: impl Iterator<Item = &'n str>,
    requested: Option<&str>,
) -> Result<(String, Sheets)> {
    let mut listed = Vec::new();
    let mut total = 0usize;
    let mut found = false;
    for name in names {
        total += 1;
        found |= requested == Some(name);
        if listed.len() < MAX_SHEETS {
            listed.push(name.to_owned());
        }
    }
    let sheet = match requested {
        Some(requested) => {
            ensure!(found, "Workbook has no sheet named {requested}");
            requested.to_owned()
        }
        None => listed.first().cloned().context("Workbook has no sheets")?,
    };
    Ok((sheet, Sheets { listed, total }))
}

/// Receives each cell's position and value.
pub(super) type Visit<'v> = dyn FnMut((u32, u32), &DataRef<'_>) -> Result<()> + 'v;

/// A workbook whose sheets can be read one cell at a time.
trait Cells {
    /// Calls `visit` with each cell of `sheet` in file order.
    fn each(&mut self, sheet: &str, visit: &mut Visit<'_>) -> Result<()>;
}

impl<RS: Read + Seek> Cells for Xlsx<RS> {
    fn each(&mut self, sheet: &str, visit: &mut Visit<'_>) -> Result<()> {
        let mut cells = self.worksheet_cells_reader(sheet)?;
        while let Some(cell) = cells.next_cell()? {
            visit(cell.get_position(), cell.get_value())?;
        }
        Ok(())
    }
}

impl<RS: Read + Seek> Cells for Xlsb<RS> {
    fn each(&mut self, sheet: &str, visit: &mut Visit<'_>) -> Result<()> {
        let mut cells = self.worksheet_cells_reader(sheet)?;
        while let Some(cell) = cells.next_cell()? {
            visit(cell.get_position(), cell.get_value())?;
        }
        Ok(())
    }
}

impl Cells for xls::Workbook {
    fn each(&mut self, sheet: &str, visit: &mut Visit<'_>) -> Result<()> {
        xls::Workbook::each(self, sheet, visit)
    }
}

/// The used area of a sheet and the cells of its first rows and columns.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Window {
    /// First used row and column.
    start: Option<(u32, u32)>,
    /// Last used row and column.
    end: (u32, u32),
    /// Kept cells by position relative to `start`.
    cells: BTreeMap<(u32, u32), String>,
    limit: usize,
}

impl Window {
    fn json(&self, sheet: &str, sheets: &Sheets) -> Value {
        let (rows, columns) = self.size();
        let shown_rows = rows.min(self.limit as u64) as u32;
        let shown_columns = columns.min(u64::from(MAX_COLUMNS)) as u32;
        let rows_out: Vec<Vec<String>> = (0..shown_rows)
            .map(|row| {
                (0..shown_columns)
                    .map(|column| self.cells.get(&(row, column)).cloned().unwrap_or_default())
                    .collect()
            })
            .collect();
        let mut value = json!({"sheet":sheet,"rows":rows_out,
            "total_rows":rows,"total_columns":columns,
            "truncated":rows > self.limit as u64 || columns > u64::from(MAX_COLUMNS)});
        sheets.describe(&mut value);
        value
    }

    /// Widens the used area to include a filled cell.
    fn include(&mut self, row: u32, column: u32) {
        let (first_row, first_column) = self.start.unwrap_or((row, column));
        self.start = Some((first_row.min(row), first_column.min(column)));
        self.end = (self.end.0.max(row), self.end.1.max(column));
    }

    /// Used rows and columns, like calamine's range height and width.
    fn size(&self) -> (u64, u64) {
        match self.start {
            Some((row, column)) => (
                u64::from(self.end.0 - row) + 1,
                u64::from(self.end.1 - column) + 1,
            ),
            None => (0, 0),
        }
    }
}

/// Streams a sheet twice: once for its used area, then keeping only the
/// first `limit` rows and [`MAX_COLUMNS`] columns of that area. Memory stays
/// bounded by the window whatever positions the file declares.
fn stream(
    workbook: &mut impl Cells,
    sheet: &str,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Window> {
    let mut window = Window {
        limit,
        ..Window::default()
    };
    let mut seen = 0u64;
    workbook.each(sheet, &mut |(row, column), value| {
        seen += 1;
        if seen.is_multiple_of(4096) {
            check(cancel)?;
        }
        if !matches!(value, DataRef::Empty) {
            window.include(row, column);
        }
        Ok(())
    })?;
    let Some(start) = window.start else {
        return Ok(window);
    };
    workbook.each(sheet, &mut |(row, column), value| {
        if let Some(position) = kept(start, (row, column), limit)
            && !matches!(value, DataRef::Empty)
        {
            window
                .cells
                .insert(position, Data::from(value.clone()).to_string());
        }
        Ok(())
    })?;
    Ok(window)
}

/// The position of `cell` within the window starting at `start`, when the
/// window keeps it.
fn kept(start: (u32, u32), cell: (u32, u32), limit: usize) -> Option<(u32, u32)> {
    let row = cell.0.checked_sub(start.0)?;
    let column = cell.1.checked_sub(start.1)?;
    ((row as usize) < limit && column < MAX_COLUMNS).then_some((row, column))
}

/// Refuses an XLSX whose `uniqueCount` would make calamine reserve more
/// shared strings than the declaring part could hold. calamine parses the
/// raw attribute bytes, so the scan does too, in every part.
fn check_shared_strings(file: &File, cancel: &CancellationToken) -> Result<()> {
    let mut archive = zip::ZipArchive::new(reader(file)?)?;
    for index in 0..archive.len() {
        check(cancel)?;
        let mut part = archive.by_index(index)?;
        let size = part.size();
        let mut bytes = Vec::new();
        (&mut part).take(MAX_ARCHIVE).read_to_end(&mut bytes)?;
        if let Some(declared) = max_unique_count(&bytes) {
            ensure!(
                declared <= size / MIN_SHARED_STRING_BYTES + 1,
                "Workbook declares more shared strings than it contains"
            );
        }
    }
    Ok(())
}

/// The largest `uniqueCount="N"` value in `bytes`, if any.
pub(super) fn max_unique_count(bytes: &[u8]) -> Option<u64> {
    const NAME: &[u8] = b"uniqueCount";
    let mut largest = None;
    let mut rest = bytes;
    while let Some(index) = rest.windows(NAME.len()).position(|window| window == NAME) {
        rest = &rest[index + NAME.len()..];
        let after = rest.trim_ascii_start();
        let Some(after) = after.strip_prefix(b"=") else {
            continue;
        };
        let after = after.trim_ascii_start();
        let Some((&quote, value)) = after.split_first() else {
            continue;
        };
        if quote != b'"' && quote != b'\'' {
            continue;
        }
        let value = value.strip_prefix(b"+").unwrap_or(value);
        let digits = value
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits == 0 {
            continue;
        }
        // Too many digits for a u64 is as good as unbounded.
        let count = std::str::from_utf8(&value[..digits])
            .ok()
            .and_then(|digits| digits.parse::<u64>().ok())
            .unwrap_or(u64::MAX);
        largest = largest.max(Some(count));
    }
    largest
}

/// Refuses an ODS whose tables would make calamine materialize more than
/// [`CELL_BUDGET`] cells when it opens the workbook.
fn check_ods_cells(file: &File, cancel: &CancellationToken) -> Result<()> {
    let mut archive = zip::ZipArchive::new(reader(file)?)?;
    let mut content = Vec::new();
    archive
        .by_name("content.xml")?
        .take(MAX_ARCHIVE)
        .read_to_end(&mut content)?;
    let cells = ods_cells(&content, cancel)?;
    ensure!(
        cells <= CELL_BUDGET,
        "Spreadsheet expands to more than {CELL_BUDGET} cells; export the needed range to a smaller file"
    );
    Ok(())
}

/// An upper bound on the cells calamine builds for every table in an ODS
/// `content.xml`: the cells it pushes while reading each row element, plus
/// the trimmed range it then expands with row repeats. A cell counts as
/// filled when it has a value type, a formula or any content, which can
/// only overestimate.
pub(super) fn ods_cells(content: &[u8], cancel: &CancellationToken) -> Result<u64> {
    let mut reader = quick_xml::Reader::from_reader(content);
    let mut buffer = Vec::new();
    let mut skipped = Vec::new();
    let mut total = 0u64;
    let mut table = OdsTable::default();
    let mut events = 0u64;
    loop {
        events += 1;
        if events.is_multiple_of(4096) {
            check(cancel)?;
        }
        buffer.clear();
        match reader.read_event_into(&mut buffer)? {
            Event::Start(element) if element.name().as_ref() == "table:table" => {
                total = total.saturating_add(table.finish());
            }
            Event::End(element) if element.name().as_ref() == "table:table" => {
                total = total.saturating_add(table.finish());
            }
            Event::Start(element) if element.name().as_ref() == "table:table-row" => {
                let repeats = repeat(&element, "table:number-rows-repeated")?;
                let pushed = ods_row(&mut reader, &mut skipped)?;
                total = total.saturating_add(pushed);
                table.row(repeats, pushed);
            }
            Event::Eof => break,
            _ => {}
        }
        ensure!(
            total <= CELL_BUDGET,
            "Spreadsheet expands to more than {CELL_BUDGET} cells; export the needed range to a smaller file"
        );
    }
    Ok(total.saturating_add(table.finish()))
}

/// Row extents of one ODS table, for the size of calamine's trimmed range.
#[derive(Default)]
struct OdsTable {
    /// Rows so far, after calamine's cap.
    rows: u64,
    /// Repeated rows from the first filled row through the last one seen.
    span: u64,
    /// Repeated rows since the last filled row.
    pending: u64,
    /// Widest filled row, in pushed cells.
    width: u64,
    filled: bool,
}

impl OdsTable {
    fn row(&mut self, repeats: u64, pushed: u64) {
        let repeats = repeats.min(ODS_MAX_ROWS.saturating_sub(self.rows));
        self.rows += repeats;
        if pushed == 0 {
            if self.filled {
                self.pending = self.pending.saturating_add(repeats);
            }
            return;
        }
        self.filled = true;
        self.span = self
            .span
            .saturating_add(self.pending)
            .saturating_add(repeats);
        self.pending = 0;
        self.width = self.width.max(pushed);
    }

    /// The cells of the trimmed range; resets for the next table.
    fn finish(&mut self) -> u64 {
        let cells = self.span.saturating_mul(self.width);
        *self = Self::default();
        cells
    }
}

/// The cells calamine pushes for one row element: every filled cell's
/// repeats, and an empty run only when another cell follows it, each capped
/// at its column limit.
fn ods_row(reader: &mut quick_xml::Reader<&[u8]>, skipped: &mut Vec<u8>) -> Result<u64> {
    let mut buffer = Vec::new();
    let mut columns = 0u64;
    let mut empty_run = 0u64;
    loop {
        buffer.clear();
        let (element, filled, filled_by_content) = match reader.read_event_into(&mut buffer)? {
            Event::Start(element) => (element.into_owned(), true, true),
            Event::Empty(element) => {
                let filled = element.attributes().flatten().any(|attribute| {
                    matches!(
                        attribute.key.as_ref(),
                        "office:value-type" | "calcext:value-type" | "table:formula"
                    )
                });
                (element.into_owned(), filled, false)
            }
            Event::End(element) if element.name().as_ref() == "table:table-row" => {
                return Ok(columns);
            }
            Event::Eof => bail!("Spreadsheet row is not closed"),
            _ => continue,
        };
        if !matches!(
            element.name().as_ref(),
            "table:table-cell" | "table:covered-table-cell"
        ) {
            bail!("Spreadsheet row contains an unexpected element");
        }
        if filled_by_content {
            skipped.clear();
            reader.read_to_end_into(element.name(), skipped)?;
        }
        let repeats = repeat(&element, "table:number-columns-repeated")?;
        columns += empty_run.min(ODS_MAX_COLUMNS - columns);
        empty_run = 0;
        let capped = repeats.min(ODS_MAX_COLUMNS - columns);
        if filled {
            columns += capped;
        } else {
            empty_run = capped;
        }
    }
}

/// A repeat attribute, 1 when absent. calamine refuses one that does not
/// parse, so this does too.
fn repeat(element: &quick_xml::events::BytesStart<'_>, name: &str) -> Result<u64> {
    match element
        .attributes()
        .flatten()
        .find(|attribute| attribute.key.as_ref() == name)
    {
        Some(attribute) => attribute
            .value
            .parse::<u64>()
            .context("Invalid repeat count in spreadsheet"),
        None => Ok(1),
    }
}

#[cfg(test)]
mod tests;
