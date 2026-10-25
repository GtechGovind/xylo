use super::{
    check,
    package::Package,
    xml::{self, Element},
};
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, Mutex, OnceLock},
};
use tokio_util::sync::CancellationToken;
use workbench_core::{
    DocumentCell, DocumentCellValue as Value, DocumentContent, DocumentEdit, DocumentOpenRequest,
    DocumentSheet,
};
const ROWS: u32 = 1_048_576;
const COLUMNS: u32 = 16_384;
#[derive(Clone)]
struct Sheet {
    name: String,
    path: String,
}
struct Cell<'a> {
    node: &'a Element,
    row: u32,
    column: u32,
    value: Value,
    reason: Option<String>,
}
struct Worksheet<'a> {
    data: &'a Element,
    cells: Vec<Cell<'a>>,
    ranges: Vec<(u32, u32, u32, u32)>,
    reason: Option<String>,
    rows: u32,
    columns: u32,
}
pub fn coordinate(address: &str) -> Result<(u32, u32)> {
    ensure!(
        !address.is_empty() && address.len() <= 10 && !address.contains('$'),
        "Use a single A1 coordinate without absolute markers"
    );
    let split = address
        .find(|c: char| c.is_ascii_digit())
        .context("Missing cell row")?;
    let mut col = 0u32;
    for ch in address[..split].bytes() {
        ensure!(
            ch.is_ascii_uppercase(),
            "Cell columns must use uppercase A1 notation"
        );
        col = col
            .checked_mul(26)
            .and_then(|v| v.checked_add(u32::from(ch - b'A' + 1)))
            .context("Cell column overflow")?;
    }
    let row = address[split..].parse::<u32>()?;
    ensure!(
        (1..=ROWS).contains(&row) && (1..=COLUMNS).contains(&col),
        "Cell exceeds Excel coordinate bounds"
    );
    Ok((row - 1, col - 1))
}
pub fn address(row: u32, column: u32) -> String {
    let mut n = column + 1;
    let mut letters = Vec::new();
    while n > 0 {
        letters.push((b'A' + ((n - 1) % 26) as u8) as char);
        n = (n - 1) / 26;
    }
    format!(
        "{}{}",
        letters.into_iter().rev().collect::<String>(),
        row + 1
    )
}
fn range(value: &str) -> Result<(u32, u32, u32, u32)> {
    let (a, b) = value.split_once(':').unwrap_or((value, value));
    let (r, c) = coordinate(a)?;
    let (er, ec) = coordinate(b)?;
    ensure!(r <= er && c <= ec, "Invalid worksheet range");
    Ok((r, c, er, ec))
}
fn part_path(target: &str) -> Result<String> {
    let joined = if target.starts_with('/') {
        target.trim_start_matches('/').to_owned()
    } else {
        format!("xl/{target}")
    };
    let mut parts = Vec::new();
    for part in joined.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop().context("Sheet relationship escapes package")?;
            }
            part => parts.push(part),
        }
    }
    ensure!(
        parts.first() == Some(&"xl"),
        "Sheet must remain in workbook package"
    );
    Ok(parts.join("/"))
}
fn sheets(package: &mut Package, cancel: &CancellationToken) -> Result<Vec<Sheet>> {
    let relationships = package.text("xl/_rels/workbook.xml.rels", cancel)?;
    let root = xml::parse(&relationships, cancel)?;
    let mut links = BTreeMap::new();
    for link in root.children_named("Relationship") {
        if link.attr("Type").is_some_and(|v| v.ends_with("/worksheet")) {
            ensure!(
                link.attr("TargetMode") != Some("External"),
                "External worksheets are not editable"
            );
            links.insert(
                link.attr("Id")
                    .context("Missing sheet relationship ID")?
                    .to_owned(),
                part_path(link.attr("Target").context("Missing sheet target")?)?,
            );
        }
    }
    let workbook = package.text("xl/workbook.xml", cancel)?;
    let root = xml::parse(&workbook, cancel)?;
    ensure!(
        root.name == "workbook",
        "Prefixed or unsupported workbook XML is not editable"
    );
    let list = root
        .children_named("sheets")
        .next()
        .context("Workbook has no sheet list")?;
    let mut result = Vec::new();
    let mut names = HashSet::new();
    for sheet in list.children_named("sheet") {
        let name = sheet.attr("name").context("Missing worksheet name")?;
        ensure!(
            name.len() <= 256 && names.insert(name.to_owned()),
            "Invalid/duplicate sheet name"
        );
        if let Some(path) = sheet.attr("r:id").and_then(|id| links.get(id)) {
            result.push(Sheet {
                name: name.into(),
                path: path.clone(),
            });
        }
    }
    ensure!(
        !result.is_empty() && result.len() <= 256,
        "Workbook needs 1..256 ordinary worksheets"
    );
    Ok(result)
}
fn shared_strings(
    package: &mut Package,
    cancel: &CancellationToken,
) -> Result<Vec<(String, bool)>> {
    if !package.contains("xl/sharedStrings.xml") {
        return Ok(Vec::new());
    }
    let text = package.text("xl/sharedStrings.xml", cancel)?;
    let root = xml::parse(&text, cancel)?;
    let mut result = Vec::new();
    for entry in root.children_named("si") {
        let mut nodes = Vec::new();
        entry.descendants("t", &mut nodes);
        let mut value = String::new();
        for node in nodes {
            value.push_str(&node.text(&text)?);
        }
        ensure!(
            value.chars().count() <= 32767,
            "Shared string exceeds Excel cell limit"
        );
        result.push((value, entry.children.iter().any(|c| c.name != "t")));
        ensure!(
            result.len() <= 100_000,
            "Too many shared strings for native editing"
        );
    }
    Ok(result)
}
fn cell_value(
    cell: &Element,
    text: &str,
    strings: &[(String, bool)],
) -> Result<(Value, Option<String>)> {
    let raw = cell
        .children_named("v")
        .next()
        .map(|n| n.text(text))
        .transpose()?
        .unwrap_or_default();
    if let Some(formula) = cell.children_named("f").next() {
        let expression = format!("={}", formula.text(text)?);
        let reason = formula
            .attr("t")
            .filter(|t| *t != "normal")
            .map(|_| "Shared/array/data-table formulas require an Office editor".into());
        return Ok((
            Value::Formula {
                expression,
                cached: (!raw.is_empty()).then_some(raw),
            },
            reason,
        ));
    }
    Ok(match cell.attr("t").unwrap_or("n"){
        "s"=>{let(value,rich)=strings.get(raw.parse::<usize>()?).context("Invalid shared-string reference")?;(Value::Text{text:value.clone()},rich.then(||"Rich-text cell styling is protected".into()))},
        "inlineStr"=>{let is=cell.children_named("is").next().context("Missing inline string")?;let mut nodes=Vec::new();is.descendants("t",&mut nodes);let mut value=String::new();for node in nodes{value.push_str(&node.text(text)?);}(Value::Text{text:value},is.children.iter().any(|v|v.name!="t").then(||"Rich-text cell styling is protected".into()))},
        "str"=>(Value::Text{text:raw},None),"b"=>{ensure!(matches!(raw.as_str(),"0"|"1"),"Invalid boolean cell");(Value::Boolean{value:raw=="1"},None)},
        "e"=>(Value::Error{code:raw},None),"d"=>(Value::DateTime{value:raw},Some("ISO date cells are displayed without converting their original type; use an Office editor to change them".into())),
        "n" if raw.is_empty()=>(Value::Blank,None),"n"=>{number(&raw)?;(Value::Number{value:raw},None)},_=>bail!("Unsupported original spreadsheet cell type"),
    })
}
fn worksheet<'a>(
    root: &'a Element,
    text: &str,
    strings: &[(String, bool)],
    cancel: &CancellationToken,
    include: impl Fn(u32, u32) -> bool,
    payload_budget: usize,
) -> Result<Worksheet<'a>> {
    ensure!(
        root.name == "worksheet",
        "Only standard unprefixed worksheet XML is editable"
    );
    let data = root
        .children_named("sheetData")
        .next()
        .context("Missing worksheet data")?;
    let mut reason = None;
    if root.children_named("sheetProtection").next().is_some() {
        reason = Some("Worksheet protection is enabled; native editing does not bypass it".into());
    }
    if root.children_named("tableParts").next().is_some() {
        reason = Some("Structured-table worksheet edits require an Office editor".into());
    }
    let mut cells = Vec::new();
    let mut rows = 0;
    let mut columns = 0;
    let mut ranges = Vec::new();
    let mut seen = HashSet::new();
    let mut previous_row = None;
    let mut payload_bytes = 0usize;
    for row in data.children_named("row") {
        check(cancel)?;
        let row_index = row
            .attr("r")
            .context("Explicit worksheet row coordinates are required")?
            .parse::<u32>()?
            .checked_sub(1)
            .context("Invalid row zero")?;
        ensure!(
            row_index < ROWS && previous_row.is_none_or(|p| row_index > p),
            "Unsorted/duplicate worksheet rows are unsupported"
        );
        previous_row = Some(row_index);
        rows = rows.max(row_index + 1);
        let mut previous_col = None;
        for cell in row.children_named("c") {
            let (r, c) = coordinate(
                cell.attr("r")
                    .context("Explicit cell coordinates are required")?,
            )?;
            ensure!(
                r == row_index && seen.insert((r, c)) && previous_col.is_none_or(|p| c > p),
                "Unsorted/duplicate worksheet cells are unsupported"
            );
            previous_col = Some(c);
            columns = columns.max(c + 1);
            let (value, mut protected) = if include(r, c) {
                cell_value(cell, text, strings)?
            } else {
                (Value::Blank, None)
            };
            let size = match &value {
                Value::Text { text } => text.len(),
                Value::Number { value } | Value::DateTime { value } => value.len(),
                Value::Formula { expression, cached } => {
                    expression.len() + cached.as_ref().map_or(0, String::len)
                }
                Value::Error { code } => code.len(),
                _ => 0,
            };
            payload_bytes = payload_bytes.saturating_add(size);
            ensure!(
                payload_bytes <= payload_budget,
                "Document page exceeds response budget; request fewer cells or save a smaller batch. Editable content was not truncated."
            );
            if cell.attr("cm").is_some()
                || cell.attr("vm").is_some()
                || cell.children_named("extLst").next().is_some()
            {
                protected=Some("This cell has linked or dynamic content. Use your Office editor to preserve it.".into());
            }
            if cell
                .children
                .iter()
                .any(|child| !matches!(child.name.as_str(), "f" | "v" | "is" | "extLst"))
            {
                protected = Some("Cell contains unsupported extension content".into());
            }
            if let Some(formula) = cell.children_named("f").next()
                && let Some(reference) = formula.attr("ref")
            {
                ranges.push(range(reference)?);
            }
            if include(r, c) {
                cells.push(Cell {
                    node: cell,
                    row: r,
                    column: c,
                    value,
                    reason: protected,
                });
            }
            ensure!(
                seen.len() <= 100_000,
                "Worksheet exceeds 100000 native editable cells"
            );
        }
    }
    if let Some(merges) = root.children_named("mergeCells").next() {
        for merge in merges.children_named("mergeCell") {
            ranges.push(range(merge.attr("ref").context("Invalid merged range")?)?);
        }
    }
    ensure!(
        ranges.len() <= 10000,
        "Worksheet has too many protected ranges"
    );
    Ok(Worksheet {
        data,
        cells,
        ranges,
        reason,
        rows,
        columns,
    })
}
fn protected(sheet: &Worksheet, row: u32, column: u32) -> Option<String> {
    sheet.reason.clone().or_else(|| {
        sheet
            .ranges
            .iter()
            .any(|(r, c, er, ec)| row >= *r && row <= *er && column >= *c && column <= *ec)
            .then(|| "Merged/shared/array range is protected".into())
    })
}
struct WorkbookMetadata {
    sheets: Vec<Sheet>,
    strings: Vec<(String, bool)>,
    dimensions: Vec<DocumentSheet>,
}
struct MetadataEntry {
    revision: String,
    workbook: Arc<WorkbookMetadata>,
}
static METADATA: OnceLock<Mutex<Option<MetadataEntry>>> = OnceLock::new();
fn dimensions(text: &str, cancel: &CancellationToken) -> Result<(u32, u32)> {
    let mut reader = quick_xml::Reader::from_str(text);
    let (mut rows, mut columns) = (0, 0);
    loop {
        check(cancel)?;
        match reader.read_event()? {
            quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e) => {
                if e.name().as_ref() == b"dimension" {
                    for attr in e.attributes() {
                        let attr = attr?;
                        if attr.key.as_ref() == b"ref" {
                            let value = attr.decode_and_unescape_value(reader.decoder())?;
                            let (_, _, r, c) = range(&value)?;
                            return Ok((r + 1, c + 1));
                        }
                    }
                }
                if e.name().as_ref() == b"c" {
                    for attr in e.attributes() {
                        let attr = attr?;
                        if attr.key.as_ref() == b"r" {
                            let value = attr.decode_and_unescape_value(reader.decoder())?;
                            let (r, c) = coordinate(&value)?;
                            rows = rows.max(r + 1);
                            columns = columns.max(c + 1);
                        }
                    }
                }
            }
            quick_xml::events::Event::DocType(_) => {
                bail!("Document type declarations are unsupported")
            }
            quick_xml::events::Event::Eof => return Ok((rows, columns)),
            _ => {}
        }
    }
}
fn metadata(
    package: &mut Package,
    revision: &str,
    cancel: &CancellationToken,
) -> Result<Arc<WorkbookMetadata>> {
    let cache = METADATA.get_or_init(|| Mutex::new(None));
    if let Some(entry) = cache
        .lock()
        .map_err(|_| anyhow::anyhow!("Workbook metadata cache unavailable"))?
        .as_ref()
        && entry.revision == revision
    {
        return Ok(entry.workbook.clone());
    }
    let sheets = sheets(package, cancel)?;
    let strings = shared_strings(package, cancel)?;
    let mut dimensions_list = Vec::new();
    for sheet in &sheets {
        let text = package.text(&sheet.path, cancel)?;
        let (rows, columns) = dimensions(&text, cancel)?;
        dimensions_list.push(DocumentSheet {
            name: sheet.name.clone(),
            rows,
            columns,
        });
    }
    let metadata = Arc::new(WorkbookMetadata {
        sheets,
        strings,
        dimensions: dimensions_list,
    });
    *cache
        .lock()
        .map_err(|_| anyhow::anyhow!("Workbook metadata cache unavailable"))? =
        Some(MetadataEntry {
            revision: revision.into(),
            workbook: metadata.clone(),
        });
    Ok(metadata)
}
pub fn open(
    package: &mut Package,
    args: &DocumentOpenRequest,
    revision: &str,
    payload_budget: usize,
    cancel: &CancellationToken,
) -> Result<DocumentContent> {
    ensure!(
        args.row_count > 0
            && args.row_count <= 200
            && args.column_count > 0
            && args.column_count <= 64
            && u64::from(args.row_count) * u64::from(args.column_count) <= 4096,
        "Spreadsheet page exceeds 4096 cells/200 rows/64 columns"
    );
    ensure!(
        args.row_start
            .checked_add(args.row_count)
            .is_some_and(|v| v <= ROWS)
            && args
                .column_start
                .checked_add(args.column_count)
                .is_some_and(|v| v <= COLUMNS),
        "Page exceeds Excel bounds"
    );
    let metadata = metadata(package, revision, cancel)?;
    let selected = args.sheet.as_deref().unwrap_or(&metadata.sheets[0].name);
    let sheet = metadata
        .sheets
        .iter()
        .find(|sheet| sheet.name == selected)
        .context("Worksheet no longer exists")?;
    // Only the selected worksheet is materialized. Unrelated large sheets must
    // not block paging a small sheet, and shared strings/metadata are revision-cached.
    let text = package.text(&sheet.path, cancel)?;
    let root = xml::parse(&text, cancel)?;
    let parsed = worksheet(
        &root,
        &text,
        &metadata.strings,
        cancel,
        |row, column| {
            row >= args.row_start
                && row < args.row_start + args.row_count
                && column >= args.column_start
                && column < args.column_start + args.column_count
        },
        payload_budget,
    )?;
    let lookup: BTreeMap<_, _> = parsed
        .cells
        .iter()
        .map(|cell| ((cell.row, cell.column), cell))
        .collect();
    let chain = package.contains("xl/calcChain.xml");
    let mut page = Vec::new();
    for row in args.row_start..args.row_start + args.row_count {
        for column in args.column_start..args.column_start + args.column_count {
            check(cancel)?;
            let cell = lookup.get(&(row, column));
            let value = cell.map_or(Value::Blank, |cell| cell.value.clone());
            let reason = protected(&parsed, row, column)
                .or_else(|| cell.and_then(|cell| cell.reason.clone()))
                .or_else(|| {
                    (chain && matches!(value, Value::Formula { .. })).then(|| {
                        "Formula edits with a calculation chain require an Office editor".into()
                    })
                });
            page.push(DocumentCell {
                address: address(row, column),
                row,
                column,
                value,
                editable: reason.is_none(),
                reason,
            });
        }
    }
    let mut sheets = metadata.dimensions.clone();
    if let Some(sheet) = sheets.iter_mut().find(|sheet| sheet.name == selected) {
        sheet.rows = parsed.rows;
        sheet.columns = parsed.columns;
    }
    Ok(DocumentContent::Spreadsheet {
        sheets,
        selected_sheet: selected.into(),
        row_start: args.row_start,
        row_count: args.row_count,
        column_start: args.column_start,
        column_count: args.column_count,
        cells: page,
        total_rows: parsed.rows,
        total_columns: parsed.columns,
    })
}

