use declarative_ui_demo::{
    ActionEvent, CompileOptions, CompiledTemplate, ComponentRegistry, DeclarativeView,
    DeclarativeViewConfig, Runtime, RuntimeEvent, StateStore, VNode, compile_template_with_style,
};
use extension_plugin_adapter::{
    ActivationHandle, DeclarativePanelSource, EventStreamBatch, UiStatePatch, apply_ui_state_patch,
    ui_action_request,
};
use extension_protocol::declarative_ui::{UiEventSubscriptionOperation, validate_ui_state_patch};
use gpui::{
    App, AppContext, AsyncApp, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString, Styled, Task, Window,
    div,
};
use gpui_component::Icon;
use one_core::gpui_tokio::Tokio;
use one_core::tab_container::{TabContent, TabContentEvent};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

const PROVIDER_ACTION_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_SUBSCRIPTION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

mod events;
use events::PanelEventSubscription;

#[derive(Debug)]
pub(crate) enum UniversalPluginPanelError {
    Compile(String),
}

impl std::fmt::Display for UniversalPluginPanelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Compile(message) => write!(f, "{message}"),
        }
    }
}

/// A host-owned wrapper around the trusted declarative renderer.
///
/// The source arrives as validated text and contains no provider process
/// handles, activation permissions, or filesystem paths.
pub(crate) struct UniversalPluginPanel {
    view: Entity<DeclarativeView>,
    runtime: Entity<Runtime>,
    focus_handle: FocusHandle,
    title: SharedString,
    icon: Option<SharedString>,
    service: crate::universal_plugins::UniversalPluginService,
    activation: ActivationHandle,
    runtime_id: String,
    runtime_generation: u64,
    pending_actions: BTreeMap<String, PendingProviderAction>,
    next_request_sequence: u64,
    action_error: Option<String>,
    provider_action_timeout: Duration,
    event_subscriptions: BTreeMap<String, PanelEventSubscription>,
    next_event_subscription_epoch: u64,
    _runtime_subscription: gpui::Subscription,
    _health_task: Task<()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingProviderAction {
    request_id: String,
    expected_revision: u64,
}

impl UniversalPluginPanel {
    pub(crate) fn compile(
        source: DeclarativePanelSource,
    ) -> Result<CompiledTemplate, UniversalPluginPanelError> {
        compile_template_with_style(
            &source.template,
            source.style.as_deref(),
            &ComponentRegistry::with_defaults(),
            CompileOptions::strict(),
        )
        .map_err(|error| UniversalPluginPanelError::Compile(error.to_string()))
    }

    pub(crate) fn new(
        template: CompiledTemplate,
        service: crate::universal_plugins::UniversalPluginService,
        activation: ActivationHandle,
        title: SharedString,
        icon: Option<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let registry = ComponentRegistry::with_defaults();
        let runtime = cx.new(|_| Runtime::new(StateStore::default()));
        let action_names = template_action_names(template.root());
        runtime
            .update(cx, |runtime, _| {
                for action in action_names {
                    runtime.on(action, |_| Ok(()))?;
                }
                Ok::<_, declarative_ui_demo::RuntimeError>(())
            })
            .expect("template action names are unique");

        let runtime_for_view = runtime.clone();
        let runtime_for_panel = runtime.clone();
        let view = cx.new(|cx| {
            DeclarativeView::new(
                DeclarativeViewConfig::new(template, runtime_for_view, registry),
                cx,
            )
        });
        let runtime_subscription = cx.subscribe(&runtime, |panel, _, event: &RuntimeEvent, cx| {
            panel.handle_runtime_event(event, cx);
        });
        let mut health_events = service.subscribe();
        let panel_runtime_id = activation.runtime_id.clone();
        let health_task = cx.spawn(async move |this, cx: &mut AsyncApp| {
            loop {
                let event = match health_events.recv().await {
                    Ok(event) => event,
                    Err(
                        tokio::sync::broadcast::error::RecvError::Lagged(_)
                        | tokio::sync::broadcast::error::RecvError::Closed,
                    ) => break,
                };
                let event_runtime_id = match &event {
                    extension_plugin_adapter::RuntimeMonitorEvent::HealthChanged {
                        runtime_id,
                        ..
                    }
                    | extension_plugin_adapter::RuntimeMonitorEvent::RuntimeRemoved {
                        runtime_id,
                    }
                    | extension_plugin_adapter::RuntimeMonitorEvent::CheckFailed {
                        runtime_id,
                        ..
                    } => runtime_id.clone(),
                };
                if event_runtime_id != panel_runtime_id {
                    continue;
                }
                let updated = this.update(cx, |panel, cx| {
                    panel.handle_runtime_monitor_event(&event, cx);
                });
                if updated.is_err() {
                    break;
                }
            }
        });

        Self {
            view,
            runtime: runtime_for_panel,
            focus_handle: cx.focus_handle(),
            title,
            icon,
            service,
            runtime_id: activation.runtime_id.clone(),
            runtime_generation: activation.runtime_generation,
            activation,
            pending_actions: BTreeMap::new(),
            next_request_sequence: 0,
            action_error: None,
            provider_action_timeout: PROVIDER_ACTION_TIMEOUT,
            event_subscriptions: BTreeMap::new(),
            next_event_subscription_epoch: 0,
            _runtime_subscription: runtime_subscription,
            _health_task: health_task,
        }
    }

