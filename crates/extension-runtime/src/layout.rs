use std::collections::BTreeMap;

use crate::{RegisteredDeclarativePanel, extension::manifest::DeclarativePanelPlacement};

pub const MAX_LAYOUT_DEPTH: usize = 16;
pub const MAX_LAYOUT_NODES: usize = 256;
pub const MAX_LAYOUT_CHILDREN: usize = 32;

/// A host page that can receive contributed layout trees.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum PageRoot {
    HomeSidebar,
    HomeTab,
    Named(String),
}

/// Safe panel metadata exposed to layout composition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NestedPanel {
    pub panel_key: String,
    pub title: String,
    pub icon: Option<String>,
}

/// A declarative, host-owned layout tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutNode {
    Panel(NestedPanel),
    Stack { id: String, children: Vec<Self> },
    Row { id: String, children: Vec<Self> },
    Column { id: String, children: Vec<Self> },
}

impl LayoutNode {
    fn children(&self) -> &[Self] {
        match self {
            Self::Panel(_) => &[],
            Self::Stack { children, .. }
            | Self::Row { children, .. }
            | Self::Column { children, .. } => children,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutRegistryError {
    #[error("layout exceeds maximum depth {MAX_LAYOUT_DEPTH}")]
    DepthExceeded,
    #[error("layout exceeds maximum node count {MAX_LAYOUT_NODES}")]
    NodeCountExceeded,
    #[error("layout node exceeds maximum child count {MAX_LAYOUT_CHILDREN}")]
    ChildCountExceeded,
    #[error("panel `{0}` is already registered")]
    DuplicatePanel(String),
}

/// Registry of page roots and nested panel metadata.
#[derive(Debug, Default)]
pub struct PageRegistry {
    roots: BTreeMap<PageRoot, Vec<LayoutNode>>,
    panels: BTreeMap<String, NestedPanel>,
}

impl PageRegistry {
    pub fn roots(&self) -> &BTreeMap<PageRoot, Vec<LayoutNode>> {
        &self.roots
    }

    pub fn panels(&self) -> &BTreeMap<String, NestedPanel> {
        &self.panels
    }

    pub fn nodes(&self, root: &PageRoot) -> &[LayoutNode] {
        self.roots.get(root).map(Vec::as_slice).unwrap_or_default()
    }

    pub fn register(
        &mut self,
        root: PageRoot,
        node: LayoutNode,
    ) -> Result<(), LayoutRegistryError> {
        validate_layout(&node)?;
        self.index_panels(&node)?;
        self.roots.entry(root).or_default().push(node);
        Ok(())
    }

    pub(crate) fn register_legacy_panel(
        &mut self,
        panel: &RegisteredDeclarativePanel,
    ) -> Result<(), LayoutRegistryError> {
        let root = match panel.placement {
            DeclarativePanelPlacement::HomeSidebar => PageRoot::HomeSidebar,
            DeclarativePanelPlacement::HomeTab => PageRoot::HomeTab,
        };
        self.register(
            root,
            LayoutNode::Panel(NestedPanel {
                panel_key: panel.panel_key.clone(),
                title: panel.title.clone(),
                icon: panel.icon.clone(),
            }),
        )
    }

    fn index_panels(&mut self, node: &LayoutNode) -> Result<(), LayoutRegistryError> {
        if let LayoutNode::Panel(panel) = node {
            if self.panels.contains_key(&panel.panel_key) {
                return Err(LayoutRegistryError::DuplicatePanel(panel.panel_key.clone()));
            }
            self.panels.insert(panel.panel_key.clone(), panel.clone());
        }
        for child in node.children() {
            self.index_panels(child)?;
        }
        Ok(())
    }
}

fn validate_layout(root: &LayoutNode) -> Result<(), LayoutRegistryError> {
    let mut stack = vec![(root, 1_usize)];
    let mut count = 0_usize;
    while let Some((node, depth)) = stack.pop() {
        count += 1;
        if depth > MAX_LAYOUT_DEPTH {
            return Err(LayoutRegistryError::DepthExceeded);
        }
        if count > MAX_LAYOUT_NODES {
            return Err(LayoutRegistryError::NodeCountExceeded);
        }
        if node.children().len() > MAX_LAYOUT_CHILDREN {
            return Err(LayoutRegistryError::ChildCountExceeded);
        }
        stack.extend(node.children().iter().map(|child| (child, depth + 1)));
    }
    Ok(())
}
