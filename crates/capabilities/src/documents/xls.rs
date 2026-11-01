//! A bounded reader for legacy Excel 97-2003 (`.xls`, BIFF8) workbooks.
//!
//! calamine's `.xls` reader follows Compound File sector chains without
//! cycle checks, reserves cells from the declared `DIMENSIONS`, and parses
//! every sheet into a bounding-box range when the workbook opens. This
//! reader instead:
//!
//! - walks every Compound File chain (DIFAT, FAT, directory, mini FAT and
//!   streams) with a visited set, so a loop is an error and no chain can
//!   yield more sectors than the file holds;
//! - never sizes an allocation from a declared count: the FAT, the
//!   `Workbook` stream and the shared strings only grow with bytes actually
//!   present, and the shared strings are packed into one buffer;
//! - ignores `DIMENSIONS` and streams the cells of one sheet at a time, so
//!   the caller keeps only the window it shows.
//!
//! Only the records that carry text and number values are decoded: `SST`,
//! `LABELSST`, `LABEL`, `NUMBER`, `RK`, `MULRK`, `BOOLERR` and the cached
//! results of `FORMULA` (with `STRING`). Anything malformed is an error.

use super::{check, workbook::Visit};
use anyhow::{Context, Result, bail, ensure};
use calamine::{CellErrorType, DataRef};
use tokio_util::sync::CancellationToken;

/// Compound File signature.
const SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
/// Ends a sector chain.
const END_OF_CHAIN: u32 = 0xFFFF_FFFE;
/// Marks an unused sector or DIFAT slot.
const FREE_SECTOR: u32 = 0xFFFF_FFFF;
/// FAT sector locations stored in the header itself.
const HEADER_DIFAT: usize = 109;
/// Size of a mini stream sector.
const MINI_SECTOR: usize = 64;
/// Streams smaller than this live in the mini stream.
const MINI_CUTOFF: u64 = 4096;
/// Size of a directory entry.
const DIRECTORY_ENTRY: usize = 128;

/// BIFF record types.
const BOF: u16 = 0x0809;
const EOF: u16 = 0x000A;
const FILEPASS: u16 = 0x002F;
const BOUNDSHEET: u16 = 0x0085;
const SST: u16 = 0x00FC;
const CONTINUE: u16 = 0x003C;
const NUMBER: u16 = 0x0203;
const RK: u16 = 0x027E;
const MULRK: u16 = 0x00BD;
const LABELSST: u16 = 0x00FD;
const LABEL: u16 = 0x0204;
const BOOLERR: u16 = 0x0205;
const FORMULA: u16 = 0x0006;
const STRING: u16 = 0x0207;
const SHRFMLA: u16 = 0x04BC;
const ARRAY: u16 = 0x0221;
const TABLE: u16 = 0x0236;

/// BIFF8 version and substream types in a `BOF` record.
const BIFF8: u16 = 0x0600;
const GLOBALS: u16 = 0x0005;
const WORKSHEET: u16 = 0x0010;

const MALFORMED: &str = "Spreadsheet .xls data is truncated or malformed";
const UNSUPPORTED: &str = "Only Excel 97-2003 (BIFF8) .xls workbooks can be read; save the workbook as .xlsx and try again";

/// A parsed BIFF8 workbook: its `Workbook` stream, worksheets and shared
/// strings. Cells are decoded on demand, one sheet at a time.
pub(super) struct Workbook {
    stream: Vec<u8>,
    /// Worksheet names, packed so a flood of `BOUNDSHEET` records costs
    /// less than the records themselves.
    sheets: Sheets,
    strings: SharedStrings,
    cancel: CancellationToken,
}

impl Workbook {
    /// Parses the Compound File in `bytes` and the globals of its
    /// `Workbook` stream.
    pub(super) fn parse(bytes: &[u8], cancel: &CancellationToken) -> Result<Self> {
        let stream = workbook_stream(bytes)?;
        let (sheets, strings) = globals(&stream, cancel)?;
        Ok(Self {
            stream,
            sheets,
            strings,
            cancel: cancel.clone(),
        })
    }