    fn refresh_runtime_generation(&mut self, cx: &mut Context<Self>) {
        if !self.pending_actions.is_empty() {
            return;
        }
        if let Ok(generation) = self.service.runtime_generation(&self.runtime_id)
            && generation > self.runtime_generation
        {
            self.runtime_generation = generation;
            cx.notify();
        }
    }

    fn handle_runtime_monitor_event(
        &mut self,
        event: &extension_plugin_adapter::RuntimeMonitorEvent,
        cx: &mut Context<Self>,
    ) {
        let extension_plugin_adapter::RuntimeMonitorEvent::HealthChanged { runtime_id, health } =
            event
        else {
            return;
        };
        if runtime_id != &self.runtime_id
            || health.state != extension_plugin_adapter::RuntimeActivationState::Active
            || !self.pending_actions.is_empty()
        {
            return;
        }

        if let Ok(generation) = self.service.runtime_generation(&self.runtime_id)
            && generation > self.runtime_generation
        {
            self.runtime_generation = generation;
            self.action_error = None;
            cx.notify();
        }
    }

    fn handle_runtime_event(&mut self, event: &RuntimeEvent, cx: &mut Context<Self>) {
        match event {
            RuntimeEvent::ActionCompleted { event, outcome } => {
                self.start_provider_action(event, outcome.revision, cx);
            }
            RuntimeEvent::StateChanged(_) | RuntimeEvent::ActionFailed { .. } => {}
        }
    }

