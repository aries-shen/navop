use std::collections::HashMap;

use gpui::{App, AppContext, Entity, Subscription, Window};
use gpui_component::highlighter::LanguageRegistry;
use gpui_component::input::{InputEvent, InputState};

use crate::{
    ComponentError, ComponentProps, NodePath, Runtime, VElement, VNode,
    component::stable_component_id,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CodeEditorSpec {
    pub(crate) language: String,
    pub(crate) placeholder: Option<String>,
    pub(crate) value: Option<String>,
    pub(crate) bind: Option<String>,
    pub(crate) line_numbers: bool,
    pub(crate) folding: bool,
}

impl CodeEditorSpec {
    pub(crate) fn from_element(element: &VElement) -> Result<Self, ComponentError> {
        let raw_language = element
            .attr("language")
            .map(str::trim)
            .filter(|language| !language.is_empty())
            .ok_or_else(|| ComponentError::new("<code-editor> requires `language`"))?;
        let language = LanguageRegistry::singleton()
            .resolve_language_name(raw_language)
            .ok_or_else(|| {
                ComponentError::new(format!(
                    "attribute `language` on <code-editor> is not a registered language, \
                     got `{raw_language}`"
                ))
            })?;

        Ok(Self {
            language,
            placeholder: element.attr("placeholder").map(str::to_owned),
            value: element.attr("value").map(str::to_owned),
            bind: element.attr("bind").map(str::to_owned),
            line_numbers: parse_bool_attribute(element, "line-numbers", true)?,
            folding: parse_bool_attribute(element, "folding", true)?,
        })
    }

    fn has_same_configuration(&self, next: &Self) -> bool {
        self.language == next.language
            && self.placeholder == next.placeholder
            && self.bind == next.bind
            && (self.bind.is_some() || self.value == next.value)
            && self.line_numbers == next.line_numbers
            && self.folding == next.folding
    }
}

pub(crate) struct CodeEditorEnvironment<'a> {
    pub(crate) window: &'a mut Window,
    pub(crate) cx: &'a mut App,
}

struct CodeEditorEntry {
    state: Entity<InputState>,
    spec: CodeEditorSpec,
    _subscription: Option<Subscription>,
}

#[derive(Default)]
pub(crate) struct CodeEditorCache {
    entries: HashMap<String, CodeEditorEntry>,
}

impl CodeEditorCache {
    pub(crate) fn resolve(
        &mut self,
        props: &ComponentProps,
        runtime: Entity<Runtime>,
        environment: CodeEditorEnvironment<'_>,
    ) -> Result<Entity<InputState>, ComponentError> {
        let spec = CodeEditorSpec::from_element(&props.element)?;
        let id = props.stable_id();
        if let Some(entry) = self.entries.get_mut(&id)
            && entry.spec.has_same_configuration(&spec)
        {
            entry.sync_bound_value(&spec, environment);
            return Ok(entry.state.clone());
        }

        let entry = CodeEditorEntry::new(spec, runtime, environment);
        let state = entry.state.clone();
        self.entries.insert(id, entry);
        Ok(state)
    }

    pub(crate) fn retain_live(&mut self, root: &VNode) {
        let live = code_editor_ids(root);
        self.entries.retain(|id, _| live.contains_key(id));
    }
}

impl CodeEditorEntry {
    fn new(
        spec: CodeEditorSpec,
        runtime: Entity<Runtime>,
        environment: CodeEditorEnvironment<'_>,
    ) -> Self {
        let placeholder = spec.placeholder.clone();
        let value = spec.value.clone();
        let line_numbers = spec.line_numbers;
        let folding = spec.folding;
        let language = spec.language.clone();
        let state = environment.cx.new(|cx| {
            let mut editor = InputState::new(environment.window, cx)
                .code_editor(language)
                .line_number(line_numbers)
                .folding(folding);
            if let Some(text) = placeholder {
                editor = editor.placeholder(text);
            }
            if let Some(text) = value {
                editor = editor.default_value(text);
            }
            editor
        });
        let subscription = spec
            .bind
            .clone()
            .map(|key| subscribe_binding(&state, runtime, key, environment.cx));
        Self {
            state,
            spec,
            _subscription: subscription,
        }
    }

    fn sync_bound_value(&mut self, next: &CodeEditorSpec, environment: CodeEditorEnvironment<'_>) {
        if next.bind.is_some() && self.spec.value != next.value {
            let value = next.value.clone().unwrap_or_default();
            self.state.update(environment.cx, |state, cx| {
                state.set_value(value, environment.window, cx);
            });
        }
        self.spec = next.clone();
    }
}

fn subscribe_binding(
    state: &Entity<InputState>,
    runtime: Entity<Runtime>,
    key: String,
    cx: &mut App,
) -> Subscription {
    cx.subscribe(state, move |input, event: &InputEvent, cx| {
        if !matches!(event, InputEvent::Change) {
            return;
        }
        let value = input.read(cx).value().to_string();
        let runtime = runtime.clone();
        let key = key.clone();
        cx.defer(move |cx| {
            runtime.update(cx, |runtime, cx| {
                runtime.set(key, value, cx);
            });
        });
    })
}

fn parse_bool_attribute(
    element: &VElement,
    name: &str,
    default: bool,
) -> Result<bool, ComponentError> {
    let Some(value) = element.attr(name) else {
        return Ok(default);
    };
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(ComponentError::new(format!(
            "attribute `{name}` on <{}> must be a boolean, got `{value}`",
            element.tag
        ))),
    }
}

fn code_editor_ids(root: &VNode) -> HashMap<String, ()> {
    let mut ids = HashMap::new();
    collect_code_editor_ids(root, &NodePath::root(), &mut ids);
    ids
}

fn collect_code_editor_ids(node: &VNode, path: &NodePath, ids: &mut HashMap<String, ()>) {
    match node {
        VNode::Element(element) => {
            if element.tag.eq_ignore_ascii_case("code-editor") {
                ids.insert(stable_component_id(element, path), ());
            }
            for (index, child) in element.children.iter().enumerate() {
                collect_code_editor_ids(child, &path.child(index), ids);
            }
        }
        VNode::Fragment(children) => {
            for (index, child) in children.iter().enumerate() {
                collect_code_editor_ids(child, &path.child(index), ids);
            }
        }
        VNode::Text(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::CodeEditorSpec;
    use crate::parse_html;

    #[test]
    fn resolves_language_aliases_through_the_host_registry() {
        let root = parse_html(r#"<code-editor language="rs" />"#).expect("valid editor");
        let element = root.element().expect("editor element");
        let spec = CodeEditorSpec::from_element(element).expect("registered language");

        assert_eq!("rust", spec.language);
    }

    #[test]
    fn unknown_languages_are_rejected() {
        let root =
            parse_html(r#"<code-editor language="not-a-language" />"#).expect("valid editor");
        let element = root.element().expect("editor element");
        let error = CodeEditorSpec::from_element(element)
            .expect_err("unregistered language must be rejected");

        assert!(error.to_string().contains("not-a-language"));
    }
}