    /// Worksheet names in workbook order.
    pub(super) fn sheet_names(&self) -> impl Iterator<Item = &str> {
        self.sheets.iter().map(|(name, _)| name)
    }

    /// Calls `visit` with the position and value of each cell of `sheet`
    /// that carries a value, in file order.
    pub(super) fn each(&self, sheet: &str, visit: &mut Visit<'_>) -> Result<()> {
        let offset = self
            .sheets
            .iter()
            .find(|(name, _)| *name == sheet)
            .map(|(_, offset)| offset as usize)
            .with_context(|| format!("Workbook has no sheet named {sheet}"))?;
        let mut records = Records::at(&self.stream, offset);
        let (kind, data) = records.next()?.context(MALFORMED)?;
        ensure!(
            kind == BOF && u16_at(data, 0)? == BIFF8 && u16_at(data, 2)? == WORKSHEET,
            "Sheet {sheet} is not a BIFF8 worksheet"
        );
        // Embedded charts nest their own BOF..EOF substreams.
        let mut depth = 1u32;
        // The cell whose formula result arrives in the next STRING record.
        let mut pending = None;
        let mut seen = 0u64;
        while depth > 0 {
            seen += 1;
            if seen.is_multiple_of(4096) {
                check(&self.cancel)?;
            }
            let (kind, data) = records.next()?.context(MALFORMED)?;
            match kind {
                BOF => {
                    depth += 1;
                    continue;
                }
                EOF => {
                    depth -= 1;
                    continue;
                }
                _ if depth > 1 => continue,
                STRING => {
                    if let Some(cell) = pending.take() {
                        let mut segments = Segments::new(data, records.clone());
                        let text = segments.unicode_string()?;
                        records = segments.records;
                        visit(cell, &DataRef::String(text))?;
                    }
                    continue;
                }
                // These may sit between a FORMULA and its STRING.
                SHRFMLA | ARRAY | TABLE | CONTINUE => continue,
                _ => pending = None,
            }
            match kind {
                NUMBER => visit(position(data)?, &DataRef::Float(f64_at(data, 6)?))?,
                RK => visit(position(data)?, &DataRef::Float(rk(u32_at(data, 6)?)))?,
                MULRK => {
                    ensure!(
                        data.len() >= 12 && (data.len() - 6).is_multiple_of(6),
                        MALFORMED
                    );
                    let (row, first) = position(data)?;
                    let count = (data.len() - 6) / 6;
                    let last = u32::from(u16_at(data, data.len() - 2)?);
                    ensure!(last + 1 == first + count as u32, MALFORMED);
                    for index in 0..count {
                        let value = rk(u32_at(data, 6 + index * 6)?);
                        visit((row, first + index as u32), &DataRef::Float(value))?;
                    }
                }
                LABELSST => {
                    let text = self.strings.get(u32_at(data, 6)?)?;
                    visit(position(data)?, &DataRef::SharedString(text))?;
                }
                LABEL => {
                    let mut segments =
                        Segments::new(data.get(6..).context(MALFORMED)?, records.clone());
                    let text = segments.unicode_string()?;
                    records = segments.records;
                    visit(position(data)?, &DataRef::String(text))?;
                }
                BOOLERR => {
                    let value = *data.get(6).context(MALFORMED)?;
                    let cell = match *data.get(7).context(MALFORMED)? {
                        0 => DataRef::Bool(value != 0),
                        _ => DataRef::Error(error(value)?),
                    };
                    visit(position(data)?, &cell)?;
                }
                FORMULA => {
                    ensure!(data.len() >= 20, MALFORMED);
                    let cell = position(data)?;
                    let result = &data[6..14];
                    if result[6..8] != [0xFF, 0xFF] {
                        visit(cell, &DataRef::Float(f64_at(data, 6)?))?;
                        continue;
                    }
                    match result[0] {
                        0 => pending = Some(cell),
                        1 => visit(cell, &DataRef::Bool(result[2] != 0))?,
                        2 => visit(cell, &DataRef::Error(error(result[2])?))?,
                        3 => visit(cell, &DataRef::String(String::new()))?,
                        _ => bail!(MALFORMED),
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// Worksheet names packed into one buffer, with each name's end and the
/// stream offset of the sheet's `BOF` record.
#[derive(Default)]
struct Sheets {
    names: String,
    entries: Vec<(u32, u32)>,
}

impl Sheets {
    fn push(&mut self, name: &str, offset: u32) -> Result<()> {
        self.names.push_str(name);
        let end = u32::try_from(self.names.len()).context("Sheet names are too large")?;
        self.entries.push((end, offset));
        Ok(())
    }

    /// Each name and offset in workbook order.
    fn iter(&self) -> impl Iterator<Item = (&str, u32)> {
        let mut start = 0;
        self.entries.iter().map(move |&(end, offset)| {
            let name = &self.names[start as usize..end as usize];
            start = end;
            (name, offset)
        })
    }
}

/// Shared strings packed into one buffer, so a table of many short strings
/// costs a few bytes each rather than a `String` each.
#[derive(Default)]
struct SharedStrings {
    text: String,
    /// End of each string in `text`.
    ends: Vec<u32>,
}

impl SharedStrings {
    /// String `index`, or an error when the table has no such string.
    fn get(&self, index: u32) -> Result<&str> {
        let index = index as usize;
        let end = *self
            .ends
            .get(index)
            .context("Cell refers to a missing shared string")?;
        let start = match index {
            0 => 0,
            _ => self.ends[index - 1],
        };
        Ok(&self.text[start as usize..end as usize])
    }
}

/// The worksheets and shared strings declared by the globals substream at
/// the start of `stream`.
fn globals(stream: &[u8], cancel: &CancellationToken) -> Result<(Sheets, SharedStrings)> {
    let mut records = Records::at(stream, 0);
    let (kind, data) = records.next()?.context(MALFORMED)?;
    ensure!(
        kind == BOF && u16_at(data, 0)? == BIFF8 && u16_at(data, 2)? == GLOBALS,
        UNSUPPORTED
    );
    let mut sheets = Sheets::default();
    let mut strings = None;
    let mut seen = 0u64;
    loop {
        seen += 1;
        if seen.is_multiple_of(4096) {
            check(cancel)?;
        }
        let (kind, data) = records.next()?.context(MALFORMED)?;
        match kind {
            EOF => break,
            FILEPASS => {
                bail!("Encrypted .xls workbooks cannot be read; remove the password and try again")
            }
            BOUNDSHEET => {
                let offset = u32_at(data, 0)?;
                // Chart, macro and module sheets carry no cells.
                if *data.get(5).context(MALFORMED)? == 0 {
                    sheets.push(&short_string(data.get(6..).context(MALFORMED)?)?, offset)?;
                }
            }
            SST => {
                ensure!(
                    strings.is_none(),
                    "Workbook declares more than one shared string table"
                );
                let mut segments = Segments::new(data, records.clone());
                strings = Some(shared_strings(&mut segments, stream.len(), cancel)?);
                records = segments.records;
            }
            _ => {}
        }
    }
    Ok((sheets, strings.unwrap_or_default()))
}

/// Reads an `SST` record and its continuations. The declared count must
/// fit the stream, at three bytes per string at least, and every declared
/// string must be present.
fn shared_strings(
    segments: &mut Segments<'_>,
    stream_size: usize,
    cancel: &CancellationToken,
) -> Result<SharedStrings> {
    let _total = segments.u32()?;
    let unique = segments.u32()?;
    ensure!(
        u64::from(unique) <= stream_size as u64 / 3,
        "Workbook declares more shared strings than it contains"
    );
    let mut strings = SharedStrings::default();
    let mut units = Vec::new();
    for index in 0..unique {
        if index.is_multiple_of(4096) {
            check(cancel)?;
        }
        segments.rich_string(&mut units)?;
        push_utf16(&mut strings.text, &units);
        strings
            .ends
            .push(u32::try_from(strings.text.len()).context("Shared strings are too large")?);
    }
    Ok(strings)
}

/// A `ShortXLUnicodeString`: an 8-bit length, flags and the characters.
fn short_string(data: &[u8]) -> Result<String> {
    let count = usize::from(*data.first().context(MALFORMED)?);
    let wide = data.get(1).context(MALFORMED)? & 1 != 0;
    let bytes = data
        .get(2..2 + count * if wide { 2 } else { 1 })
        .context(MALFORMED)?;
    let mut text = String::new();
    push_utf16(&mut text, &chars(bytes, wide));
    Ok(text)
}

/// UTF-16 code units from compressed (Latin-1) or wide characters.
fn chars(bytes: &[u8], wide: bool) -> Vec<u16> {
    if wide {
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect()
    } else {
        bytes.iter().map(|&byte| u16::from(byte)).collect()
    }
}

/// Appends UTF-16 `units` to `text`, replacing unpaired surrogates.
fn push_utf16(text: &mut String, units: &[u16]) {
    text.extend(
        char::decode_utf16(units.iter().copied())
            .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER)),
    );
}

/// The row and column at the start of a cell record.
fn position(data: &[u8]) -> Result<(u32, u32)> {
    Ok((u32::from(u16_at(data, 0)?), u32::from(u16_at(data, 2)?)))
}

/// Decodes an `RK` number: a 30-bit integer or the high bits of a double,
/// optionally divided by 100.
fn rk(value: u32) -> f64 {
    let number = if value & 2 != 0 {
        f64::from((value as i32) >> 2)
    } else {
        f64::from_bits(u64::from(value & 0xFFFF_FFFC) << 32)
    };
    if value & 1 != 0 {
        number / 100.0
    } else {
        number
    }
}

/// The cell error for a BIFF error code.
fn error(code: u8) -> Result<CellErrorType> {
    Ok(match code {
        0x00 => CellErrorType::Null,
        0x07 => CellErrorType::Div0,
        0x0F => CellErrorType::Value,
        0x17 => CellErrorType::Ref,
        0x1D => CellErrorType::Name,
        0x24 => CellErrorType::Num,
        0x2A => CellErrorType::NA,
        0x2B => CellErrorType::GettingData,
        _ => bail!(MALFORMED),
    })
}

/// BIFF records from an offset of the `Workbook` stream.
#[derive(Clone)]
struct Records<'a> {
    stream: &'a [u8],
    position: usize,
}

impl<'a> Records<'a> {
    fn at(stream: &'a [u8], position: usize) -> Self {
        Self { stream, position }
    }

    /// The next record's type and data, or `None` at the end of the stream.
    fn next(&mut self) -> Result<Option<(u16, &'a [u8])>> {
        if self.position == self.stream.len() {
            return Ok(None);
        }
        let kind = u16_at(self.stream.get(self.position..).context(MALFORMED)?, 0)?;
        let size = usize::from(u16_at(self.stream, self.position + 2)?);
        let start = self.position + 4;
        let data = self.stream.get(start..start + size).context(MALFORMED)?;
        self.position = start + size;
        Ok(Some((kind, data)))
    }

    /// The data of the next record when it is a `CONTINUE`.
    fn next_continue(&mut self) -> Result<Option<&'a [u8]>> {
        let mut ahead = self.clone();
        match ahead.next()? {
            Some((CONTINUE, data)) => {
                *self = ahead;
                Ok(Some(data))
            }
            _ => Ok(None),
        }
    }
}

/// The bytes of one record followed by its `CONTINUE` records, pulled one
/// record at a time.
struct Segments<'a> {
    current: &'a [u8],
    /// Positioned after the last record consumed.
    records: Records<'a>,
}

impl<'a> Segments<'a> {
    fn new(current: &'a [u8], records: Records<'a>) -> Self {
        Self { current, records }
    }