    fn start_provider_action(
        &mut self,
        event: &ActionEvent,
        expected_revision: u64,
        cx: &mut Context<Self>,
    ) {
        let action = event.name().to_owned();
        if !self.pending_actions.is_empty() {
            self.action_error = Some(format!(
                "another provider action is already running; wait for it to finish before dispatching `{action}`"
            ));
            cx.notify();
            return;
        }

        let managed_client = match self.service.universal_plugin_client(&self.runtime_id) {
            Ok(client) => client,
            Err(error) => {
                self.action_error = Some(format!("Failed to dispatch action `{action}`: {error}"));
                cx.notify();
                return;
            }
        };
        if managed_client.generation > self.runtime_generation {
            // An action is itself evidence that the panel is still mounted and
            // no older request is pending. Adopt the newer client immediately.
            self.runtime_generation = managed_client.generation;
        }
        if managed_client.generation != self.runtime_generation {
            self.action_error = Some(format!(
                "Action `{action}` was not sent because its provider runtime was replaced"
            ));
            cx.notify();
            return;
        }

        self.next_request_sequence = self.next_request_sequence.saturating_add(1);
        let request_id = format!(
            "universal-panel:{}:{}",
            self.runtime_generation, self.next_request_sequence
        );
        let request = ui_action_request(event, request_id.clone(), Some(expected_revision));
        self.pending_actions.insert(
            action.clone(),
            PendingProviderAction {
                request_id: request_id.clone(),
                expected_revision,
            },
        );
        self.action_error = None;
        cx.notify();

        let client = managed_client.client().clone();
        let action_timeout = self.provider_action_timeout;
        let action_task = Tokio::spawn(cx, async move {
            let result = tokio::time::timeout(action_timeout, client.ui_action(&request)).await;
            result
        });
        let started_generation = managed_client.generation;
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = action_task
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|timeout| timeout.to_string()))
                .and_then(|result| result.map_err(|error| error.to_string()));
            let _ = this.update(cx, |panel, cx| {
                panel.finish_provider_action(action, request_id, started_generation, result, cx);
            });
        })
        .detach();
    }

    fn finish_provider_action(
        &mut self,
        action: String,
        request_id: String,
        started_generation: u64,
        result: Result<UiStatePatch, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.pending_actions.remove(&action) else {
            return;
        };
        if pending.request_id != request_id {
            self.pending_actions.insert(action.clone(), pending);
            self.action_error = Some(format!(
                "Action `{action}` completed with an unexpected request id"
            ));
            cx.notify();
            return;
        }

        let runtime_was_replaced =
            self.service.runtime_generation(&self.runtime_id) != Ok(started_generation);
        if self.runtime_generation != started_generation || runtime_was_replaced {
            self.action_error = Some(format!(
                "Action `{action}` failed because its provider runtime was replaced"
            ));
            self.refresh_runtime_generation(cx);
            cx.notify();
            return;
        }
        match result {
            Ok(patch) => {
                if let Err(error) = validate_ui_state_patch(&patch) {
                    self.action_error = Some(format!(
                        "Action `{action}` returned an invalid event subscription: {error}"
                    ));
                    cx.notify();
                    return;
                }
                let patch_result = self
                    .runtime
                    .update(cx, |runtime, cx| apply_ui_state_patch(runtime, &patch, cx))
                    .map_err(|error| error.to_string());
                match patch_result {
                    Ok(_) => {
                        self.action_error = self
                            .reconcile_event_subscriptions(&patch.event_subscriptions, cx)
                            .err();
                    }
                    Err(error) => {
                        self.action_error =
                            Some(format!("Action `{action}` failed to update state: {error}"));
                    }
                }
            }
            Err(error) => {
                self.action_error = Some(format!("Action `{action}` failed: {error}"));
            }
        }
        cx.notify();
    }

    fn reconcile_event_subscriptions(
        &mut self,
        operations: &[UiEventSubscriptionOperation],
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        for operation in operations {
            match operation {
                UiEventSubscriptionOperation::Subscribe { .. } => {
                    events::subscribe(self, operation, cx)?;
                }
                UiEventSubscriptionOperation::Unsubscribe { subscription_id } => {
                    self.cancel_event_subscription(subscription_id);
                }
            }
        }
        Ok(())
    }

    fn cancel_event_subscription(&mut self, subscription_id: &str) {
        if let Some(subscription) = self.event_subscriptions.remove(subscription_id) {
            subscription.cancel().detach();
        }
    }

    fn allocate_event_subscription_epoch(&mut self) -> Result<u64, String> {
        let epoch = self.next_event_subscription_epoch;
        self.next_event_subscription_epoch = epoch
            .checked_add(1)
            .ok_or_else(|| "event subscription epoch exhausted".to_owned())?;
        Ok(epoch)
    }

    fn apply_event_result(
        &mut self,
        subscription_id: &str,
        epoch: u64,
        state_key: &str,
        generation: u64,
        result: Result<EventStreamBatch, String>,
        cx: &mut Context<Self>,
    ) {
        let is_current_subscription = self
            .event_subscriptions
            .get(subscription_id)
            .is_some_and(|subscription| subscription.epoch == epoch);
        if !is_current_subscription
            || self.runtime_generation != generation
            || self.service.runtime_generation(&self.runtime_id) != Ok(generation)
        {
            return;
        }
        match result {
            Ok(batch) => {
                let operation = events::state_operation(state_key, events::event_state(batch));
                if let Err(error) = self.runtime.update(cx, |runtime, cx| {
                    runtime.apply_external_patch(None, &[operation], cx)
                }) {
                    self.action_error = Some(error.to_string());
                }
            }
            Err(error) => self.action_error = Some(format!("Event stream failed: {error}")),
        }
        cx.notify();
    }
}

