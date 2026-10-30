use super::{
    check,
    package::Package,
    xml::{self, Element},
};
use anyhow::{Context, Result, bail, ensure};
use std::collections::{BTreeMap, HashSet};
use tokio_util::sync::CancellationToken;
use xylo_core::{DocumentContent, DocumentEdit, DocumentOpenRequest, DocumentParagraph};
const PART: &str = "word/document.xml";
fn paragraphs(root: &Element) -> Vec<(&Element, Option<String>)> {
    fn visit<'a>(
        node: &'a Element,
        inherited: Option<String>,
        output: &mut Vec<(&'a Element, Option<String>)>,
    ) {
        let reason = inherited.or_else(|| {
            match node.name.as_str() {
                "w:sdt" => Some("a content control"),
                "w:ins" | "w:del" => Some("tracked changes"),
                "w:drawing" | "w:pict" | "w:object" => Some("an image or embedded object"),
                "w:fldSimple" => Some("a field"),
                _ => None,
            }
            .map(|label| {
                format!("This paragraph belongs to {label}. Use your Office editor to preserve it.")
            })
        });
        if node.name == "w:p" {
            output.push((node, reason.clone()));
        }
        for child in &node.children {
            visit(child, reason.clone(), output);
        }
    }
    let mut result = Vec::new();
    visit(root, None, &mut result);
    result
}
fn content(p: &Element, xml: &str) -> Result<DocumentParagraph> {
    let mut text_nodes = Vec::new();
    p.descendants("w:t", &mut text_nodes);
    let mut text = String::new();
    for node in text_nodes {
        text.push_str(&node.text(xml)?);
    }
    let protected = [
        ("w:tab", "a tab or line break"),
        ("w:br", "a tab or line break"),
        ("w:cr", "a tab or line break"),
        ("w:fldChar", "a field"),
        ("w:fldSimple", "a field"),
        ("w:instrText", "a field"),
        ("w:del", "tracked changes"),
        ("w:ins", "tracked changes"),
        ("w:object", "an image or embedded object"),
        ("w:drawing", "an image or embedded object"),
        ("w:pict", "an image or embedded object"),
        ("w:sdt", "a content control"),
        ("w:footnoteReference", "a footnote or endnote reference"),
        ("w:endnoteReference", "a footnote or endnote reference"),
        ("w:sym", "a special symbol"),
        ("w:p", "nested paragraph content"),
    ];
    let reason = (text.len()>64*1024).then(||"This paragraph exceeds the native editing limit. Use your Office editor to preserve it.".to_owned()).or_else(||protected.iter().find_map(|(name, label)| {
        let mut nodes = Vec::new();
        p.descendants(name, &mut nodes);
        (!nodes.is_empty()).then(|| {
            format!("This paragraph contains {label}. Use your Office editor to preserve it.")
        })
    }));
    Ok(DocumentParagraph {
        index: 0,
        text,
        editable: reason.is_none(),
        reason,
    })
}
fn document(package: &mut Package, cancel: &CancellationToken) -> Result<(String, Element)> {
    let xml = package.text(PART, cancel)?;
    let root = xml::parse(&xml, cancel)?;
    ensure!(
        root.name == "w:document"
            && root.attr("xmlns:w").is_some_and(|v| v
                == "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
                || v == "http://purl.oclc.org/ooxml/wordprocessingml/main"),
        "Only standard WordprocessingML documents with the w namespace are editable"
    );
    Ok((xml, root))
}
pub fn open(
    package: &mut Package,
    args: &DocumentOpenRequest,
    payload_budget: usize,
    cancel: &CancellationToken,
) -> Result<DocumentContent> {
    let (xml, root) = document(package, cancel)?;
    let all = paragraphs(&root);
    ensure!(all.len() <= 100_000, "Document has too many paragraphs");
    let start = args.offset as usize;
    ensure!(start <= all.len(), "Paragraph page offset exceeds document");
    let mut page = Vec::new();
    let mut payload_bytes = 0usize;
    for (index, (p, inherited)) in all.iter().enumerate().skip(start).take(args.limit as usize) {
        check(cancel)?;
        let mut value = content(p, &xml)?;
        payload_bytes = payload_bytes.saturating_add(value.text.len());
        ensure!(
            payload_bytes <= payload_budget,
            "Document page exceeds response budget; reduce paragraph limit. Editable content was not truncated."
        );
        if let Some(reason) = inherited {
            value.editable = false;
            value.reason = Some(reason.clone());
        }
        value.index = index as u32;
        page.push(value);
    }
    Ok(DocumentContent::Docx {
        has_more: start + page.len() < all.len(),
        paragraphs: page,
        total_paragraphs: all.len() as u32,
        offset: args.offset,
    })
}
pub fn edit(
    package: &mut Package,
    edits: &[DocumentEdit],
    cancel: &CancellationToken,
) -> Result<BTreeMap<String, String>> {
    let (xml, root) = document(package, cancel)?;
    let all = paragraphs(&root);
    let mut seen = HashSet::new();
    let mut patches = Vec::new();
    for edit in edits {
        check(cancel)?;
        let DocumentEdit::Paragraph { index, text } = edit else {
            bail!("DOCX edits must target paragraphs")
        };
        ensure!(
            seen.insert(*index),
            "Coalesce repeated paragraph edits before saving"
        );
        ensure!(
            text.len() <= 64 * 1024 && !text.contains(['\n', '\r', '\t']),
            "Paragraph edits must be at most 64 KiB without line/tab changes"
        );
        let (p, inherited) = all
            .get(*index as usize)
            .context("Paragraph no longer exists")?;
        ensure!(
            inherited.is_none(),
            "{}",
            inherited.as_deref().unwrap_or("Protected paragraph")
        );
        let original = content(p, &xml)?;
        ensure!(original.editable, "{}", original.reason.unwrap_or_default());
        if original.text == *text {
            continue;
        }
        let mut nodes = Vec::new();
        p.descendants("w:t", &mut nodes);
        if nodes.is_empty() {
            let run = format!(
                "<w:r><w:t xml:space=\"preserve\">{}</w:t></w:r>",
                xml::escape(text)?
            );
            if xml[p.start..p.end].trim_end().ends_with("/>") {
                let opening = &xml[p.start..p.end - 2];
                patches.push((p.start, p.end, format!("{opening}>{run}</w:p>")));
            } else {
                patches.push((p.close_start, p.close_start, run));
            }
            continue;
        }
        let before: Vec<char> = original.text.chars().collect();
        let after: Vec<char> = text.chars().collect();
        let prefix = before
            .iter()
            .zip(&after)
            .take_while(|(a, b)| a == b)
            .count();
        let suffix = before[prefix..]
            .iter()
            .rev()
            .zip(after[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        let old_end = before.len() - suffix;
        let new_end = after.len() - suffix;
        let mut at = 0;
        let mut target = None;
        for node in nodes {
            let old = node.text(&xml)?;
            let count = old.chars().count();
            if prefix >= at && old_end <= at + count {
                target = Some((node, old, at));
                break;
            }
            at += count;
        }
        let (node,old,at)=target.context("This edit crosses styled runs or hyperlink boundaries. Edit within one run to preserve formatting.")?;
        let old: Vec<char> = old.chars().collect();
        let replacement = old[..prefix - at]
            .iter()
            .chain(after[prefix..new_end].iter())
            .chain(old[old_end - at..].iter())
            .collect::<String>();
        let mut attrs = node.attrs.clone();
        attrs.insert("xml:space".into(), "preserve".into());
        let mut tag = format!("<{}", node.name);
        for (name, value) in attrs {
            tag.push_str(&format!(" {name}=\"{}\"", xml::escape(&value)?));
        }
        tag.push('>');
        tag.push_str(&xml::escape(&replacement)?);
        tag.push_str(&format!("</{}>", node.name));
        patches.push((node.start, node.end, tag));
    }
    Ok(BTreeMap::from([(
        PART.into(),
        xml::patches(&xml, patches)?,
    )]))
}
