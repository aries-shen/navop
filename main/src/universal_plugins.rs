//! Application-owned lifecycle for universal resource plugin panels.
//!
//! GPUI code may render catalog metadata and dispatch activation intents, but
//! this service remains the sole owner of provider processes and supervision.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use extension_plugin_adapter::{
    ActivationError, ActivationHandle, ActivationManager, DeclarativePanelDescriptor,
    HostApiFactory, RuntimeHealth, RuntimeMonitor, RuntimeMonitorConfig, SessionFactory,
};
use extension_runtime::{ExtensionRuntimeCatalog, GlobalExtensionRuntimeCatalog};
use one_core::gpui_tokio::Tokio;

/// A global wrapper that gives the service exactly one application owner.
#[derive(Clone)]
pub(crate) struct GlobalUniversalPluginService {
    service: UniversalPluginService,
}

impl gpui::Global for GlobalUniversalPluginService {}

impl GlobalUniversalPluginService {
    fn new(service: UniversalPluginService) -> Self {
        Self { service }
    }

    pub(crate) fn service(&self) -> UniversalPluginService {
        self.service.clone()
    }
}

/// The host-owned activation and supervision facade used by GPUI features.
#[derive(Clone)]
pub(crate) struct UniversalPluginService {
    manager: Arc<ActivationManager>,
    monitor: Arc<RuntimeMonitor>,
    shutdown_lock: Arc<tokio::sync::Mutex<()>>,
    stopped: Arc<AtomicBool>,
}

impl UniversalPluginService {
    fn from_catalog(catalog: Arc<ExtensionRuntimeCatalog>) -> Self {
        let manager = Arc::new(ActivationManager::from_shared_catalog(
            catalog,
            production_session_factory(),
            production_host_api_factory(),
        ));
        let monitor = Arc::new(RuntimeMonitor::new(
            Arc::clone(&manager),
            RuntimeMonitorConfig::default(),
        ));
        Self {
            manager,
            monitor,
            shutdown_lock: Arc::new(tokio::sync::Mutex::new(())),
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.manager, &other.manager) && Arc::ptr_eq(&self.monitor, &other.monitor)
    }

    /// UI-safe catalog projection without template/style paths or permissions.
    #[cfg(test)]
    pub(crate) fn panel_catalog(&self) -> Vec<DeclarativePanelDescriptor> {
        self.manager.declarative_panel_catalog()
    }

    #[cfg(test)]
    pub(crate) fn runtime_health(&self, runtime_id: &str) -> Option<RuntimeHealth> {
        self.monitor.runtime_health(runtime_id)
    }

    #[cfg(test)]
    pub(crate) fn runtime_healths(&self) -> Vec<(String, RuntimeHealth)> {
        self.monitor
            .runtime_healths()
            .into_iter()
            .collect::<Vec<_>>()
    }

    #[cfg(test)]
    pub(crate) async fn activate_panel(
        &self,
        panel_key: &str,
    ) -> Result<ActivationHandle, ActivationError> {
        let handle = self.manager.activate_panel(panel_key).await?;
        self.monitor.track(handle.runtime_id.clone());
        Ok(handle)
    }

    #[cfg(test)]
    pub(crate) async fn deactivate_panel(&self, panel_key: &str) -> Result<(), ActivationError> {
        let runtime_id = self
            .panel_catalog()
            .into_iter()
            .find(|panel| panel.panel_key == panel_key)
            .map(|panel| panel.runtime_id);
        let result = self.manager.deactivate_panel(panel_key).await;
        if result.is_ok()
            && let Some(runtime_id) = runtime_id
        {
            // Keep monitoring a runtime while another panel still references it,
            // even if its first session is still starting and has no client yet.
            if self.manager.runtime_state(&runtime_id).is_err() {
                self.monitor.untrack(&runtime_id);
            }
        }
        result
    }

    #[cfg(test)]
    pub(crate) async fn deactivate_extension(
        &self,
        extension_id: &str,
    ) -> Result<(), ActivationError> {
        let runtime_ids: Vec<String> = self
            .panel_catalog()
            .into_iter()
            .filter(|panel| panel.extension_id == extension_id)
            .map(|panel| panel.runtime_id)
            .collect::<Vec<_>>();
        let result = self.manager.deactivate_extension(extension_id).await;
        if result.is_ok() {
            for runtime_id in runtime_ids {
                self.monitor.untrack(&runtime_id);
            }
        }
        result
    }