    /// Moves to the next `CONTINUE` record.
    fn advance(&mut self) -> Result<()> {
        self.current = self.records.next_continue()?.context(MALFORMED)?;
        Ok(())
    }

    fn byte(&mut self) -> Result<u8> {
        while self.current.is_empty() {
            self.advance()?;
        }
        let byte = self.current[0];
        self.current = &self.current[1..];
        Ok(byte)
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes([self.byte()?, self.byte()?]))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes([
            self.byte()?,
            self.byte()?,
            self.byte()?,
            self.byte()?,
        ]))
    }

    /// Skips `count` bytes, which may span records.
    fn skip(&mut self, mut count: usize) -> Result<()> {
        while count > 0 {
            if self.current.is_empty() {
                self.advance()?;
            }
            let step = count.min(self.current.len());
            self.current = &self.current[step..];
            count -= step;
        }
        Ok(())
    }

    /// Reads `count` characters into `units`. A `CONTINUE` that splits the
    /// characters starts with a flags byte giving their width from there.
    fn characters(&mut self, count: usize, mut wide: bool, units: &mut Vec<u16>) -> Result<()> {
        units.clear();
        while units.len() < count {
            if self.current.is_empty() {
                self.advance()?;
                wide = self.byte()? & 1 != 0;
            }
            let width = if wide { 2 } else { 1 };
            let take = (count - units.len()).min(self.current.len() / width);
            ensure!(take > 0, MALFORMED);
            let (bytes, rest) = self.current.split_at(take * width);
            units.extend(chars(bytes, wide));
            self.current = rest;
        }
        Ok(())
    }

    /// An `XLUnicodeString` (`LABEL`, `STRING`): length, flags, characters.
    fn unicode_string(&mut self) -> Result<String> {
        let count = usize::from(self.u16()?);
        let wide = self.byte()? & 1 != 0;
        let mut units = Vec::new();
        self.characters(count, wide, &mut units)?;
        let mut text = String::new();
        push_utf16(&mut text, &units);
        Ok(text)
    }

    /// An `XLUnicodeRichExtendedString` (`SST`) into `units`, skipping its
    /// formatting runs and phonetic data.
    fn rich_string(&mut self, units: &mut Vec<u16>) -> Result<()> {
        let count = usize::from(self.u16()?);
        let flags = self.byte()?;
        let runs = if flags & 0x08 != 0 {
            usize::from(self.u16()?)
        } else {
            0
        };
        let extended = if flags & 0x04 != 0 {
            self.u32()? as usize
        } else {
            0
        };
        self.characters(count, flags & 1 != 0, units)?;
        self.skip(runs * 4)?;
        self.skip(extended)
    }
}

