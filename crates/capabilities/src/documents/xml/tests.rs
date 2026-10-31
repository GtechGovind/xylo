use super::*;

#[test]
fn text_and_attributes_resolve_predefined_and_numeric_entities() {
    let xml = r#"<r a="x&amp;&#47;y"><t>A &amp; B &lt;C&gt; &#65;&#x42; &quot;q&apos;</t></r>"#;
    let root = parse(xml, &CancellationToken::new()).unwrap();
    assert_eq!(root.attr("a"), Some("x&/y"));
    assert_eq!(root.children[0].text(xml).unwrap(), "A & B <C> AB \"q'");
    let round_trip = escape("A & B <C>").unwrap();
    assert_eq!(round_trip, "A &amp; B &lt;C&gt;");
}

#[test]
fn undeclared_entities_and_document_types_are_refused() {
    let undeclared = "<r><t>&custom;</t></r>";
    let root = parse(undeclared, &CancellationToken::new()).unwrap();
    assert!(root.children[0].text(undeclared).is_err());
    assert!(
        parse(
            r#"<!DOCTYPE r [<!ENTITY e "x">]><r>&e;</r>"#,
            &CancellationToken::new()
        )
        .is_err()
    );
}