impl Render for UniversalPluginPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let mut content = div()
            .id("universal-plugin-panel")
            .size_full()
            .child(self.view.clone());
        if !self.pending_actions.is_empty() {
            content = content.child(div().child(format!(
                "Provider action running ({} pending)",
                self.pending_actions.len()
            )));
        }
        if let Some(error) = self.action_error.clone() {
            content = content.child(div().child(error));
        }
        content
    }
}

impl Focusable for UniversalPluginPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<TabContentEvent> for UniversalPluginPanel {}

impl TabContent for UniversalPluginPanel {
    fn content_key(&self) -> &'static str {
        "UniversalPluginPanel"
    }

    fn title(&self, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn icon(&self, _cx: &App) -> Option<Icon> {
        self.icon
            .clone()
            .map(|path| Icon::default().path(path).color())
    }

    fn closeable(&self, _cx: &App) -> bool {
        true
    }

    fn can_rename(&self, _cx: &App) -> bool {
        false
    }

    fn try_close(
        &mut self,
        _tab_id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        // Terminal views are host-owned resources. Provider markup cannot
        // request their release, so release them before this tab is dropped.
        self.view
            .update(cx, |view, cx| view.shutdown_terminal_sessions(cx));
        let subscriptions = std::mem::take(&mut self.event_subscriptions)
            .into_values()
            .map(PanelEventSubscription::cancel)
            .collect::<Vec<_>>();

        let service = self.service.clone();
        let activation = self.activation.clone();
        let (events_closed_tx, events_closed_rx) = tokio::sync::oneshot::channel();
        if subscriptions.is_empty() {
            let _ = events_closed_tx.send(());
        } else {
            cx.spawn(async move |_, _| {
                for subscription in subscriptions {
                    let _ = subscription.await;
                }
                let _ = events_closed_tx.send(());
            })
            .detach();
        }
        // Provider shutdown negotiates over Tokio-backed transports. Keep that
        // work on the Tokio runtime. Event streams get a bounded grace period
        // to send event/close before the owning provider activation is retired.
        Tokio::spawn(cx, async move {
            let _ =
                tokio::time::timeout(EVENT_SUBSCRIPTION_SHUTDOWN_TIMEOUT, events_closed_rx).await;
            if let Err(error) = service.deactivate_activation(&activation).await {
                tracing::warn!(
                    panel = activation.panel_key,
                    %error,
                    "failed to deactivate universal plugin panel"
                );
            }
        })
        .detach();

        Task::ready(true)
    }
}

fn template_action_names(root: &VNode) -> Vec<String> {
    let mut actions = BTreeSet::new();
    visit_action_names(root, &mut actions);
    actions.into_iter().collect()
}

