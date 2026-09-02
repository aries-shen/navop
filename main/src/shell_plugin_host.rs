mod components;
mod context;
mod resource;
mod value;

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context as _, Result, anyhow};
use extension_host::CancellationToken;
use extension_plugin_adapter::ActivationHandle;
use gpui::{App, WeakEntity, Window};
use gpui_shell::{LoadedScriptView, ShellRuntime, ViewLoadOptions, policy::Policy};
use one_core::tab_container::TabItem;

use self::{
    components::component_registry,
    context::context_module,
    resource::{ShellResourceSession, resource_module},
};
use crate::{
    onetcli_app::GlobalTabContainer, shell_plugin_tab::ShellPluginTab,
    universal_plugins::UniversalPluginService,
};

static NEXT_SHELL_TAB_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(crate) struct ShellPluginHost {
    service: UniversalPluginService,
    runtime: Rc<ShellRuntime>,
    tokio: tokio::runtime::Handle,
    tabs: Rc<RefCell<HashMap<String, Vec<TrackedShellTab>>>>,
    retiring: Rc<RefCell<HashSet<String>>>,
}

#[derive(Clone)]
struct TrackedShellTab {
    view_key: String,
    tab: WeakEntity<ShellPluginTab>,
}

impl gpui::Global for ShellPluginHost {}

pub(crate) struct PreparedShellView {
    pub(crate) contribution: extension_runtime::RegisteredShellViewContribution,
    pub(crate) activations: Vec<ActivationHandle>,
}

impl ShellPluginHost {
    pub(crate) fn new(service: UniversalPluginService, cx: &App) -> Result<Self> {
        let components = component_registry()?;
        Ok(Self {
            service,
            runtime: ShellRuntime::new_isolated_with_components(components)
                .context("create gpui-shell runtime")?,
            tokio: one_core::gpui_tokio::Tokio::handle(cx),
            tabs: Rc::new(RefCell::new(HashMap::new())),
            retiring: Rc::new(RefCell::new(HashSet::new())),
        })
    }

    pub(crate) fn contribution(
        &self,
        extension_id: &str,
        view_id: &str,
    ) -> Option<extension_runtime::RegisteredShellViewContribution> {
        self.service.shell_view(extension_id, view_id)
    }

    async fn prepare_with_service(
        service: UniversalPluginService,
        contribution: extension_runtime::RegisteredShellViewContribution,
        cancel: CancellationToken,
    ) -> Result<PreparedShellView> {
        let mut activations = Vec::new();
        for runtime_id in contribution.backends.values() {
            let activation = tokio::select! {
                _ = cancel.cancelled() => {
                    release_activations(&service, activations).await;
                    return Err(anyhow!("extension activation cancelled"));
                }
                activation = service.activate_runtime(runtime_id) => activation,
            };
            match activation {
                Ok(handle) => activations.push(handle),
                Err(error) => {
                    release_activations(&service, activations).await;
                    return Err(anyhow!(error.to_string()));
                }
            }
        }
        Ok(PreparedShellView {
            contribution,
            activations,
        })
    }

    pub(crate) fn start_prepare(
        &self,
        contribution: extension_runtime::RegisteredShellViewContribution,
        cancel: CancellationToken,
    ) -> tokio::sync::oneshot::Receiver<Result<PreparedShellView>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let service = self.service.clone();
        self.tokio.spawn(async move {
            let result = Self::prepare_with_service(service.clone(), contribution, cancel).await;
            if let Err(result) = sender.send(result)
                && let Ok(prepared) = result
            {
                release_activations(&service, prepared.activations).await;
            }
        });
        receiver
    }

    pub(crate) fn load(
        &self,
        prepared: PreparedShellView,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<LoadedScriptView> {
        validate_entry_path(&prepared.contribution)?;
        let session = Arc::new(ShellResourceSession::new(
            self.service.clone(),
            prepared.contribution.backends.clone(),
            self.tokio.clone(),
        ));
        let mut policy = Policy::new().with_application(&prepared.contribution.view_key);
        if prepared
            .contribution
            .modules
            .contains(&extension_runtime::extension::manifest::ShellHostModule::Context)
        {
            policy = policy.with_host_module(context_module(&prepared.contribution))?;
        }
        if prepared
            .contribution
            .modules
            .contains(&extension_runtime::extension::manifest::ShellHostModule::Resource)
        {
            policy = policy.with_host_module(resource_module(session))?;
        }
        let options = ViewLoadOptions::new(
            &prepared.contribution.extension_root,
            entry_relative_path(&prepared.contribution)?,
            Rc::new(policy),
        );
        self.runtime.load_view(options, window, cx)
    }

    pub(crate) fn release(&self, activations: Vec<ActivationHandle>) {
        if activations.is_empty() {
            return;
        }
        let service = self.service.clone();
        self.tokio.spawn(async move {
            release_activations(&service, activations).await;
        });
    }

    pub(crate) fn release_task(
        &self,
        activations: Vec<ActivationHandle>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if activations.is_empty() {
            return None;
        }
        let service = self.service.clone();
        Some(self.tokio.spawn(async move {
            release_activations(&service, activations).await;
        }))
    }

    pub(crate) fn service(&self) -> UniversalPluginService {
        self.service.clone()
    }

    pub(crate) fn start_monitor_bridge(&self, cx: &mut App) {
        let (sender, receiver) = smol::channel::bounded::<String>(32);
        let mut events = self.service.subscribe();
        self.tokio.spawn(async move {
            while let Ok(event) = events.recv().await {
                let runtime_id = match event {
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
                    } => runtime_id,
                };
                if sender.send(runtime_id).await.is_err() {
                    break;
                }
            }
        });
        let tabs = Rc::clone(&self.tabs);
        cx.spawn(async move |cx| {
            while let Ok(runtime_id) = receiver.recv().await {
                let tracked = tabs
                    .borrow()
                    .values()
                    .flatten()
                    .cloned()
                    .collect::<Vec<_>>();
                for tab in tracked {
                    let _ = tab
                        .tab
                        .update(cx, |tab, cx| tab.runtime_changed(&runtime_id, cx));
                }
            }
        })
        .detach();
    }

    fn register_tab(
        &self,
        extension_id: String,
        view_key: String,
        tab: WeakEntity<ShellPluginTab>,
    ) {
        self.tabs
            .borrow_mut()
            .entry(extension_id)
            .or_default()
            .push(TrackedShellTab { view_key, tab });
    }
}

