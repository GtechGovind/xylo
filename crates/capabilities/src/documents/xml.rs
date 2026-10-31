//! Byte-positioned XML scanning: untouched markup is copied verbatim.
use anyhow::{Context, Result, bail, ensure};
use quick_xml::{Reader, events::Event};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;
#[derive(Clone, Debug)]
pub struct Element {
    pub name: String,
    pub attrs: BTreeMap<String, String>,
    pub start: usize,
    pub open_end: usize,
    pub close_start: usize,
    pub end: usize,
    pub children: Vec<Element>,
}
impl Element {
    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).map(String::as_str)
    }
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> {
        self.children.iter().filter(move |child| child.name == name)
    }
    pub fn descendants<'a>(&'a self, name: &str, out: &mut Vec<&'a Element>) {
        for child in &self.children {
            if child.name == name {
                out.push(child);
            }
            child.descendants(name, out);
        }
    }
    pub fn text(&self, xml: &str) -> Result<String> {
        ensure!(self.children.is_empty(), "Unsupported nested text markup");
        let raw = &xml[self.open_end..self.close_start];
        ensure!(
            !raw.contains('<'),
            "CDATA/nested markup in editable text is unsupported"
        );
        Ok(quick_xml::escape::unescape(raw)?.into_owned())
    }
}
pub fn parse(xml: &str, cancel: &CancellationToken) -> Result<Element> {
    ensure!(
        xml.len() <= 32 * 1024 * 1024,
        "Office XML part exceeds 32 MiB"
    );
    let mut reader = Reader::from_str(xml);
    reader.config_mut().check_end_names = true;
    let mut stack: Vec<Element> = Vec::new();
    let mut roots = Vec::new();
    let mut nodes = 0usize;
    loop {
        ensure!(!cancel.is_cancelled(), "Document operation cancelled");
        let start = reader.buffer_position() as usize;
        let event = reader.read_event()?;
        let end = reader.buffer_position() as usize;
        match event {
            Event::Start(e) | Event::Empty(e) => {
                nodes += 1;
                ensure!(
                    nodes <= 250_000 && stack.len() < 128,
                    "Office XML exceeds element/depth budget"
                );
                let empty = xml[start..end].trim_end().ends_with("/>");
                let mut attrs = BTreeMap::new();
                for attr in e.attributes() {
                    let attr = attr?;
                    let key = attr.key.as_ref().to_owned();
                    ensure!(
                        attrs
                            .insert(
                                key,
                                attr.normalized_value(quick_xml::XmlVersion::Implicit1_0)?
                                    .into_owned()
                            )
                            .is_none(),
                        "Duplicate XML attribute"
                    );
                }
                let node = Element {
                    name: e.name().as_ref().to_owned(),
                    attrs,
                    start,
                    open_end: end,
                    close_start: end,
                    end,
                    children: Vec::new(),
                };
                if empty {
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(node);
                    } else {
                        roots.push(node);
                    }
                } else {
                    stack.push(node);
                }
            }
            Event::End(_) => {
                let mut node = stack.pop().context("Unexpected XML end")?;
                node.close_start = start;
                node.end = end;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(node);
                } else {
                    roots.push(node);
                }
            }
            Event::DocType(_) => bail!("Document type/entity declarations are not supported"),
            Event::Eof => break,
            _ => {}
        }
    }
    ensure!(
        stack.is_empty() && roots.len() == 1,
        "Office XML must have one complete root"
    );
    Ok(roots.remove(0))
}
pub fn escape(value: &str) -> Result<String> {
    ensure!(
        value.chars().all(|c| matches!(c, '\t' | '\r' | '\n')
            || c >= ' ' && !matches!(c, '\u{fffe}' | '\u{ffff}')),
        "Text contains unsupported XML control characters"
    );
    Ok(quick_xml::escape::escape(value).into_owned())
}
pub fn patches(xml: &str, mut replacements: Vec<(usize, usize, String)>) -> Result<String> {
    replacements.sort_by_key(|v| (v.0, v.1));
    let mut output = String::new();
    let mut at = 0;
    for (start, end, text) in replacements {
        ensure!(
            start >= at && end >= start && end <= xml.len(),
            "Overlapping document patches are unsupported"
        );
        output.push_str(&xml[at..start]);
        output.push_str(&text);
        at = end;
        ensure!(
            output.len() <= 32 * 1024 * 1024,
            "Edited Office part exceeds 32 MiB"
        );
    }
    output.push_str(&xml[at..]);
    ensure!(
        output.len() <= 32 * 1024 * 1024,
        "Edited Office part exceeds 32 MiB"
    );
    Ok(output)
}

#[cfg(test)]
mod tests;
