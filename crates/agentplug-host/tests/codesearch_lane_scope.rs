use agentplug_host::{cost_class_for_dispatch, DispatchCostClass};

fn cost_class(body: &str) -> DispatchCostClass {
    cost_class_for_dispatch("codesearch", body)
}

#[test]
fn codesearch_scoped_to_named_files_is_cheap() {
    assert_eq!(
        cost_class(r#"{"glob":"AGENTS.md","query":"creationSnapPending"}"#),
        DispatchCostClass::Cheap,
        "a scope naming one file reads that file, so it must not queue on the store lane"
    );
    assert_eq!(
        cost_class(r#"{"path":"AGENTS.md","query":"creationSnapPending"}"#),
        DispatchCostClass::Cheap,
        "path scope naming one file must not queue on the store lane"
    );
    assert_eq!(
        cost_class(r#"{"path_glob":"AGENTS.md","query":"creationSnapPending"}"#),
        DispatchCostClass::Cheap,
        "path_glob naming one file must not queue on the store lane"
    );
    assert_eq!(
        cost_class(r#"{"include":"AGENTS.md","query":"creationSnapPending"}"#),
        DispatchCostClass::Cheap,
        "include naming one file must not queue on the store lane"
    );
    assert_eq!(
        cost_class(r#"{"paths":["AGENTS.md","src/main.cpp"],"query":"creationSnapPending"}"#),
        DispatchCostClass::Cheap,
        "a scope naming only files must not queue on the store lane"
    );
}

#[test]
fn codesearch_over_a_subtree_keeps_the_store_lane() {
    assert_eq!(
        cost_class(r#"{"glob":"**/*.md","query":"creationSnapPending"}"#),
        DispatchCostClass::Heavy,
        "a wildcard scope still walks and embeds a subtree, so it keeps the store lane"
    );
    assert_eq!(
        cost_class(r#"{"paths":["src/**"],"query":"anything"}"#),
        DispatchCostClass::Heavy,
        "a wildcard path scope keeps the store lane"
    );
    assert_eq!(
        cost_class(r#"{"query":"semantic search"}"#),
        DispatchCostClass::Heavy,
        "an unscoped default codesearch can rebuild and embed, so it keeps the store lane"
    );
}

#[test]
fn explicit_codesearch_modes_are_unchanged() {
    assert_eq!(
        cost_class(r#"{"mode":"literal","query":"creationSnapPending"}"#),
        DispatchCostClass::Cheap
    );
    assert_eq!(
        cost_class(r#"{"mode":"regex","query":"creation[A-Za-z]*"}"#),
        DispatchCostClass::Cheap
    );
}