/// The `Workbook` stream of a Compound File.
fn workbook_stream(bytes: &[u8]) -> Result<Vec<u8>> {
    let file = CompoundFile::parse(bytes)?;
    let directory = file.directory()?;
    let root = directory.first().context(MALFORMED)?;
    ensure!(root.kind == 5, "Spreadsheet .xls has no root storage");
    let Some(entry) = directory
        .iter()
        .find(|entry| entry.kind == 2 && entry.name.eq_ignore_ascii_case("Workbook"))
    else {
        if directory
            .iter()
            .any(|entry| entry.kind == 2 && entry.name.eq_ignore_ascii_case("Book"))
        {
            bail!(UNSUPPORTED);
        }
        bail!("Spreadsheet .xls has no Workbook stream");
    };
    ensure!(
        entry.size <= bytes.len() as u64,
        "Workbook stream is larger than the file"
    );
    let size = entry.size as usize;
    if entry.size >= MINI_CUTOFF {
        return gather(&file.fat, entry.start, Some(size), |id| file.sector(id));
    }
    let mini_stream = gather(
        &file.fat,
        root.start,
        Some(root.size_within(bytes)?),
        |id| file.sector(id),
    )?;
    let mut mini_fat = table(&gather(&file.fat, file.first_mini_fat, None, |id| {
        file.sector(id)
    })?);
    mini_fat.truncate(mini_stream.len().div_ceil(MINI_SECTOR));
    gather(&mini_fat, entry.start, Some(size), |id| {
        let start = id as usize * MINI_SECTOR;
        let end = (start + MINI_SECTOR).min(mini_stream.len());
        mini_stream.get(start..end).context(MALFORMED)
    })
}

