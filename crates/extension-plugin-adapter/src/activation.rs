//! Host-authoritative lazy activation for Declarative UI panels.
//!
//! This layer owns provider process sessions and activation references. It does
//! not mount a GPUI panel or create a native window; those host UI operations
//! are performed by a later integration layer after activation succeeds.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use extension_host::{HostError, ProcessRpcSession};
use extension_runtime::{
    ExtensionRuntimeCatalog, RegisteredIpcRuntimeBinding, extension::manifest::current_host_version,
};
use futures::FutureExt;
use futures::future::BoxFuture;
use parking_lot::Mutex as SyncMutex;
use tokio::sync::Mutex;

use thiserror::Error;

/// Asynchronous factory used to start a runtime session.
///
/// The indirection keeps process supervision independently testable. The
/// production implementation is [`process_session_factory`]; restart policy and
/// process monitoring belong to a later supervision layer.
pub type SessionFactory = Arc<
    dyn Fn(
            RegisteredIpcRuntimeBinding,
        ) -> BoxFuture<'static, Result<Arc<dyn ManagedRpcSession>, HostError>>
        + Send
        + Sync,
>;

/// The process-session capability required by the activation manager.
///
/// `ProcessRpcSession` is the only production implementation. The trait exists
/// so ownership semantics can be tested without spawning a child process.
pub trait ManagedRpcSession: Send + Sync {
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()>;
}

impl ManagedRpcSession for ProcessRpcSession {
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        ProcessRpcSession::shutdown(self).boxed()
    }
}

/// Build the production IPC session factory for native resource providers.
///
/// Each call receives a fresh instance ID, negotiates the extension API, and
/// starts one `ProcessRpcSession`. Restart/backoff supervision is intentionally
/// not part of this factory; it belongs to the later supervision layer.
pub fn process_session_factory() -> SessionFactory {
    Arc::new(|binding| {
        Box::pin(async move {
            let host_version = current_host_version().to_string();
            let instance_id = uuid::Uuid::new_v4().to_string();
            let negotiation = extension_host::NegotiationConfig::new(host_version, instance_id)
                .offer_api("extension", "1.0");
            let config = crate::process_session_config(&binding, negotiation)
                .map_err(|error| HostError::Config(error.to_string()))?;
            let session = ProcessRpcSession::start(config).await?;
            Ok(Arc::new(session) as Arc<dyn ManagedRpcSession>)
        })
    })
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ActivationError {
    #[error("declarative panel `{panel_key}` is not registered")]
    PanelNotFound { panel_key: String },
    #[error("IPC runtime `{runtime_id}` is not registered")]
    RuntimeNotFound { runtime_id: String },
    #[error(
        "runtime `{runtime_id}` is owned by extension `{extension_id}` and cannot serve panel `{panel_key}`"
    )]
    OwnerMismatch {
        extension_id: String,
        runtime_id: String,
        panel_key: String,
    },
    #[error(
        "registered declarative panel `{panel_key}` refers to unsupported runtime `{runtime_id}`"
    )]
    UnsupportedRuntime {
        panel_key: String,
        runtime_id: String,
    },
    #[error("failed to activate runtime: {0}")]
    SessionStart(String),
    #[error("failed to activate runtime `{runtime_id}`")]
    InvalidRuntime { runtime_id: String },
}

impl From<HostError> for ActivationError {
    fn from(error: HostError) -> Self {
        Self::SessionStart(error.to_string())
    }
}

impl ActivationError {
    fn session_start(error: HostError) -> Self {
        Self::SessionStart(error.to_string())
    }
}

/// The result of a successful panel activation.
///
/// This value describes ownership and is safe to copy into a UI entity. It does
/// not own or control the provider process; shutdown remains manager-owned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationHandle {
    pub extension_id: String,
    pub panel_key: String,
    pub runtime_id: String,
    pub state: RuntimeActivationState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeActivationState {
    Starting,
    Active,
}

struct ActivatedRuntime {
    extension_id: String,
    panels: BTreeSet<String>,
    state: RuntimeActivationState,
    session: Option<Arc<dyn ManagedRpcSession>>,
    start_generation: u64,
    factory_claimed: bool,
}