    /// Gracefully stops active runtimes and then stops the monitor task.
    pub(crate) async fn shutdown(&self) {
        let _shutdown_guard = self.shutdown_lock.lock().await;
        if self.stopped.load(Ordering::SeqCst) {
            return;
        }

        let runtime_ids = self
            .monitor
            .tracked_runtimes()
            .into_iter()
            .collect::<Vec<_>>();
        for runtime_id in runtime_ids {
            let _ = self.manager.deactivate_runtime(&runtime_id).await;
            self.monitor.untrack(&runtime_id);
        }
        self.monitor.stop().await;
        self.stopped.store(true, Ordering::SeqCst);
    }
}

impl UniversalPluginService {
    fn start_monitor(&self) -> Result<(), extension_plugin_adapter::RuntimeMonitorError> {
        if self.stopped.load(Ordering::SeqCst) {
            return Err(extension_plugin_adapter::RuntimeMonitorError::AlreadyRunning);
        }
        self.monitor.start()
    }
}

fn production_session_factory() -> SessionFactory {
    extension_plugin_adapter::process_session_factory()
}

fn production_host_api_factory() -> HostApiFactory {
    Arc::new(|binding| {
        let host = extension_plugin_adapter::UniversalProviderHost::new(
            binding.permissions.iter().cloned(),
            Arc::new(extension_plugin_adapter::MapSecretResolver::default()),
        );
        Arc::new(extension_host::HostApiHandler::new(Arc::new(host)))
    })
}

pub(crate) fn init(cx: &mut gpui::App) {
    assert!(
        cx.try_global::<GlobalUniversalPluginService>().is_none(),
        "universal plugin service must have exactly one application owner"
    );

    let catalog = cx
        .default_global::<GlobalExtensionRuntimeCatalog>()
        .get()
        .unwrap_or_else(|| Arc::new(ExtensionRuntimeCatalog::empty()));
    let global = GlobalUniversalPluginService::new(UniversalPluginService::from_catalog(catalog));
    let service = global.service();
    let startup_service = service.clone();
    let quit_service = service.clone();
    cx.set_global(global);

    // RuntimeMonitor::start must be called from the Tokio runtime. Starting it
    // through a spawned task also keeps startup off the GPUI foreground thread.
    Tokio::spawn(cx, async move {
        if let Err(error) = startup_service.start_monitor() {
            tracing::warn!(%error, "failed to start universal plugin runtime monitor");
        }
    })
    .detach();

    // Normal quit paths await `shutdown` before `cx.quit()`. This is only the
    // bounded fallback for platform-driven quit paths.
    cx.on_app_quit(move |cx| {
        let quit_service = quit_service.clone();
        let shutdown_task = Tokio::spawn(cx, async move { quit_service.shutdown().await });
        async move {
            if let Err(error) = shutdown_task.await {
                tracing::warn!(%error, "universal plugin shutdown fallback did not complete");
            }
        }
    })
    .detach();
}

pub(crate) fn spawn_shutdown(
    cx: &gpui::App,
) -> Option<gpui::Task<Result<(), one_core::gpui_tokio::JoinError>>> {
    let service = cx.try_global::<GlobalUniversalPluginService>()?.service();
    Some(Tokio::spawn(cx, async move { service.shutdown().await }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn universal_plugin_service_has_one_application_owner(cx: &mut gpui::TestAppContext) {
        let (first, second, runtime) = cx.update(|cx| {
            one_core::gpui_tokio::init(cx);
            extension_runtime::init(cx);
            init(cx);

            let first = cx.global::<GlobalUniversalPluginService>().service();
            let second = cx.global::<GlobalUniversalPluginService>().service();
            let runtime = one_core::gpui_tokio::Tokio::handle(cx);
            (first, second, runtime)
        });

        assert!(first.same_owner(&second));
        runtime.block_on(first.shutdown());
        runtime.block_on(second.shutdown());
    }
}