/// A Compound File's sectors and allocation table.
struct CompoundFile<'a> {
    bytes: &'a [u8],
    sector_size: usize,
    /// Sectors after the header; the last may be short.
    sectors: usize,
    /// Next sector of each sector, one entry per sector in the file.
    fat: Vec<u32>,
    first_directory: u32,
    first_mini_fat: u32,
    /// Version 3 directory entries keep only 32 bits of a stream size.
    version: u16,
}

impl<'a> CompoundFile<'a> {
    /// Reads the header, walks the DIFAT and loads the FAT.
    fn parse(bytes: &'a [u8]) -> Result<Self> {
        ensure!(
            bytes.len() >= 512 && bytes[..8] == SIGNATURE,
            "File is not an Excel 97-2003 .xls workbook"
        );
        let version = u16_at(bytes, 0x1A)?;
        let sector_size = match (version, u16_at(bytes, 0x1E)?) {
            (3, 9) => 512,
            (4, 12) => 4096,
            _ => bail!(MALFORMED),
        };
        ensure!(
            u16_at(bytes, 0x20)? == 6 && u64::from(u32_at(bytes, 0x38)?) == MINI_CUTOFF,
            MALFORMED
        );
        ensure!(bytes.len() >= sector_size, MALFORMED);
        let sectors = (bytes.len() - sector_size).div_ceil(sector_size);
        let mut file = Self {
            bytes,
            sector_size,
            sectors,
            fat: Vec::new(),
            first_directory: u32_at(bytes, 0x30)?,
            first_mini_fat: u32_at(bytes, 0x3C)?,
            version,
        };
        let fat_sectors = u32_at(bytes, 0x2C)? as usize;
        let difat_sectors = u32_at(bytes, 0x48)? as usize;
        ensure!(
            fat_sectors <= sectors && difat_sectors <= sectors,
            "Spreadsheet .xls declares more sectors than it contains"
        );
        // Every DIFAT sector is visited at most once, so a loop is an error
        // and the walk ends within the file.
        let mut locations = table(&bytes[0x4C..0x4C + HEADER_DIFAT * 4]);
        let mut visited = vec![false; sectors];
        let mut next = u32_at(bytes, 0x44)?;
        for _ in 0..difat_sectors {
            let index = next as usize;
            ensure!(
                index < sectors && !std::mem::replace(&mut visited[index], true),
                "Spreadsheet .xls DIFAT chain loops or leaves the file"
            );
            let sector = file.full_sector(next)?;
            let (entries, link) = sector.split_at(sector_size - 4);
            locations.extend(table(entries));
            next = u32_at(link, 0)?;
        }
        ensure!(
            difat_sectors == 0 || matches!(next, END_OF_CHAIN | FREE_SECTOR),
            "Spreadsheet .xls DIFAT chain is longer than declared"
        );
        ensure!(locations.len() >= fat_sectors, MALFORMED);
        let mut fat = Vec::with_capacity(fat_sectors * sector_size / 4);
        for &location in &locations[..fat_sectors] {
            fat.extend(table(file.full_sector(location)?));
        }
        fat.truncate(sectors);
        file.fat = fat;
        Ok(file)
    }

