use super::DocxExport;
use std::io::{Cursor, Read};

fn part(bytes: &[u8], name: &str) -> String {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
    let mut text = String::new();
    archive
        .by_name(name)
        .unwrap()
        .read_to_string(&mut text)
        .unwrap();
    text
}

fn well_formed(xml: &str) {
    let mut reader = quick_xml::Reader::from_str(xml);
    while !matches!(reader.read_event().unwrap(), quick_xml::events::Event::Eof) {}
}

#[test]
fn links_get_external_relationships_and_text_is_escaped() {
    let mut doc = DocxExport::default();
    doc.paragraph("first line\nsecond <line> & \"more\"");
    doc.link("https://example.com/?a=1&b=2", "Source <one>");
    let mut bytes = Cursor::new(Vec::new());
    doc.write(&mut bytes).unwrap();
    let bytes = bytes.into_inner();

    let document = part(&bytes, "word/document.xml");
    let relationships = part(&bytes, "word/_rels/document.xml.rels");
    for xml in [
        &document,
        &relationships,
        &part(&bytes, "[Content_Types].xml"),
        &part(&bytes, "_rels/.rels"),
    ] {
        well_formed(xml);
    }
    assert!(document.contains("first line</w:t><w:br/>"));
    assert!(document.contains("second &lt;line&gt; &amp; &quot;more&quot;"));
    assert!(document.contains("<w:hyperlink r:id=\"link1\">"));
    assert!(document.contains("Source &lt;one&gt;"));
    assert!(relationships.contains(
        r#"Id="link1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.com/?a=1&amp;b=2" TargetMode="External""#
    ));
}