fn number(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'+' | b'e' | b'E'))
            && value.parse::<f64>()?.is_finite(),
        "Use a finite decimal spreadsheet number"
    );
    Ok(())
}
fn cell_xml(old: Option<&Element>, source: &str, address: &str, value: &Value) -> Result<String> {
    let mut attrs = old.map(|c| c.attrs.clone()).unwrap_or_default();
    attrs.insert("r".into(), address.into());
    attrs.remove("t");
    let value = match value {
        Value::Blank => String::new(),
        Value::Text { text } => {
            ensure!(
                text.chars().count() <= 32767,
                "Cell text exceeds Excel limit"
            );
            attrs.insert("t".into(), "inlineStr".into());
            format!(
                "<is><t xml:space=\"preserve\">{}</t></is>",
                xml::escape(text)?
            )
        }
        Value::Number { value } => {
            number(value)?;
            format!("<v>{value}</v>")
        }
        Value::Boolean { value } => {
            attrs.insert("t".into(), "b".into());
            format!("<v>{}</v>", u8::from(*value))
        }
        Value::Formula { expression, .. } => {
            ensure!(
                expression.starts_with('=')
                    && expression.chars().count() <= 8193
                    && expression.len() > 1,
                "Formula must start with '=' and contain at most 8192 expression characters"
            );
            format!("<f>{}</f>", xml::escape(&expression[1..])?)
        }
        Value::Error { code } => {
            ensure!(
                [
                    "#NULL!",
                    "#DIV/0!",
                    "#VALUE!",
                    "#REF!",
                    "#NAME?",
                    "#NUM!",
                    "#N/A",
                    "#GETTING_DATA"
                ]
                .contains(&code.as_str()),
                "Unsupported Excel error code"
            );
            attrs.insert("t".into(), "e".into());
            format!("<v>{}</v>", xml::escape(code)?)
        }
        Value::DateTime { .. } => bail!(
            "Date-type edits require an Office editor; choose explicit text/number to replace a supported cell"
        ),
    };
    let mut result = "<c".to_owned();
    for (name, value) in attrs {
        result.push_str(&format!(" {name}=\"{}\"", xml::escape(&value)?));
    }
    result.push('>');
    result.push_str(&value);
    if let Some(old) = old {
        for child in &old.children {
            if child.name == "extLst" {
                result.push_str(&source[child.start..child.end]);
            }
        }
    }
    result.push_str("</c>");
    Ok(result)
}
pub fn edit(
    package: &mut Package,
    edits: &[DocumentEdit],
    cancel: &CancellationToken,
) -> Result<BTreeMap<String, String>> {
    let sheets = sheets(package, cancel)?;
    let strings = shared_strings(package, cancel)?;
    let chain = package.contains("xl/calcChain.xml");
    let mut grouped: BTreeMap<String, BTreeMap<(u32, u32), &Value>> = BTreeMap::new();
    for edit in edits {
        let DocumentEdit::Cell {
            sheet,
            address,
            value,
        } = edit
        else {
            bail!("Spreadsheet edits must target typed cells")
        };
        ensure!(
            sheets.iter().any(|s| s.name == *sheet),
            "Edited sheet no longer exists"
        );
        let key = coordinate(address)?;
        ensure!(
            grouped
                .entry(sheet.clone())
                .or_default()
                .insert(key, value)
                .is_none(),
            "Coalesce repeated cell edits before saving"
        );
    }
    let mut output = BTreeMap::new();
    for sheet in sheets {
        let Some(changes) = grouped.get(&sheet.name) else {
            continue;
        };
        let text = package.text(&sheet.path, cancel)?;
        let root = xml::parse(&text, cancel)?;
        let parsed = worksheet(
            &root,
            &text,
            &strings,
            cancel,
            |row, column| changes.contains_key(&(row, column)),
            16 * 1024 * 1024,
        )?;
        let lookup: BTreeMap<_, _> = parsed
            .cells
            .iter()
            .map(|c| ((c.row, c.column), c))
            .collect();
        let rows: BTreeMap<u32, &Element> = parsed
            .data
            .children_named("row")
            .map(|r| Ok((r.attr("r").context("Missing row")?.parse::<u32>()? - 1, r)))
            .collect::<Result<_>>()?;
        let mut replacements = Vec::new();
        let mut additions: BTreeMap<u32, Vec<(u32, String)>> = BTreeMap::new();
        for (&(row, column), value) in changes {
            check(cancel)?;
            ensure!(
                protected(&parsed, row, column).is_none(),
                "Cannot edit a protected/merged/array/table range"
            );
            let old = lookup.get(&(row, column));
            ensure!(
                old.is_none_or(|c| c.reason.is_none()),
                "Cannot edit protected rich/shared/formula cell content"
            );
            ensure!(
                !chain
                    || !matches!(value, Value::Formula { .. })
                        && old.is_none_or(|c| !matches!(c.value, Value::Formula { .. })),
                "Formula edits with a calculation chain require an Office editor"
            );
            let replacement = cell_xml(old.map(|c| c.node), &text, &address(row, column), value)?;
            if let Some(old) = old {
                replacements.push((old.node.start, old.node.end, replacement));
            } else {
                additions
                    .entry(row)
                    .or_default()
                    .push((column, replacement));
            }
        }
        let mut new_rows = Vec::new();
        for (row, cells) in additions {
            if let Some(existing) = rows.get(&row) {
                let inserts = cells
                    .into_iter()
                    .map(|(column, value)| {
                        let position = existing
                            .children_named("c")
                            .find(|c| {
                                c.attr("r")
                                    .and_then(|r| coordinate(r).ok())
                                    .is_some_and(|(_, c)| c > column)
                            })
                            .map_or(existing.close_start, |c| c.start);
                        (position, position, value)
                    })
                    .collect::<Vec<_>>();
                if text[existing.start..existing.end]
                    .trim_end()
                    .ends_with("/>")
                {
                    let values = inserts.into_iter().map(|(_, _, v)| v).collect::<String>();
                    replacements.push((
                        existing.start,
                        existing.end,
                        format!("{}>{values}</row>", &text[existing.start..existing.end - 2]),
                    ));
                } else {
                    replacements.extend(inserts);
                }
            } else {
                new_rows.push((
                    row,
                    format!(
                        "<row r=\"{}\">{}</row>",
                        row + 1,
                        cells.into_iter().map(|(_, v)| v).collect::<String>()
                    ),
                ));
            }
        }
        if text[parsed.data.start..parsed.data.end]
            .trim_end()
            .ends_with("/>")
            && !new_rows.is_empty()
        {
            let values = new_rows.into_iter().map(|(_, v)| v).collect::<String>();
            replacements.push((
                parsed.data.start,
                parsed.data.end,
                format!(
                    "{}>{values}</sheetData>",
                    &text[parsed.data.start..parsed.data.end - 2]
                ),
            ));
        } else {
            for (row, value) in new_rows {
                let position = rows
                    .range((row + 1)..)
                    .next()
                    .map_or(parsed.data.close_start, |(_, r)| r.start);
                replacements.push((position, position, value));
            }
        }
        if let Some(dimension) = root.children_named("dimension").next() {
            let max_row = changes
                .keys()
                .map(|(r, _)| *r)
                .max()
                .unwrap_or(0)
                .max(parsed.rows.saturating_sub(1));
            let max_col = changes
                .keys()
                .map(|(_, c)| *c)
                .max()
                .unwrap_or(0)
                .max(parsed.columns.saturating_sub(1));
            replacements.push((
                dimension.start,
                dimension.end,
                format!("<dimension ref=\"A1:{}\"/>", address(max_row, max_col)),
            ));
        }
        output.insert(sheet.path, xml::patches(&text, replacements)?);
    }
    let workbook = package.text("xl/workbook.xml", cancel)?;
    let root = xml::parse(&workbook, cancel)?;
    let calc = root.children_named("calcPr").next();
    let mut attrs = calc.map(|v| v.attrs.clone()).unwrap_or_default();
    attrs.insert("fullCalcOnLoad".into(), "1".into());
    attrs.insert("forceFullCalc".into(), "1".into());
    let mut tag = "<calcPr".to_owned();
    for (k, v) in attrs {
        tag.push_str(&format!(" {k}=\"{}\"", xml::escape(&v)?));
    }
    tag.push_str("/>");
    let (start, end) = calc.map_or((root.close_start, root.close_start), |c| (c.start, c.end));
    output.insert(
        "xl/workbook.xml".into(),
        xml::patches(&workbook, vec![(start, end, tag)])?,
    );
    Ok(output)
}