    /// Sector `id`, cut short at the end of the file.
    fn sector(&self, id: u32) -> Result<&'a [u8]> {
        let index = id as usize;
        ensure!(index < self.sectors, MALFORMED);
        let start = (index + 1) * self.sector_size;
        let end = (start + self.sector_size).min(self.bytes.len());
        Ok(&self.bytes[start..end])
    }

    /// Sector `id`, which must be complete.
    fn full_sector(&self, id: u32) -> Result<&'a [u8]> {
        let sector = self.sector(id)?;
        ensure!(sector.len() == self.sector_size, MALFORMED);
        Ok(sector)
    }

    /// Every directory entry.
    fn directory(&self) -> Result<Vec<Entry>> {
        let bytes = gather(&self.fat, self.first_directory, None, |id| self.sector(id))?;
        bytes
            .as_chunks::<DIRECTORY_ENTRY>()
            .0
            .iter()
            .map(|entry| Entry::parse(entry, self.version))
            .collect()
    }
}

/// A Compound File directory entry.
struct Entry {
    name: String,
    /// 0 unused, 1 storage, 2 stream, 5 root.
    kind: u8,
    start: u32,
    size: u64,
}

impl Entry {
    fn parse(bytes: &[u8], version: u16) -> Result<Self> {
        let name_size = usize::from(u16_at(bytes, 0x40)?);
        ensure!(name_size <= 64 && name_size.is_multiple_of(2), MALFORMED);
        let units = chars(&bytes[..name_size.saturating_sub(2)], true);
        let mut name = String::new();
        push_utf16(&mut name, &units);
        let size = match version {
            3 => u64::from(u32_at(bytes, 0x78)?),
            _ => u64_at(bytes, 0x78)?,
        };
        Ok(Self {
            name,
            kind: bytes[0x42],
            start: u32_at(bytes, 0x74)?,
            size,
        })
    }

