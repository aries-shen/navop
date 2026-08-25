use super::*;
use crate::universal_plugins::{
    GlobalUniversalPluginService, UniversalPanelDescriptor, UniversalPluginStatus,
};

impl HomePage {
    /// Refreshes the immutable UI projection and monitor snapshots.
    ///
    /// This method never starts a provider process. Activation happens only in
    /// response to a user intent.
    pub(super) fn refresh_universal_plugin_panels(&mut self, cx: &mut Context<Self>) {
        let Some(service) = cx
            .try_global::<GlobalUniversalPluginService>()
            .map(|global| global.service())
        else {
            self.universal_plugin_panels.clear();
            self.universal_plugin_status.clear();
            cx.notify();
            return;
        };

        self.universal_plugin_panels = service
            .panel_catalog()
            .into_iter()
            .map(UniversalPanelDescriptor::from)
            .collect();
        self.sync_universal_plugin_health(&service);
        cx.notify();
    }

    pub(super) fn activate_universal_panel(
        &mut self,
        panel_key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self
            .universal_plugin_panels
            .iter()
            .any(|panel| panel.panel_key == panel_key)
        {
            return;
        }
        if !self
            .activating_universal_panels
            .insert(panel_key.to_owned())
        {
            return;
        }

        self.universal_plugin_activation_error = None;
        cx.notify();

        let Some(service) = cx
            .try_global::<GlobalUniversalPluginService>()
            .map(|global| global.service())
        else {
            self.activating_universal_panels.remove(panel_key);
            self.universal_plugin_activation_error =
                Some("Universal plugin service is unavailable".to_owned());
            cx.notify();
            return;
        };

        if service.active_panel_keys().contains(panel_key) {
            self.deactivate_universal_panel(panel_key, cx);
            return;
        }

        let activation_panel_key = panel_key.to_owned();
        let activation_task = Tokio::spawn(cx, async move {
            service.activate_panel(&activation_panel_key).await
        });
        let Some(panel) = self
            .universal_plugin_panels
            .iter()
            .find(|panel| panel.panel_key == panel_key)
            .cloned()
        else {
            self.activating_universal_panels.remove(panel_key);
            cx.notify();
            return;
        };
        let tab_container = cx
            .try_global::<crate::onetcli_app::GlobalTabContainer>()
            .map(|global| global.primary_pane())
            .unwrap_or_else(|| self.tab_container.clone());
        let panel_key = panel_key.to_owned();
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = activation_task
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
            let update = this.update(cx, |this, cx| {
                this.activating_universal_panels.remove(&panel_key);
                let update = match result {
                    Ok(handle) => {
                        this.universal_plugin_status
                            .insert(handle.runtime_id.clone(), handle.state.into());
                        this.universal_plugin_activations
                            .insert(panel_key.clone(), handle.clone());
                        (Some(handle), None)
                    }
                    Err(error) => {
                        tracing::error!(panel = panel_key, %error, "failed to activate universal plugin panel");
                        this.universal_plugin_activation_error = Some(error);
                        (None, this.universal_plugin_activation_error.clone())
                    }
                };
                cx.notify();
                update
            })
            .ok()
            .unwrap_or_default();
            let (activation, notification) = update;

            let _ = cx.update_window(window_handle, |_, window, cx| {
                window.refresh();
                if let Some(activation) = activation
                    && let Err(error) = this.update(cx, |this, cx| {
                        this.open_universal_panel_tab(
                            &panel,
                            activation,
                            &tab_container,
                            window,
                            cx,
                        );
                    })
                {
                    tracing::error!(panel = panel_key, %error, "failed to mount universal plugin panel");
                    window.push_notification(
                        Notification::error("Failed to mount universal plugin panel".to_owned())
                            .autohide(true),
                        cx,
                    );
                }
                if let Some(error) = notification {
                    window.push_notification(
                        Notification::error(error).autohide(true),
                        cx,
                    );
                }
            });
        })
        .detach();
    }

    pub(super) fn open_universal_panel_tab(
        &mut self,
        panel: &UniversalPanelDescriptor,
        activation: extension_plugin_adapter::ActivationHandle,
        tab_container: &Entity<TabContainer>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(service) = cx
            .try_global::<GlobalUniversalPluginService>()
            .map(|global| global.service())
        else {
            return;
        };
        let source = match service.panel_source(&panel.panel_key) {
            Ok(source) => source,
            Err(error) => {
                tracing::error!(panel = panel.panel_key, %error, "failed to load universal plugin panel source");
                window.push_notification(Notification::error(error.to_string()).autohide(true), cx);
                return;
            }
        };
        let template = match crate::universal_plugin_panel::UniversalPluginPanel::compile(source) {
            Ok(template) => template,
            Err(error) => {
                tracing::error!(panel = panel.panel_key, %error, "failed to compile universal plugin panel");
                window.push_notification(Notification::error(error.to_string()).autohide(true), cx);
                return;
            }
        };
        let icon = panel.icon.clone();
        let title = panel.title.clone();
        let tab_id = universal_plugin_tab_id(&panel.panel_key);
        tab_container.update(cx, |tabs, cx| {
            tabs.activate_or_add_tab_lazy(
                tab_id.clone(),
                |_window, cx| {
                    let view = cx.new(|cx| {
                        crate::universal_plugin_panel::UniversalPluginPanel::new(
                            template,
                            service.clone(),
                            activation,
                            title,
                            icon,
                            cx,
                        )
                    });
                    TabItem::new(tab_id, "home", view)
                },
                window,
                cx,
            );
        });
    }

    pub(super) fn deactivate_universal_panel(&mut self, panel_key: &str, cx: &mut Context<Self>) {
        let Some(service) = cx
            .try_global::<GlobalUniversalPluginService>()
            .map(|global| global.service())
        else {
            return;
        };
        let Some(activation) = self.universal_plugin_activations.get(panel_key).cloned() else {
            return;
        };

        self.activating_universal_panels.remove(panel_key);
        let panel_key = panel_key.to_owned();
        let runtime_id = activation.runtime_id.clone();
        let activation_id = activation.activation_id;
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = service.deactivate_activation(&activation).await;
            let _ = this.update(cx, |this, cx| {
                if result.is_ok() {
                    if this
                        .universal_plugin_activations
                        .get(&panel_key)
                        .is_some_and(|current| current.activation_id == activation_id)
                    {
                        this.universal_plugin_activations.remove(&panel_key);
                    }
                    this.universal_plugin_status.remove(&runtime_id);
                    this.refresh_universal_plugin_panels(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn sync_universal_plugin_health(&mut self, service: &UniversalPluginService) {
        self.universal_plugin_status = service
            .runtime_healths()
            .into_iter()
            .map(|(runtime_id, health)| (runtime_id, health.state.into()))
            .collect();
    }

    pub(super) fn observe_universal_plugin_health(
        &mut self,
        service: &UniversalPluginService,
        cx: &mut Context<Self>,
    ) {
        let mut events = service.subscribe();
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            loop {
                let event = match events.recv().await {
                    Ok(event) => event,
                    Err(
                        tokio::sync::broadcast::error::RecvError::Lagged(_)
                        | tokio::sync::broadcast::error::RecvError::Closed,
                    ) => break,
                };

                let updated = this.update(cx, |this, cx| {
                    match event {
                        extension_plugin_adapter::RuntimeMonitorEvent::HealthChanged {
                            runtime_id,
                            health,
                        } => {
                            this.universal_plugin_status
                                .insert(runtime_id, health.state.into());
                        }
                        extension_plugin_adapter::RuntimeMonitorEvent::RuntimeRemoved {
                            runtime_id,
                        } => {
                            this.universal_plugin_status.remove(&runtime_id);
                        }
                        extension_plugin_adapter::RuntimeMonitorEvent::CheckFailed {
                            runtime_id,
                            ..
                        } => {
                            this.universal_plugin_status
                                .insert(runtime_id, UniversalPluginStatus::Failed);
                        }
                    }
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
    }
}

pub(crate) fn universal_plugin_tab_id(panel_key: &str) -> String {
    format!("universal-panel:{panel_key}")
}
