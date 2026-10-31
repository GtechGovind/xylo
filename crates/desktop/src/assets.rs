//! Application asset source: the toolkit's default icons plus the extra
//! glyphs Xylo's design uses. Only the listed SVGs are embedded.
//!
//! Every outline icon is served with a soft 1.5 stroke instead of Lucide's
//! default 2 so toolbar, row and toolkit icons (dropdown chevrons, checkboxes,
//! menus) share one calm weight without touching call sites. The rewrite runs
//! once per embedded path and only replaces the root `<svg>` element's
//! `stroke-width`; icons without one (filled marks, window controls) are served
//! unchanged. Glyph choice and sizing live in `crate::icons`.
use gpui_kit::AssetSource;
use gpui_kit::assets::icon_assets;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::LazyLock;

icon_assets!(
    ExtraIcons,
    [
        Brain,
        CalendarClock,
        CircleDashed,
        Columns2,
        Container,
        FileDiff,
        FilePen,
        FileSearch,
        Files,
        FolderPlus,
        Gauge,
        GitBranch,
        Image,
        Lightbulb,
        ListChecks,
        MessageSquare,
        NotebookPen,
        Pencil,
        Pin,
        Plug,
        Puzzle,
        RotateCcw,
        Server,
        Shield,
        ShieldAlert,
        ShieldCheck,
        ShieldQuestionMark,
        SlidersHorizontal,
        Sparkles,
        SquarePen,
        Terminal,
        Trash,
        Volume2,
        Wrench,
        X,
    ]
);

/// Xylo's own mark: a prompt chevron and cursor line (the coding side) with a
/// four-point spark (the assistant side). Drawn for this product.
use crate::ui::XYLO_MARK;
const XYLO_MARK_SVG: &[u8] = include_bytes!("../assets/xylo-mark.svg");

/// Stroke width every outline icon is drawn with, in the 24-unit Lucide grid.
pub const SOFT_STROKE_WIDTH: &str = "1.5";

/// Every embedded icon, softened once on first use. Extras win over toolkit
/// icons that share a path; both come from the same Lucide catalog.
static ICONS: LazyLock<HashMap<String, Cow<'static, [u8]>>> = LazyLock::new(|| {
    let mut icons = HashMap::new();
    for source in [
        &gpui_kit::assets::Assets as &dyn AssetSource,
        &ExtraIcons as &dyn AssetSource,
    ] {
        for path in source.list("").unwrap_or_default() {
            if let Ok(Some(bytes)) = source.load(&path) {
                icons.insert(path.to_string(), soften(bytes));
            }
        }
    }
    icons
});

#[derive(Clone, Copy, Debug, Default)]
pub struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui_kit::Result<Option<Cow<'static, [u8]>>> {
        if path == XYLO_MARK {
            return Ok(Some(Cow::Borrowed(XYLO_MARK_SVG)));
        }
        if let Some(bytes) = ICONS.get(path) {
            let bytes: &'static [u8] = bytes;
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        // Unknown paths keep the toolkit's own result (empty path or error).
        gpui_kit::assets::Assets.load(path)
    }
    fn list(&self, path: &str) -> gpui_kit::Result<Vec<gpui_kit::SharedString>> {
        let mut paths = gpui_kit::assets::Assets.list(path)?;
        paths.extend(ExtraIcons.list(path)?);
        if XYLO_MARK.starts_with(path) {
            paths.push(XYLO_MARK.into());
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

/// Serves `svg` with a soft root stroke, borrowing it when nothing changes.
fn soften(svg: Cow<'static, [u8]>) -> Cow<'static, [u8]> {
    match with_root_stroke_width(&svg, SOFT_STROKE_WIDTH) {
        Some(softened) => Cow::Owned(softened),
        None => svg,
    }
}

/// Rewrites the root `<svg>` element's `stroke-width` value. Returns `None`
/// when the document is not UTF-8, has no root `stroke-width`, or already uses
/// `width`, so child elements and stroke-less icons are never touched.
fn with_root_stroke_width(svg: &[u8], width: &str) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(svg).ok()?;
    let tag_start = root_tag_start(text)?;
    let value = attribute_value(&text[tag_start..], "stroke-width")?;
    let (start, end) = (tag_start + value.start, tag_start + value.end);
    if &text[start..end] == width {
        return None;
    }
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(width);
    out.push_str(&text[end..]);
    Some(out.into_bytes())
}

/// Byte offset of the root `<svg` start tag, skipping any XML prolog.
fn root_tag_start(text: &str) -> Option<usize> {
    text.match_indices("<svg")
        .map(|(index, _)| index)
        .find(|&index| {
            text[index + 4..]
                .chars()
                .next()
                .is_some_and(|next| next.is_whitespace() || next == '>' || next == '/')
        })
}

/// Byte range of `name`'s value within the start tag at the beginning of
/// `tag`. Attributes are tokenized in order, so a matching name inside another
/// attribute's value is never mistaken for the attribute itself.
fn attribute_value(tag: &str, name: &str) -> Option<std::ops::Range<usize>> {
    let bytes = tag.as_bytes();
    // Skip `<svg`.
    let mut at = 4;
    loop {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        match bytes.get(at)? {
            b'>' | b'/' => return None,
            _ => {}
        }
        let name_start = at;
        while bytes
            .get(at)
            .is_some_and(|&byte| !byte.is_ascii_whitespace() && !matches!(byte, b'=' | b'>' | b'/'))
        {
            at += 1;
        }
        let attribute = &tag[name_start..at];
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        if bytes.get(at) != Some(&b'=') {
            // A bare attribute without a value.
            continue;
        }
        at += 1;
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        let value = match *bytes.get(at)? {
            quote @ (b'"' | b'\'') => {
                let start = at + 1;
                let end = start + tag[start..].find(quote as char)?;
                at = end + 1;
                start..end
            }
            _ => {
                let start = at;
                while bytes
                    .get(at)
                    .is_some_and(|&byte| !byte.is_ascii_whitespace() && !matches!(byte, b'>'))
                {
                    at += 1;
                }
                start..at
            }
        };
        if attribute == name {
            return Some(value);
        }
    }
}

#[cfg(test)]
#[path = "assets/tests.rs"]
mod tests;