    /// The declared size, which must fit in the file.
    fn size_within(&self, bytes: &[u8]) -> Result<usize> {
        ensure!(self.size <= bytes.len() as u64, MALFORMED);
        Ok(self.size as usize)
    }
}

/// Follows the chain from `start` through `table`, appending each sector
/// until `size` bytes are read (or, without a size, to the end of the
/// chain). Each sector is visited at most once, so a loop is an error and
/// the result never exceeds the sectors behind `table`.
fn gather<'s>(
    table: &[u32],
    start: u32,
    size: Option<usize>,
    sector: impl Fn(u32) -> Result<&'s [u8]>,
) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(size.unwrap_or(0));
    let mut visited = vec![false; table.len()];
    let mut next = start;
    while size.is_none_or(|size| bytes.len() < size) && next != END_OF_CHAIN {
        let index = next as usize;
        ensure!(
            index < table.len(),
            "Spreadsheet .xls sector chain leaves the file"
        );
        ensure!(
            !std::mem::replace(&mut visited[index], true),
            "Spreadsheet .xls sector chain loops"
        );
        let data = sector(next)?;
        let take = size.map_or(data.len(), |size| data.len().min(size - bytes.len()));
        bytes.extend_from_slice(&data[..take]);
        next = table[index];
    }
    if let Some(size) = size {
        ensure!(bytes.len() == size, "Spreadsheet .xls stream is truncated");
    }
    Ok(bytes)
}

/// Little-endian `u32` entries of an allocation table.
fn table(bytes: &[u8]) -> Vec<u32> {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|entry| u32::from_le_bytes(*entry))
        .collect()
}

fn u16_at(bytes: &[u8], at: usize) -> Result<u16> {
    let bytes = bytes.get(at..at + 2).context(MALFORMED)?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    let bytes = bytes.get(at..at + 4).context(MALFORMED)?;
    Ok(u32::from_le_bytes(bytes.try_into()?))
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64> {
    let bytes = bytes.get(at..at + 8).context(MALFORMED)?;
    Ok(u64::from_le_bytes(bytes.try_into()?))
}

fn f64_at(bytes: &[u8], at: usize) -> Result<f64> {
    Ok(f64::from_bits(u64_at(bytes, at)?))
}

#[cfg(test)]
mod tests;
