//! Writes a new Word document from plain paragraphs and source links.
//!
//! The package holds only the four parts Word requires: content types, the
//! package relationships, the main document and its relationships (one per
//! external link). Text is XML-escaped and keeps its whitespace; a line break
//! inside a paragraph becomes a Word line break.
use anyhow::Result;
use quick_xml::escape::escape;
use std::io::{Seek, Write};
use zip::{CompressionMethod, ZipWriter, write::SimpleFileOptions};

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>"#;

const PACKAGE_RELATIONSHIPS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#;

const HYPERLINK: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink";

/// A document body: text paragraphs, then one linked paragraph per source.
#[derive(Default)]
pub(super) struct DocxExport {
    body: String,
    links: Vec<String>,
}

impl DocxExport {
    /// Adds a paragraph of plain text.
    pub(super) fn paragraph(&mut self, text: &str) {
        self.body.push_str("<w:p><w:r>");
        push_text(&mut self.body, text);
        self.body.push_str("</w:r></w:p>");
    }

    /// Adds a paragraph holding `title` linked to the external `url`.
    pub(super) fn link(&mut self, url: &str, title: &str) {
        self.links.push(url.to_owned());
        let id = self.links.len();
        self.body.push_str(&format!(
            "<w:p><w:hyperlink r:id=\"link{id}\"><w:r><w:rPr><w:rStyle w:val=\"Hyperlink\"/><w:color w:val=\"0563C1\"/><w:u w:val=\"single\"/></w:rPr>"
        ));
        push_text(&mut self.body, title);
        self.body.push_str("</w:r></w:hyperlink></w:p>");
    }

    /// Writes the complete package.
    pub(super) fn write(&self, output: impl Write + Seek) -> Result<()> {
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        let mut zip = ZipWriter::new(output);
        for (name, contents) in [
            ("[Content_Types].xml", CONTENT_TYPES.to_owned()),
            ("_rels/.rels", PACKAGE_RELATIONSHIPS.to_owned()),
            ("word/document.xml", self.document()),
            ("word/_rels/document.xml.rels", self.relationships()),
        ] {
            zip.start_file(name, options)?;
            zip.write_all(contents.as_bytes())?;
        }
        zip.finish()?;
        Ok(())
    }

    fn document(&self) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><w:body>{}<w:sectPr/></w:body></w:document>"#,
            self.body
        )
    }

    fn relationships(&self) -> String {
        let mut xml = String::from(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
        );
        for (index, url) in self.links.iter().enumerate() {
            xml.push_str(&format!(
                r#"<Relationship Id="link{}" Type="{HYPERLINK}" Target="{}" TargetMode="External"/>"#,
                index + 1,
                escape(url.as_str())
            ));
        }
        xml.push_str("</Relationships>");
        xml
    }
}

/// Appends `text` as run content: escaped, whitespace kept, line breaks as
/// `<w:br/>`.
fn push_text(xml: &mut String, text: &str) {
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            xml.push_str("<w:br/>");
        }
        xml.push_str("<w:t xml:space=\"preserve\">");
        xml.push_str(&escape(line.trim_end_matches('\r')));
        xml.push_str("</w:t>");
    }
}

#[cfg(test)]
mod tests;
