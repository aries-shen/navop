//! Application-owned lifecycle for universal resource plugin panels.
//!
//! GPUI code may render catalog metadata and dispatch activation intents, but
//! this service remains the sole owner of provider processes and supervision.

use std::collections::BTreeSet;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use extension_plugin_adapter::{
    ActivationError, ActivationHandle, ActivationManager, DeclarativePanelDescriptor,
    HostApiFactory, RuntimeActivationState, RuntimeHealth, RuntimeMonitor, RuntimeMonitorConfig,
    RuntimeMonitorEvent, SessionFactory,
};
use extension_runtime::{
    ExtensionRuntimeCatalog, GlobalExtensionRuntimeCatalog,
    extension::manifest::DeclarativePanelPlacement,
};
use gpui::SharedString;
use gpui_component::{IconName, IconNamed};
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

/// A GPUI-owned descriptor projected from the host catalog.
///
/// The icon is retained because it is UI metadata; activation permissions and
/// filesystem paths intentionally never cross this boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniversalPanelDescriptor {
    pub extension_id: String,
    pub panel_key: String,
    pub title: SharedString,
    pub runtime_id: String,
    pub placement: UniversalPanelPlacement,
    pub icon: Option<SharedString>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniversalPanelPlacement {
    HomeSidebar,
    HomeTab,
}

impl From<DeclarativePanelDescriptor> for UniversalPanelDescriptor {
    fn from(panel: DeclarativePanelDescriptor) -> Self {
        Self {
            extension_id: panel.extension_id,
            panel_key: panel.panel_key,
            title: panel.title.into(),
            runtime_id: panel.runtime_id,
            placement: match panel.placement {
                DeclarativePanelPlacement::HomeSidebar => UniversalPanelPlacement::HomeSidebar,
                DeclarativePanelPlacement::HomeTab => UniversalPanelPlacement::HomeTab,
            },
            icon: panel
                .icon
                .and_then(|icon| universal_plugin_icon_path(&icon)),
        }
    }
}

fn universal_plugin_icon_path(icon: &str) -> Option<SharedString> {
    match icon {
        "database" => Some(IconName::Database),
        "search" => Some(IconName::Search),
        "globe" => Some(IconName::Globe),
        "terminal" => Some(IconName::TerminalColor),
        "extensions" => Some(IconName::ExtensionsColor),
        "box" => Some(IconName::Apps),
        "radio" => Some(IconName::SerialPort),
        "cluster" => Some(IconName::Network),
        "cloud" => Some(IconName::DockerColor),
        "send" => Some(IconName::Export),
        _ => None,
    }
    .map(IconNamed::path)
}

/// A compact status snapshot safe to render without awaiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UniversalPluginStatus {
    Starting,
    Restarting,
    Active,
    Degraded,
    Failed,
    CrashLoop,
}

impl From<RuntimeActivationState> for UniversalPluginStatus {
    fn from(state: RuntimeActivationState) -> Self {
        match state {
            RuntimeActivationState::Starting => Self::Starting,
            RuntimeActivationState::Restarting => Self::Restarting,
            RuntimeActivationState::Active => Self::Active,
            RuntimeActivationState::Degraded => Self::Degraded,
            RuntimeActivationState::Failed => Self::Failed,
            RuntimeActivationState::CrashLoop => Self::CrashLoop,
        }
    }
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

    #[cfg(test)]
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.manager, &other.manager) && Arc::ptr_eq(&self.monitor, &other.monitor)
    }

    /// UI-safe catalog projection without template/style paths or permissions.
    pub(crate) fn panel_catalog(&self) -> Vec<DeclarativePanelDescriptor> {
        self.manager.declarative_panel_catalog()
    }

    pub(crate) fn runtime_healths(&self) -> Vec<(String, RuntimeHealth)> {
        self.monitor
            .runtime_healths()
            .into_iter()
            .collect::<Vec<_>>()
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::broadcast::Receiver<RuntimeMonitorEvent> {
        self.monitor.subscribe()
    }

    pub(crate) async fn activate_panel(
        &self,
        panel_key: &str,
    ) -> Result<ActivationHandle, ActivationError> {
        let handle = self.manager.activate_panel(panel_key).await?;
        self.monitor.track(handle.runtime_id.clone());
        Ok(handle)
    }

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
    #[allow(dead_code)]
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

    pub(crate) fn active_panel_keys(&self) -> BTreeSet<String> {
        self.manager.active_panel_keys()
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

    #[test]
    fn universal_panel_projection_keeps_only_ui_metadata() {
        let descriptor = DeclarativePanelDescriptor {
            extension_id: "elasticsearch".to_owned(),
            panel_key: "elasticsearch.cluster".to_owned(),
            title: "Elasticsearch".to_owned(),
            runtime_id: "elasticsearch.runtime".to_owned(),
            placement: DeclarativePanelPlacement::HomeSidebar,
            icon: Some("search".to_owned()),
        };

        let panel = UniversalPanelDescriptor::from(descriptor);

        assert_eq!(panel.extension_id, "elasticsearch");
        assert_eq!(panel.panel_key, "elasticsearch.cluster");
        assert_eq!(panel.title.as_ref(), "Elasticsearch");
        assert_eq!(panel.runtime_id, "elasticsearch.runtime");
        assert_eq!(panel.placement, UniversalPanelPlacement::HomeSidebar);
        assert_eq!(panel.icon.as_deref(), Some("icons/search.svg"));
    }

    #[test]
    fn universal_panel_projection_converts_manifest_icon_to_asset_path() {
        let descriptor = DeclarativePanelDescriptor {
            extension_id: "kubernetes".to_owned(),
            panel_key: "kubernetes.cluster".to_owned(),
            title: "Kubernetes".to_owned(),
            runtime_id: "kubernetes.runtime".to_owned(),
            placement: DeclarativePanelPlacement::HomeSidebar,
            icon: Some("cluster".to_owned()),
        };

        let panel = UniversalPanelDescriptor::from(descriptor);

        assert_eq!(panel.icon.as_deref(), Some("icons/network.svg"));
    }

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
