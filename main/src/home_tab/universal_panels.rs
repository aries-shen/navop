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
        let panel_key = panel_key.to_owned();
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = activation_task
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));

            let notification = this.update(cx, |this, cx| {
                this.activating_universal_panels.remove(&panel_key);
                let notification = match result {
                    Ok(handle) => {
                        this.universal_plugin_status
                            .insert(handle.runtime_id, handle.state.into());
                        None
                    }
                    Err(error) => {
                        tracing::error!(panel = panel_key, %error, "failed to activate universal plugin panel");
                        this.universal_plugin_activation_error = Some(error);
                        this.universal_plugin_activation_error.clone()
                    }
                };
                cx.notify();
                notification
            })
            .ok()
            .flatten();

            let _ = cx.update_window(window_handle, |_, window, cx| {
                window.refresh();
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

    pub(super) fn deactivate_universal_panel(&mut self, panel_key: &str, cx: &mut Context<Self>) {
        let Some(service) = cx
            .try_global::<GlobalUniversalPluginService>()
            .map(|global| global.service())
        else {
            return;
        };
        let Some(runtime_id) = self
            .universal_plugin_panels
            .iter()
            .find(|panel| panel.panel_key == panel_key)
            .map(|panel| panel.runtime_id.clone())
        else {
            return;
        };

        self.activating_universal_panels.remove(panel_key);
        let panel_key = panel_key.to_owned();
        let runtime_id = runtime_id.clone();
        cx.spawn(async move |this, cx: &mut AsyncApp| {
            let result = service.deactivate_panel(&panel_key).await;
            let _ = this.update(cx, |this, cx| {
                if result.is_ok() {
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
