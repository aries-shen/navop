use extension_plugin_adapter::ActivationHandle;
use extension_protocol::resource::{ResourceCloseParams, ResourceOpenParams};
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, SharedString, Styled, Task, Window, div,
};
use one_core::{
    storage::{ActiveConnectionLease, ActiveConnections, StoredConnection},
    tab_container::{TabContent, TabContentEvent},
};

use crate::universal_plugins::UniversalPluginService;

enum State {
    Connecting,
    Connected {
        activation: ActivationHandle,
        resource_id: String,
        capabilities: Vec<String>,
    },
    Failed(String),
}

pub(crate) struct ExtensionConnectionTab {
    connection_lease: Option<ActiveConnectionLease>,
    title: SharedString,
    focus_handle: FocusHandle,
    service: UniversalPluginService,
    runtime_id: String,
    state: State,
    closing: bool,
}

impl ExtensionConnectionTab {
    pub(crate) fn load(
        service: UniversalPluginService,
        connection: StoredConnection,
        contribution: extension_runtime::RegisteredResourceConnectionContribution,
        cx: &mut App,
    ) -> Entity<Self> {
        let connection_id = connection.id.expect("saved extension connection");
        let connection_lease = cx
            .default_global::<ActiveConnections>()
            .lease(connection_id);
        let runtime_id = contribution.runtime_id.clone();
        let view = cx.new(|cx| Self {
            connection_lease: Some(connection_lease),
            title: connection.name.clone().into(),
            focus_handle: cx.focus_handle(),
            service: service.clone(),
            runtime_id: runtime_id.clone(),
            state: State::Connecting,
            closing: false,
        });
        let params = connection.to_extension_params();
        view.update(cx, |_, cx| {
            cx.spawn(async move |this, cx| {
                let result: anyhow::Result<_> = async {
                    let params = params?;
                    let activation = service.activate_runtime(&runtime_id).await?;
                    let client = match service.universal_plugin_client(&runtime_id) {
                        Ok(client) => client,
                        Err(error) => {
                            let _ = service.deactivate_activation(&activation).await;
                            return Err(error.into());
                        }
                    };
                    let mut config = params.config;
                    config.insert(
                        "credential_refs".into(),
                        serde_json::Value::Object(
                            params
                                .secrets
                                .keys()
                                .map(|field| {
                                    (
                                        field.clone(),
                                        serde_json::Value::String(format!(
                                            "secret://self/{connection_id}:{field}"
                                        )),
                                    )
                                })
                                .collect(),
                        ),
                    );
                    let opened = client
                        .client()
                        .open_resource(&ResourceOpenParams {
                            resource_type: contribution.resource_type,
                            config: serde_json::Value::Object(config),
                            metadata: None,
                        })
                        .await;
                    match opened {
                        Ok(opened) => Ok((activation, opened)),
                        Err(error) => {
                            let _ = service.deactivate_activation(&activation).await;
                            Err(error.into())
                        }
                    }
                }
                .await;
                let _ = this.update(cx, |this, cx| {
                    this.state = match result {
                        Ok((activation, opened)) if !this.closing => State::Connected {
                            activation,
                            resource_id: opened.resource_id,
                            capabilities: opened.capabilities,
                        },
                        Ok((activation, opened)) => {
                            let service = this.service.clone();
                            let resource_id = opened.resource_id;
                            one_core::gpui_tokio::Tokio::spawn_result(cx, async move {
                                if let Ok(client) =
                                    service.universal_plugin_client(&activation.runtime_id)
                                {
                                    let _ = client
                                        .client()
                                        .close_resource(&ResourceCloseParams { resource_id })
                                        .await;
                                }
                                let _ = service.deactivate_activation(&activation).await;
                                Ok(())
                            })
                            .detach();
                            State::Failed("Connection closed".into())
                        }
                        Err(error) => State::Failed(error.to_string()),
                    };
                    cx.notify();
                });
            })
            .detach();
        });
        view
    }

    fn close(&mut self, cx: &mut Context<Self>) -> Task<bool> {
        self.closing = true;
        let state = std::mem::replace(&mut self.state, State::Failed("Connection closed".into()));
        self.connection_lease.take();
        let State::Connected {
            activation,
            resource_id,
            ..
        } = state
        else {
            return Task::ready(true);
        };
        let service = self.service.clone();
        let runtime_id = self.runtime_id.clone();
        let task = one_core::gpui_tokio::Tokio::spawn_result(cx, async move {
            if let Ok(client) = service.universal_plugin_client(&runtime_id) {
                let _ = client
                    .client()
                    .close_resource(&ResourceCloseParams { resource_id })
                    .await;
            }
            let _ = service.deactivate_activation(&activation).await;
            Ok(())
        });
        cx.spawn(async move |_, _| task.await.is_ok())
    }
}

impl EventEmitter<TabContentEvent> for ExtensionConnectionTab {}

impl Focusable for ExtensionConnectionTab {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ExtensionConnectionTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().p_4().child(match &self.state {
            State::Connecting => "Connecting extension...".to_string(),
            State::Connected { capabilities, .. } => {
                format!("Connected\n\nCapabilities:\n{}", capabilities.join("\n"))
            }
            State::Failed(error) => format!("Extension connection failed: {error}"),
        })
    }
}

impl TabContent for ExtensionConnectionTab {
    fn content_key(&self) -> &'static str {
        "ExtensionConnection"
    }
    fn title(&self, _cx: &App) -> SharedString {
        self.title.clone()
    }
    fn can_rename(&self, _cx: &App) -> bool {
        false
    }
    fn try_close(&mut self, _id: &str, _window: &mut Window, cx: &mut Context<Self>) -> Task<bool> {
        self.close(cx)
    }
}
