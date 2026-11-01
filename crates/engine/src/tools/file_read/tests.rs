use super::*;
#[test]
fn pdf_page_ranges_are_bounded_and_one_based() {
    assert_eq!(pages(&json!({"pages":"1-5"})).unwrap(), (1, 5));
    assert_eq!(pages(&json!({"pages":"20"})).unwrap(), (20, 1));
    for range in ["0-5", "5-1", "1-21", "x", "1-2-3"] {
        assert!(pages(&json!({"pages":range})).is_err());
    }
}

#[test]
fn pdf_range_is_forwarded_to_the_document_capability() {
    assert_eq!(
        pdf_args(Path::new("book.pdf"), &json!({"pages":"3-7"})).unwrap(),
        json!({"path":"book.pdf","page":3,"max_pages":5})
    );
    assert_eq!(pages(&json!({"pages":"1-20"})).unwrap(), (1, 20));
}