#[derive(Default)]
struct ActivationState {
    runtimes: BTreeMap<String, ActivatedRuntime>,
    start_locks: BTreeMap<String, Arc<Mutex<()>>>,
    deactivations: BTreeMap<String, u64>,
}

struct StartingRuntime {
    binding: RegisteredIpcRuntimeBinding,
    generation: u64,
}

/// Manages lazy runtime starts, runtime sharing across panels, and reference-counted shutdown.
pub struct ActivationManager {
    catalog: ExtensionRuntimeCatalog,
    session_factory: SessionFactory,
    state: SyncMutex<ActivationState>,
}

impl ActivationManager {
    pub fn new(catalog: ExtensionRuntimeCatalog, session_factory: SessionFactory) -> Self {
        Self {
            catalog,
            session_factory,
            state: SyncMutex::new(ActivationState::default()),
        }
    }

    pub async fn activate_panel(
        &self,
        panel_key: &str,
    ) -> Result<ActivationHandle, ActivationError> {
        let (binding, runtime_id, generation) = {
            let mut state = self.state.lock();
            let matching: Vec<_> = self
                .catalog
                .declarative_panels()
                .iter()
                .filter(|panel| panel.panel_key == panel_key)
                .collect();
            let panel =
                matching
                    .first()
                    .copied()
                    .ok_or_else(|| ActivationError::PanelNotFound {
                        panel_key: panel_key.to_owned(),
                    })?;
            if matching.len() != 1 {
                return Err(ActivationError::InvalidRuntime {
                    runtime_id: panel_key.to_owned(),
                });
            }

            let binding = self
                .catalog
                .ipc_runtime_bindings()
                .find(|binding| binding.runtime_key == panel.runtime_id)
                .ok_or_else(|| ActivationError::UnsupportedRuntime {
                    panel_key: panel_key.to_owned(),
                    runtime_id: panel.runtime_id.clone(),
                })?;
            if binding.extension_id != panel.extension_id {
                return Err(ActivationError::OwnerMismatch {
                    extension_id: binding.extension_id.clone(),
                    runtime_id: panel.runtime_id.clone(),
                    panel_key: panel_key.to_owned(),
                });
            }

            if let Some(runtime) = state.runtimes.get_mut(&panel.runtime_id) {
                runtime.panels.insert(panel_key.to_owned());
                return Ok(ActivationHandle {
                    extension_id: panel.extension_id.clone(),
                    panel_key: panel_key.to_owned(),
                    runtime_id: panel.runtime_id.clone(),
                    state: runtime.state,
                });
            }

            let generation = state
                .deactivations
                .get(&panel.runtime_id)
                .map(|generation| generation + 1)
                .unwrap_or_default();
            state.runtimes.insert(
                panel.runtime_id.clone(),
                ActivatedRuntime {
                    extension_id: binding.extension_id.clone(),
                    panels: BTreeSet::from([panel_key.to_owned()]),
                    state: RuntimeActivationState::Starting,
                    session: None,
                    start_generation: generation,
                    factory_claimed: false,
                },
            );

            (binding.clone(), panel.runtime_id.clone(), generation)
        };

        let start_lock = {
            let mut state = self.state.lock();
            Arc::clone(state.start_locks.entry(runtime_id.clone()).or_default())
        };
        let _start_guard = start_lock.lock().await;

        let starting = {
            let mut state = self.state.lock();
            let Some(runtime) = state.runtimes.get_mut(&runtime_id) else {
                return Err(ActivationError::RuntimeNotFound {
                    runtime_id: runtime_id.clone(),
                });
            };
            if runtime.start_generation != generation {
                return Err(ActivationError::RuntimeNotFound {
                    runtime_id: runtime_id.clone(),
                });
            }
            if runtime.factory_claimed {
                runtime.panels.insert(panel_key.to_owned());
                return Ok(ActivationHandle {
                    extension_id: binding.extension_id.clone(),
                    panel_key: panel_key.to_owned(),
                    runtime_id: runtime_id.clone(),
                    state: runtime.state,
                });
            }
            runtime.factory_claimed = true;
            StartingRuntime {
                binding,
                generation,
            }
        };

        let session = (self.session_factory)(starting.binding.clone())
            .await
            .map_err(|error| {
                let mut state = self.state.lock();
                if let Some(runtime) = state.runtimes.get(&runtime_id) {
                    if runtime.start_generation == starting.generation
                        && runtime.state == RuntimeActivationState::Starting
                        && runtime.session.is_none()
                    {
                        state.runtimes.remove(&runtime_id);
                    }
                }
                ActivationError::session_start(error)
            })?;

        let stale_session: Option<Arc<dyn ManagedRpcSession>> = {
            let mut state = self.state.lock();
            if let Some(runtime) = state.runtimes.get_mut(&runtime_id)
                && runtime.start_generation == starting.generation
            {
                runtime.panels.insert(panel_key.to_owned());
                runtime.state = RuntimeActivationState::Active;
                runtime.session.replace(session)
            } else {
                Some(session)
            }
        };
        if let Some(session) = stale_session {
            session.shutdown().await;
            return Err(ActivationError::RuntimeNotFound {
                runtime_id: runtime_id.clone(),
            });
        }

        Ok(ActivationHandle {
            extension_id: starting.binding.extension_id.clone(),
            panel_key: panel_key.to_owned(),
            runtime_id,
            state: RuntimeActivationState::Active,
        })
    }

