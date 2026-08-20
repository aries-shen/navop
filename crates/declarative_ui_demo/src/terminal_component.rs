use std::{collections::BTreeMap, rc::Rc};

use gpui::{AnyView, App};

use crate::{ComponentError, VNode};

/// A registry of type-erased, host-owned terminal views.
///
/// Provider markup can reference an already-approved session, but cannot create
/// a shell, command, working directory, or connection. That keeps every process
/// decision in trusted host code and capability enforcement.
#[derive(Default)]
pub(crate) struct TerminalSessionStore {
    sessions: BTreeMap<String, TerminalHandle>,
}

#[derive(Clone)]
struct TerminalHandle {
    view: AnyView,
    release: Rc<dyn Fn(&mut App)>,
}

impl TerminalSessionStore {
    pub(crate) fn view(&self, session: &str) -> Result<AnyView, ComponentError> {
        self.sessions
            .get(session)
            .map(|handle| handle.view.clone())
            .ok_or_else(|| unavailable(session))
    }

    /// Register a host-owned view under an already-approved session id.
    ///
    /// This is deliberately not reachable from component renderers. The future
    /// activation/capability manager will use the store owned by
    /// `DeclarativeView` to inject trusted terminal views.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn register(
        &mut self,
        session: impl Into<String>,
        view: AnyView,
        release: impl Fn(&mut App) + 'static,
    ) -> Result<(), ComponentError> {
        let session = session.into();
        if session.trim().is_empty() {
            return Err(ComponentError::new(
                "terminal session names must not be empty",
            ));
        }

        let handle = TerminalHandle {
            view,
            release: Rc::new(release),
        };

        // Replacing a handle may invalidate the earlier owner token. Reject the
        // replacement until the host explicitly removes that session.
        if self.sessions.insert(session.clone(), handle).is_some() {
            return Err(ComponentError::new(format!(
                "terminal session `{session}` is already registered"
            )));
        }
        Ok(())
    }

    pub(crate) fn remove(&mut self, session: &str, cx: Option<&mut App>) {
        let Some(handle) = self.sessions.remove(session) else {
            return;
        };
        if let Some(cx) = cx {
            (handle.release)(cx);
        }
    }

    pub(crate) fn retain_live(&mut self, root: &VNode, cx: &mut App) {
        let live = referenced_sessions(root);
        let stale = self
            .sessions
            .keys()
            .filter(|session| !live.contains(*session))
            .cloned()
            .collect::<Vec<_>>();
        for session in stale {
            self.remove(&session, Some(cx));
        }
    }

    pub(crate) fn shutdown(&mut self, cx: &mut App) {
        let sessions = std::mem::take(&mut self.sessions);
        for (_, handle) in sessions {
            (handle.release)(cx);
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, session: &str) -> bool {
        self.sessions.contains_key(session)
    }
}

pub(crate) fn unavailable(session: &str) -> ComponentError {
    ComponentError::new(format!(
        "terminal session `{session}` is not available; it must be created and approved by the host"
    ))
}

fn referenced_sessions(root: &VNode) -> std::collections::BTreeSet<String> {
    let mut sessions = std::collections::BTreeSet::new();
    collect_sessions(root, &mut sessions);
    sessions
}

fn collect_sessions(node: &VNode, sessions: &mut std::collections::BTreeSet<String>) {
    match node {
        VNode::Element(element) => {
            if element.tag.eq_ignore_ascii_case("terminal")
                && let Some(session) = element.attr("session")
            {
                sessions.insert(session.to_owned());
            }
            for child in &element.children {
                collect_sessions(child, sessions);
            }
        }
        VNode::Fragment(children) => {
            for child in children {
                collect_sessions(child, sessions);
            }
        }
        VNode::Text(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, rc::Rc};

    use gpui::{App, AppContext, Context, Render};

    use crate::{VElement, VNode};

    fn root(session: Option<&str>) -> VNode {
        let mut element = VElement {
            tag: "terminal".into(),
            attrs: Default::default(),
            classes: Default::default(),
            children: Vec::new(),
        };
        if let Some(session) = session {
            element.attrs.insert("session".into(), session.into());
        }
        VNode::Element(element)
    }

    #[derive(Default)]
    struct NoopView;

    impl Render for NoopView {
        fn render(
            &mut self,
            _window: &mut gpui::Window,
            _cx: &mut Context<Self>,
        ) -> impl gpui::IntoElement {
            gpui::div()
        }
    }

    #[gpui::test]
    fn terminal_sessions_are_host_owned_and_cleanup_is_explicit(cx: &mut gpui::TestAppContext) {
        cx.update(|cx: &mut App| {
            let mut store = super::TerminalSessionStore::default();
            assert!(store.view("missing").is_err());

            let released = Rc::new(Cell::new(false));
            let release = {
                let released = released.clone();
                move |_cx: &mut App| released.set(true)
            };
            let view = cx.new(|_| NoopView);
            store
                .register("host-approved", view.clone().into(), release)
                .expect("register terminal session");
            assert!(store.contains("host-approved"));
            assert_eq!(
                view.entity_id(),
                store.view("host-approved").expect("view").entity_id()
            );
            assert!(!released.get());

            store.retain_live(&root(Some("host-approved")), cx);
            assert!(store.contains("host-approved"));
            assert!(!released.get());

            store.retain_live(&root(None), cx);
            assert!(store.is_empty());
            assert!(released.get());

            let duplicate = cx.new(|_| NoopView);
            store
                .register("duplicate", duplicate.into(), |_cx: &mut App| {})
                .expect("register first duplicate");
            let second = cx.new(|_| NoopView);
            assert!(
                store
                    .register("duplicate", second.into(), |_cx: &mut App| {})
                    .is_err()
            );
            store.shutdown(cx);
            assert!(store.is_empty());
        });
    }
}