async fn release_activations(service: &UniversalPluginService, activations: Vec<ActivationHandle>) {
    for activation in activations {
        let _ = service.deactivate_activation(&activation).await;
    }
}

impl extension_view::ShellViewOpener for ShellPluginHost {
    fn open(&self, extension_id: &str, view_id: &str, window: &mut Window, cx: &mut App) {
        if self.retiring.borrow().contains(extension_id) {
            tracing::warn!(extension_id, view_id, "extension is retiring");
            return;
        }
        let Some(contribution) = self.contribution(extension_id, view_id) else {
            tracing::warn!(extension_id, view_id, "shell view contribution not found");
            return;
        };
        if contribution.singleton
            && self.tabs.borrow().get(extension_id).is_some_and(|tabs| {
                tabs.iter()
                    .any(|tab| tab.view_key == contribution.view_key && tab.tab.upgrade().is_some())
            })
        {
            tracing::info!(
                extension_id,
                view_id,
                "singleton shell view is already open"
            );
            return;
        }
        let tab_id = if contribution.singleton {
            format!("shell:{}", contribution.view_key)
        } else {
            format!(
                "shell:{}:{}",
                contribution.view_key,
                NEXT_SHELL_TAB_ID.fetch_add(1, Ordering::Relaxed)
            )
        };
        let host = self.clone();
        let extension_key = extension_id.to_string();
        let view_key = contribution.view_key.clone();
        let tab_container = cx.global::<GlobalTabContainer>().primary_pane();
        tab_container.update(cx, |tabs, cx| {
            tabs.activate_or_add_tab_lazy(
                tab_id.clone(),
                move |window, cx| {
                    let registry_host = host.clone();
                    let view = ShellPluginTab::load(host, contribution, window, cx);
                    registry_host.register_tab(extension_key, view_key, view.downgrade());
                    TabItem::new(tab_id, format!("shell:{extension_id}"), view)
                },
                window,
                cx,
            );
        });
    }

    fn close_extension(
        &self,
        extension_id: &str,
        window: &mut Window,
        cx: &mut App,
    ) -> gpui::Task<bool> {
        let _ = window;
        self.service.begin_extension_retire(extension_id);
        self.retiring.borrow_mut().insert(extension_id.to_string());
        let tabs = self
            .tabs
            .borrow()
            .get(extension_id)
            .cloned()
            .unwrap_or_default();
        let tasks = tabs
            .into_iter()
            .filter_map(|tab| tab.tab.upgrade())
            .map(|tab| tab.update(cx, |tab, cx| tab.close_for_extension(cx)))
            .collect::<Vec<_>>();
        let service = self.service.clone();
        let extension_id = extension_id.to_string();
        let retiring = Rc::clone(&self.retiring);
        cx.spawn(async move |cx| {
            for task in tasks {
                if !task.await {
                    retiring.borrow_mut().remove(&extension_id);
                    service.finish_extension_retire(&extension_id);
                    return false;
                }
            }
            let stop = one_core::gpui_tokio::Tokio::spawn_result(cx, {
                let extension_id = extension_id.clone();
                let service = service.clone();
                async move {
                    service.deactivate_extension(&extension_id).await;
                    Ok(())
                }
            });
            let stopped = stop.await.is_ok();
            if !stopped {
                retiring.borrow_mut().remove(&extension_id);
                service.finish_extension_retire(&extension_id);
                return false;
            }
            true
        })
    }

    fn finish_extension_change(&self, extension_id: &str) {
        self.retiring.borrow_mut().remove(extension_id);
        self.service.finish_extension_retire(extension_id);
    }
}

fn validate_entry_path(
    contribution: &extension_runtime::RegisteredShellViewContribution,
) -> Result<()> {
    let root = contribution.extension_root.canonicalize()?;
    let entry = contribution.entry_path.canonicalize()?;
    if !entry.starts_with(root) {
        anyhow::bail!("shell entry escaped extension root");
    }
    Ok(())
}

fn entry_relative_path(
    contribution: &extension_runtime::RegisteredShellViewContribution,
) -> Result<&str> {
    contribution
        .entry_path
        .strip_prefix(&contribution.extension_root)?
        .to_str()
        .ok_or_else(|| anyhow!("shell entry path is not UTF-8"))
}