    pub async fn deactivate_panel(&self, panel_key: &str) -> Result<(), ActivationError> {
        let session =
            {
                let mut state = self.state.lock();
                let Some(runtime_id) = state.runtimes.iter().find_map(|(id, runtime)| {
                    runtime.panels.contains(panel_key).then(|| id.clone())
                }) else {
                    return Ok(());
                };

                let runtime = state.runtimes.get_mut(&runtime_id).ok_or_else(|| {
                    ActivationError::RuntimeNotFound {
                        runtime_id: runtime_id.clone(),
                    }
                })?;
                runtime.panels.remove(panel_key);
                if !runtime.panels.is_empty() {
                    return Ok(());
                }

                let generation = runtime.start_generation;
                let is_starting = runtime.state == RuntimeActivationState::Starting;
                let session = runtime.session.take();
                if runtime.state == RuntimeActivationState::Starting {
                    state.deactivations.insert(runtime_id.clone(), generation);
                    state.runtimes.remove(&runtime_id);
                }
                if !is_starting {
                    state.runtimes.remove(&runtime_id);
                }
                session
            };
        if let Some(session) = session {
            session.shutdown().await;
        }
        Ok(())
    }

    pub async fn deactivate_runtime(&self, runtime_id: &str) -> Result<(), ActivationError> {
        let runtime = {
            let mut state = self.state.lock();
            let runtime = state.runtimes.remove(runtime_id);
            if let Some(runtime) = &runtime {
                state
                    .deactivations
                    .insert(runtime_id.to_owned(), runtime.start_generation);
            }
            runtime
        };
        if let Some(runtime) = runtime {
            if let Some(session) = runtime.session {
                session.shutdown().await;
            }
        }
        Ok(())
    }

    pub async fn deactivate_extension(&self, extension_id: &str) -> Result<(), ActivationError> {
        let removed = {
            let mut state = self.state.lock();
            let keys: Vec<String> = state
                .runtimes
                .iter()
                .filter(|(_, runtime)| runtime.extension_id == extension_id)
                .map(|(key, _)| key.clone())
                .collect();
            let deactivations = keys
                .iter()
                .map(|key| {
                    let generation = state.runtimes[key].start_generation;
                    (key.clone(), generation)
                })
                .collect::<Vec<_>>();
            for (key, generation) in deactivations {
                state.deactivations.insert(key, generation);
            }
            keys.into_iter()
                .filter_map(|key| state.runtimes.remove(&key))
                .collect::<Vec<_>>()
        };
        for runtime in removed {
            if let Some(session) = runtime.session {
                session.shutdown().await;
            }
        }
        Ok(())
    }

    pub fn runtime_state(
        &self,
        runtime_id: &str,
    ) -> Result<RuntimeActivationState, ActivationError> {
        self.state
            .lock()
            .runtimes
            .get(runtime_id)
            .map(|runtime| runtime.state)
            .ok_or(ActivationError::RuntimeNotFound {
                runtime_id: runtime_id.to_owned(),
            })
    }

    pub fn active_panel_keys(&self) -> BTreeSet<String> {
        self.state
            .lock()
            .runtimes
            .values()
            .flat_map(|runtime| runtime.panels.iter().cloned())
            .collect()
    }
}