fn visit_action_names(node: &VNode, actions: &mut BTreeSet<String>) {
    match node {
        VNode::Element(element) => {
            if let Some(action) = element.attr("action").map(str::trim)
                && !action.is_empty()
            {
                actions.insert(action.to_owned());
            }
            for child in &element.children {
                visit_action_names(child, actions);
            }
        }
        VNode::Fragment(children) => {
            for child in children {
                visit_action_names(child, actions);
            }
        }
        VNode::Text(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension_host::{
        HostApiHandler, JsonRpcClient, NegotiationConfig, ProcessRpcSession,
        ProcessRpcSessionConfig, SpawnConfig,
        transport::{FramedTransport, recv_async, send_async},
    };
    use extension_protocol::{
        declarative_ui::UiActionRequest,
        envelope::{Response, RpcMessage},
        lifecycle::InitResult,
        method,
    };
    use extension_runtime::extension::manifest::load_from_dir;
    use futures::future::BoxFuture;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use std::time::Instant;
    use tokio::{io::duplex, sync::mpsc};

    use crate::universal_plugins::UniversalPluginService;

    struct PanelTestRoot(gpui::Entity<UniversalPluginPanel>);

    impl Render for PanelTestRoot {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.0.clone())
        }
    }

    #[test]
    fn template_action_names_are_unique_and_recursive() {
        let template = compile_template_with_style(
            r#"<div><button action="search">Search</button><div><button action="search">Again</button><button action="reset">Reset</button></div></div>"#,
            None,
            &ComponentRegistry::with_defaults(),
            CompileOptions::strict(),
        )
        .expect("test template compiles");

        assert_eq!(
            vec!["reset".to_owned(), "search".to_owned()],
            template_action_names(template.root())
        );
    }

    #[allow(dead_code)]
    struct NoopHost;

    #[async_trait::async_trait]
    impl extension_host::HostApiProvider for NoopHost {
        async fn request_credential(
            &self,
            _params: extension_protocol::host::RequestCredentialParams,
        ) -> extension_host::HostResult<extension_protocol::host::RequestCredentialResult> {
            unimplemented!()
        }

        async fn resolve_secret(
            &self,
            _params: extension_protocol::host::ResolveSecretParams,
        ) -> extension_host::HostResult<extension_protocol::host::ResolveSecretResult> {
            unimplemented!()
        }

        async fn notify(
            &self,
            _params: extension_protocol::host::NotifyParams,
        ) -> extension_host::HostResult<extension_protocol::host::NotifyResult> {
            unimplemented!()
        }

        async fn quick_pick(
            &self,
            _params: extension_protocol::host::QuickPickParams,
        ) -> extension_host::HostResult<extension_protocol::host::QuickPickResult> {
            unimplemented!()
        }

        async fn open_view(
            &self,
            _params: extension_protocol::host::OpenViewParams,
        ) -> extension_host::HostResult<()> {
            unimplemented!()
        }

        async fn storage_get(
            &self,
            _params: extension_protocol::host::StorageGetParams,
        ) -> extension_host::HostResult<extension_protocol::host::StorageGetResult> {
            unimplemented!()
        }

        async fn storage_set(
            &self,
            _params: extension_protocol::host::StorageSetParams,
        ) -> extension_host::HostResult<()> {
            unimplemented!()
        }

        async fn log(
            &self,
            _params: extension_protocol::host::LogParams,
        ) -> extension_host::HostResult<()> {
            unimplemented!()
        }

        async fn show_dialog(
            &self,
            _params: extension_protocol::declarative_ui::UiDialogRequest,
        ) -> extension_host::HostResult<extension_protocol::declarative_ui::UiDialogResult>
        {
            unimplemented!()
        }
    }

    async fn fake_provider_session(
        observed: mpsc::UnboundedSender<UiActionRequest>,
    ) -> Arc<ProcessRpcSession> {
        let (client_side, provider_side) = duplex(32 * 1024);
        let (client_reader, client_writer) = tokio::io::split(client_side);
        let (provider_reader, provider_writer) = tokio::io::split(provider_side);
        tokio::spawn(fake_provider(provider_reader, provider_writer, observed));

        let client = JsonRpcClient::start(FramedTransport::new(client_reader, client_writer));
        let config = ProcessRpcSessionConfig::new(
            SpawnConfig::new("universal-plugin-panel-test"),
            NegotiationConfig::new("0.0.0-test", "panel-test").offer_api("extension", "1.0"),
        );
        let session = ProcessRpcSession::start_with_client(client, None, config)
            .await
            .expect("start fake provider session");
        Arc::new(session)
    }

    async fn fake_provider<R, W>(
        mut reader: R,
        mut writer: W,
        observed: mpsc::UnboundedSender<UiActionRequest>,
    ) where
        R: extension_host::ReadFramed,
        W: extension_host::WriteFramed,
    {
        while let Ok(message) = recv_async::<_, RpcMessage>(&mut reader).await {
            let RpcMessage::Request(request) = message else {
                continue;
            };
            let result = match request.method.as_str() {
                method::INIT => {
                    let init = InitResult::new("0.0.0-test")
                        .with_api("extension", "1.0")
                        .with_method(method::UI_ACTION)
                        .with_method(method::SHUTDOWN);
                    serde_json::to_value(init).expect("serialize init")
                }
                method::UI_ACTION => {
                    let params: UiActionRequest =
                        serde_json::from_value(request.params).expect("deserialize UI action");
                    let _ = observed.send(params);
                    json!({
                        "expected_revision": 0,
                        "operations": [
                            {"operation": "set", "key": "status", "value": "ready"}
                        ]
                    })
                }
                method::SHUTDOWN => Value::Null,
                _ => Value::Null,
            };
            send_async(
                &mut writer,
                &RpcMessage::Response(Response::ok(request.id, result)),
            )
            .await
            .expect("send fake provider response");
            if request.method == method::SHUTDOWN {
                break;
            }
        }
    }

    fn universal_panel_fixture(
        cx: &mut gpui::TestAppContext,
    ) -> (
        UniversalPluginService,
        gpui::Entity<UniversalPluginPanel>,
        gpui::Entity<UniversalPluginPanel>,
        gpui::Entity<Runtime>,
        mpsc::UnboundedReceiver<UiActionRequest>,
        tokio::runtime::Handle,
    ) {
        let root = tempfile::TempDir::new().expect("create extension fixture");
        std::fs::create_dir_all(root.path().join("bin")).expect("create bin");
        std::fs::create_dir_all(root.path().join("ui")).expect("create ui");
        std::fs::write(root.path().join("bin/provider"), b"provider").expect("write provider");
        std::fs::write(
            root.path().join("ui/main.html"),
            r#"<button action="load">Load</button>"#,
        )
        .expect("write template");
        let manifest = json!({
            "schema_version": 1,
            "id": "com.navop.panel-test",
            "name": "Panel Test",
            "version": "0.1.0",
            "engines": {"onetcli": ">=0.1.0"},
            "permissions": ["spawn:./bin/provider"],
            "runtime": {
                "ipc": [{
                    "id": "main",
                    "entry": {"command": "bin/provider"},
                    "transport": {"kind": "local_socket", "connect_timeout_ms": 2_500}
                }]
            },
            "contributes": {
                "declarativePanels": [{
                    "id": "main",
                    "title": "Panel Test",
                    "runtimeId": "main",
                    "template": "ui/main.html"
                }, {
                    "id": "second",
                    "title": "Panel Test Second",
                    "runtimeId": "main",
                    "template": "ui/main.html"
                }]
            }
        });
        std::fs::write(root.path().join("extension.json"), manifest.to_string())
            .expect("write manifest");
        let loaded = load_from_dir(root.path()).expect("load fixture manifest");
        let catalog = extension_runtime::ExtensionRuntimeCatalog::from_manifests(vec![loaded])
            .expect("create runtime catalog");

        cx.update(|cx| {
            gpui_component::init(cx);
            cx.set_global(gpui_component::Theme::default());
            one_core::gpui_tokio::init(cx);
            let tokio_handle = one_core::gpui_tokio::Tokio::handle(cx);
            let host_api_factory: extension_plugin_adapter::HostApiFactory =
                Arc::new(|_, _| Arc::new(HostApiHandler::new(Arc::new(NoopHost))));
            let (observed_tx, observed_rx) = mpsc::unbounded_channel();
            let session_factory: extension_plugin_adapter::SessionFactory = {
                let observed_tx = observed_tx.clone();
                Arc::new(move |_context| {
                    let observed_tx = observed_tx.clone();
                    Box::pin(async move {
                        Ok(fake_provider_session(observed_tx).await
                            as Arc<dyn extension_plugin_adapter::ManagedRpcSession>)
                    }) as BoxFuture<'static, _>
                })
            };
            let manager = extension_plugin_adapter::ActivationManager::new(
                catalog,
                session_factory,
                host_api_factory,
            );
            let service = UniversalPluginService::from_activation_manager(manager);
            let activation = tokio_handle
                .block_on(service.activate_panel("com.navop.panel-test::main"))
                .expect("activate fixture panel");
            let second_activation = tokio_handle
                .block_on(service.activate_panel("com.navop.panel-test::second"))
                .expect("activate second fixture panel");
            assert_eq!(activation.runtime_id, second_activation.runtime_id);
            let source = service
                .panel_source("com.navop.panel-test::main")
                .expect("load panel source");
            let template = UniversalPluginPanel::compile(source).expect("compile panel");
            let second_source = service
                .panel_source("com.navop.panel-test::second")
                .expect("load second panel source");
            let second_template =
                UniversalPluginPanel::compile(second_source).expect("compile second panel");

            let panel = cx.new(|cx| {
                UniversalPluginPanel::new(
                    template,
                    service.clone(),
                    activation,
                    "Panel Test".into(),
                    None,
                    cx,
                )
            });
            let runtime = panel.read(cx).runtime.clone();
            let second_panel = cx.new(|cx| {
                UniversalPluginPanel::new(
                    second_template,
                    service.clone(),
                    second_activation,
                    "Panel Test Second".into(),
                    None,
                    cx,
                )
            });
            (
                service,
                panel,
                second_panel,
                runtime,
                observed_rx,
                tokio_handle,
            )
        })
    }

    #[gpui::test]
    fn panel_actions_bridge_provider_patches_and_report_errors(cx: &mut gpui::TestAppContext) {
        let (service, panel, _second, runtime, mut observed, tokio) = universal_panel_fixture(cx);
        // GPUI's deterministic scheduler forbids activity from foreign Tokio
        // workers by default. This fixture intentionally bridges deterministic
        // GPUI futures to a real Tokio runtime and duplex JSON-RPC transport.
        cx.dispatcher.scheduler().allow_parking();

        let event = ActionEvent::new("load", "load-button", declarative_ui_demo::NodePath::root());
        cx.update(|cx| {
            runtime
                .update(cx, |runtime, cx| runtime.dispatch(event, cx))
                .expect("dispatch registered panel action");
        });
        let observed_request = tokio
            .block_on(async { observed.recv().await })
            .expect("provider receives the declarative action request");
        assert_eq!("load", observed_request.action);
        assert_eq!("load-button", observed_request.source_id);
        assert_eq!(Some(0), observed_request.expected_revision);
        assert_eq!(
            "universal-panel:0:1", observed_request.request_id,
            "request ids combine generation and panel-local sequence"
        );
        cx.run_until_parked();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline
            && cx.update(|cx| runtime.read(cx).get("status").is_none())
        {
            tokio.block_on(async {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            });
            cx.run_until_parked();
        }
        cx.update(|cx| {
            assert_eq!(Some("ready"), runtime.read(cx).get("status"));
            assert_eq!(1, runtime.read(cx).revision());
            assert!(panel.read(cx).action_error.is_none());
            assert!(panel.read(cx).pending_actions.is_empty());
        });

        tokio.block_on(service.shutdown());
    }

    #[gpui::test]
    fn active_runtime_health_refreshes_panel_generation(cx: &mut gpui::TestAppContext) {
        let (service, panel, _second, _runtime, _observed, tokio) = universal_panel_fixture(cx);
        let runtime_id = cx.update(|cx| panel.read(cx).runtime_id.clone());

        tokio.block_on(async {
            service
                .deactivate_panel("com.navop.panel-test::main")
                .await
                .expect("deactivate first stale fixture panel");
            service
                .deactivate_panel("com.navop.panel-test::second")
                .await
                .expect("deactivate second stale fixture panel");
            service
                .activate_panel("com.navop.panel-test::main")
                .await
                .expect("activate replacement fixture panel");
        });
        let replacement_generation = service
            .runtime_generation(&runtime_id)
            .expect("read replacement generation");
        assert_eq!(1, replacement_generation);
        cx.update(|cx| {
            assert_eq!(0, panel.read(cx).runtime_generation);

            panel.update(cx, |panel, cx| {
                panel.handle_runtime_monitor_event(
                    &extension_plugin_adapter::RuntimeMonitorEvent::HealthChanged {
                        runtime_id: runtime_id.clone(),
                        health: extension_plugin_adapter::RuntimeHealth {
                            state: extension_plugin_adapter::RuntimeActivationState::Active,
                            session_closed: false,
                            ping_error: None,
                            restart_attempts: 1,
                            restart_budget: 3,
                            restart_backoff_remaining: None,
                        },
                    },
                    cx,
                );
            });
            assert_eq!(1, panel.read(cx).runtime_generation);
            assert!(panel.read(cx).action_error.is_none());
        });

        tokio.block_on(service.shutdown());
    }

    #[gpui::test]
    fn closing_panel_deactivates_its_runtime_only_after_last_panel(cx: &mut gpui::TestAppContext) {
        let (service, first, second, _runtime, _observed, tokio) = universal_panel_fixture(cx);
        cx.dispatcher.scheduler().allow_parking();

        let first_close = cx.update(|cx| {
            cx.open_window(gpui::WindowOptions::default(), |_window, cx| {
                cx.new(|_| PanelTestRoot(first.clone()))
            })
            .expect("open first panel window")
            .update(cx, |_, window, cx| {
                first.update(cx, |panel, cx| panel.try_close("tab", window, cx))
            })
        });
        cx.run_until_parked();
        let first_closed = tokio.block_on(first_close.expect("close first panel in window"));
        assert!(first_closed);
        assert_eq!(
            ["com.navop.panel-test::second"].as_slice(),
            service.active_panel_keys().iter().collect::<Vec<_>>()
        );
        assert!(
            service
                .runtime_generation("com.navop.panel-test::main")
                .is_ok()
        );

        let second_close = cx.update(|cx| {
            cx.open_window(gpui::WindowOptions::default(), |_window, cx| {
                cx.new(|_| PanelTestRoot(second.clone()))
            })
            .expect("open second panel window")
            .update(cx, |_, window, cx| {
                second.update(cx, |panel, cx| panel.try_close("tab", window, cx))
            })
        });
        cx.run_until_parked();
        let second_closed = tokio.block_on(second_close.expect("close second panel in window"));
        assert!(second_closed);
        assert!(service.active_panel_keys().is_empty());

        // Tab closure returns immediately, while provider shutdown continues
        // on its detached Tokio task. Wait for that asynchronous lifecycle step
        // so this test observes the eventual rather than incidental state.
        let deadline = Instant::now() + Duration::from_secs(2);
        while service
            .runtime_generation("com.navop.panel-test::main")
            .is_ok()
        {
            assert!(
                Instant::now() < deadline,
                "shared runtime was not shut down after the last panel closed"
            );
            tokio.block_on(async {
                tokio::time::sleep(Duration::from_millis(10)).await;
            });
            cx.run_until_parked();
        }

        tokio.block_on(service.shutdown());
    }

    #[gpui::test]
    fn stale_panel_close_does_not_release_replacement_activation(cx: &mut gpui::TestAppContext) {
        let (service, first, second, _runtime, _observed, tokio) = universal_panel_fixture(cx);
        cx.dispatcher.scheduler().allow_parking();
        let (first_activation, second_activation) = cx.update(|cx| {
            (
                first.read(cx).activation.clone(),
                second.read(cx).activation.clone(),
            )
        });

        tokio.block_on(async {
            service
                .deactivate_activation(&first_activation)
                .await
                .expect("release first fixture activation");
            service
                .deactivate_activation(&second_activation)
                .await
                .expect("release second fixture activation");
        });
        let replacement = tokio
            .block_on(service.activate_panel("com.navop.panel-test::main"))
            .expect("activate replacement panel");
        assert_ne!(first_activation.activation_id, replacement.activation_id);

        let stale_close = cx.update(|cx| {
            cx.open_window(gpui::WindowOptions::default(), |_window, cx| {
                cx.new(|_| PanelTestRoot(first.clone()))
            })
            .expect("open stale panel window")
            .update(cx, |_, window, cx| {
                first.update(cx, |panel, cx| panel.try_close("tab", window, cx))
            })
        });
        cx.run_until_parked();
        assert!(tokio.block_on(stale_close.expect("close stale panel")));
        tokio.block_on(async {
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        cx.run_until_parked();
        assert!(
            service
                .active_panel_keys()
                .contains("com.navop.panel-test::main"),
            "replacement activation disappeared after stale close"
        );

        tokio.block_on(async {
            service
                .deactivate_activation(&replacement)
                .await
                .expect("release replacement activation");
            service.shutdown().await;
        });
    }
}
