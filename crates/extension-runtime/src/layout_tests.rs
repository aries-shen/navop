use super::*;

fn panel(key: &str) -> LayoutNode {
    LayoutNode::Panel(NestedPanel {
        panel_key: key.into(),
        title: key.into(),
        icon: None,
    })
}

#[test]
fn registry_supports_named_pages_and_nested_layouts() {
    let mut registry = PageRegistry::default();
    registry
        .register(
            PageRoot::Named("cluster".into()),
            LayoutNode::Row {
                id: "main".into(),
                children: vec![
                    panel("nav"),
                    LayoutNode::Column {
                        id: "content".into(),
                        children: vec![panel("details")],
                    },
                ],
            },
        )
        .unwrap();
    assert_eq!(2, registry.panels().len());
    assert_eq!(1, registry.nodes(&PageRoot::Named("cluster".into())).len());
}

#[test]
fn registry_rejects_duplicate_panels_and_resource_exhaustion() {
    let mut registry = PageRegistry::default();
    registry
        .register(PageRoot::HomeTab, panel("duplicate"))
        .unwrap();
    assert!(matches!(
        registry.register(PageRoot::HomeSidebar, panel("duplicate")),
        Err(LayoutRegistryError::DuplicatePanel(_))
    ));

    let too_many = LayoutNode::Row {
        id: "wide".into(),
        children: (0..=MAX_LAYOUT_CHILDREN)
            .map(|index| panel(&format!("p{index}")))
            .collect(),
    };
    assert_eq!(
        Err(LayoutRegistryError::ChildCountExceeded),
        PageRegistry::default().register(PageRoot::HomeTab, too_many)
    );
}
