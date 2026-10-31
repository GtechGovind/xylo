use super::*;
use serde_json::json;

#[test]
fn a_schema_compiles_once_per_name_and_is_shared_with_children() {
    let parent = Lookup::default();
    let schema = || json!({"type":"object","properties":{"path":{"type":"string"}}});
    let first = parent.schemas.get("file_read", schema);
    let again = parent.schemas.get("file_read", || unreachable!("cached"));
    assert!(Arc::ptr_eq(&first, &again));
    let child = parent.child();
    assert!(Arc::ptr_eq(
        &first,
        &child.schemas.get("file_read", || unreachable!("shared"))
    ));
    let validator = first.validator().unwrap();
    assert!(std::ptr::eq(validator, first.validator().unwrap()));
    assert!(validator.is_valid(&json!({"path":"a"})));
}

#[test]
fn a_schema_that_does_not_compile_is_remembered_as_unenforced() {
    let broken = CompiledSchema::new(json!({"type":"no-such-type"}));
    assert!(broken.validator().is_none());
    assert!(broken.validator().is_none());
}

#[test]
fn rediscovery_forgets_the_connector_snapshot_and_the_index() {
    let lookup = Lookup::default();
    *lookup.connectors.lock() = Some(Arc::new(Vec::new()));
    *lookup.search.lock() = Some(Arc::new(SearchIndex::new(
        SearchKey {
            builtin: Vec::new(),
            writes: true,
        },
        Vec::new(),
        Vec::new(),
    )));
    let child = lookup.child();
    assert!(child.connectors.lock().is_none() && child.search.lock().is_none());
    lookup.invalidate_connectors();
    assert!(lookup.connectors.lock().is_none() && lookup.search.lock().is_none());
}
