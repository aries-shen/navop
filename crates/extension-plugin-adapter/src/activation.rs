//! Host-authoritative lazy activation for Declarative UI panels.
//!
//! This layer owns provider process sessions and activation references. It does
//! not mount a GPUI panel or create a native window; those host UI operations
//! are performed by a later integration layer after activation succeeds.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::Arc,
    time::Duration,
};

use crate::provider_permissions::ResourceOpenAuthorizer;
use extension_host::{HostError, ProcessRpcSession};
use extension_runtime::extension::manifest::DeclarativePanelPlacement;
use extension_runtime::{
    ExtensionRuntimeCatalog, RegisteredIpcRuntimeBinding, extension::manifest::current_host_version,
};
use futures::FutureExt;
use futures::future::BoxFuture;
use parking_lot::Mutex as SyncMutex;
use tokio::{
    sync::{Mutex, Notify, broadcast, mpsc},
    time::{Instant, sleep},
};

use thiserror::Error;

/// Asynchronous factory used to start a runtime session.
///
/// The indirection keeps process supervision independently testable. The
/// production implementation is [`process_session_factory`].
pub type SessionFactory = Arc<
    dyn Fn(SessionContext) -> BoxFuture<'static, Result<Arc<dyn ManagedRpcSession>, HostError>>
        + Send
        + Sync,
>;

/// Inputs needed to start one activation-owned runtime session.
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub binding: RegisteredIpcRuntimeBinding,
    pub host_api: Arc<extension_host::HostApiHandler>,
}

/// Creates permission-enforcing reverse Host API dispatchers for activations.
pub type HostApiFactory =
    Arc<dyn Fn(RegisteredIpcRuntimeBinding) -> Arc<extension_host::HostApiHandler> + Send + Sync>;

/// The process-session capability required by the activation manager.
///
/// `ProcessRpcSession` is the only production implementation. The trait exists
/// so ownership semantics can be tested without spawning a child process.
pub trait ManagedRpcSession: Send + Sync {
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()>;

    /// Reports whether the transport or child process has already exited.
    fn is_closed(&self) -> bool {
        false
    }

    /// Performs a process-level health request.
    ///
    /// The default is useful for in-process test doubles. A failed ping does
    /// not by itself prove that a process crashed; supervision only restarts
    /// after `is_closed` confirms closure.
    fn ping<'a>(&'a self) -> BoxFuture<'a, Result<(), HostError>> {
        async {
            if self.is_closed() {
                Err(HostError::Closed)
            } else {
                Ok(())
            }
        }
        .boxed()
    }

    fn universal_plugin_client(
        &self,
        _open_authorizer: Option<extension_host::OpenAuthorizer>,
    ) -> Option<extension_host::UniversalPluginClient> {
        None
    }
}

impl ManagedRpcSession for ProcessRpcSession {
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        ProcessRpcSession::shutdown(self).boxed()
    }

    fn universal_plugin_client(
        &self,
        open_authorizer: Option<extension_host::OpenAuthorizer>,
    ) -> Option<extension_host::UniversalPluginClient> {
        let session = Arc::new(self.clone_session());
        let client = extension_host::UniversalPluginClient::new(session);
        Some(match open_authorizer {
            Some(authorizer) => client.with_open_authorizer(authorizer),
            None => client,
        })
    }

    fn is_closed(&self) -> bool {
        ProcessRpcSession::is_closed(self)
    }

    fn ping<'a>(&'a self) -> BoxFuture<'a, Result<(), HostError>> {
        if !self.declares_method(extension_protocol::method::PING) {
            return async { Ok(()) }.boxed();
        }
        ProcessRpcSession::request_value(
            self,
            extension_protocol::method::PING,
            serde_json::Value::Null,
        )
        .map(|result| result.map(|_| ()))
        .boxed()
    }
}

/// Build the production IPC session factory for native resource providers.
///
/// Each call receives a fresh instance ID, negotiates the extension API, and
/// starts one `ProcessRpcSession`. Restart/backoff supervision is owned by
/// [`ActivationManager`] and remains host-authoritative.
pub fn process_session_factory() -> SessionFactory {
    Arc::new(|context| {
        Box::pin(async move {
            let host_version = current_host_version().to_string();
            let instance_id = uuid::Uuid::new_v4().to_string();
            let negotiation = extension_host::NegotiationConfig::new(host_version, instance_id)
                .offer_api("extension", "1.0");
            let config = crate::process_session_config(&context.binding, negotiation)
                .map_err(|error| HostError::Config(error.to_string()))?;
            let config = config.with_host_api(context.host_api);
            let session = ProcessRpcSession::start(config).await?;
            Ok(Arc::new(session) as Arc<dyn ManagedRpcSession>)
        })
    })
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
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
    #[error("runtime `{runtime_id}` has no active session")]
    RuntimeNotReady { runtime_id: String },
}

/// Host-owned restart timing policy.
///
/// Extension manifests choose whether restart is enabled and how many restarts
/// are budgeted; timing remains host-authoritative and cannot be configured by
/// an extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupervisionPolicy {
    pub initial_restart_backoff: Duration,
    pub max_restart_backoff: Duration,
    pub backoff_multiplier: u32,
}

impl Default for SupervisionPolicy {
    fn default() -> Self {
        Self {
            initial_restart_backoff: Duration::from_millis(250),
            max_restart_backoff: Duration::from_secs(8),
            backoff_multiplier: 2,
        }
    }
}

impl SupervisionPolicy {
    fn backoff_for_attempt(&self, attempt: u32) -> Duration {
        let mut backoff = self.initial_restart_backoff;
        for _ in 1..attempt {
            backoff = backoff
                .checked_mul(self.backoff_multiplier)
                .unwrap_or(self.max_restart_backoff)
                .min(self.max_restart_backoff);
        }
        backoff
    }
}

/// A point-in-time supervision snapshot for an activated runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeHealth {
    pub state: RuntimeActivationState,
    pub session_closed: bool,
    pub ping_error: Option<String>,
    pub restart_attempts: u32,
    pub restart_budget: u32,
    pub restart_backoff_remaining: Option<Duration>,
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

/// A UI-facing, immutable catalog entry for a registered declarative panel.
///
/// This projection contains only rendering metadata. Activation authorization
/// and runtime ownership remain exclusively in [`ActivationManager`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclarativePanelDescriptor {
    pub extension_id: String,
    pub panel_key: String,
    pub title: String,
    pub runtime_id: String,
    pub placement: DeclarativePanelPlacement,
    pub icon: Option<String>,
}

impl<'a> From<&'a extension_runtime::RegisteredDeclarativePanel> for DeclarativePanelDescriptor {
    fn from(panel: &'a extension_runtime::RegisteredDeclarativePanel) -> Self {
        Self {
            extension_id: panel.extension_id.clone(),
            panel_key: panel.panel_key.clone(),
            title: panel.title.clone(),
            runtime_id: panel.runtime_id.clone(),
            placement: panel.placement,
            icon: panel.icon.clone(),
        }
    }
}

/// A typed client bound to one activation-owned session generation.
///
/// The generation lets callers detect that a supervisor has replaced a closed
/// process and reacquire a client instead of silently using the old transport.
#[derive(Clone)]
pub struct ManagedUniversalPluginClient {
    pub runtime_id: String,
    pub generation: u64,
    client: extension_host::UniversalPluginClient,
}

impl ManagedUniversalPluginClient {
    pub fn client(&self) -> &extension_host::UniversalPluginClient {
        &self.client
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeActivationState {
    Starting,
    Restarting,
    Active,
    /// The process transport is still open, but its health ping failed.
    Degraded,
    /// Restart is disabled or a restart could not be initiated.
    Failed,
    /// The configured restart budget has been exhausted.
    CrashLoop,
}

struct ActivatedRuntime {
    extension_id: String,
    panels: BTreeSet<String>,
    state: RuntimeActivationState,
    session: Option<Arc<dyn ManagedRpcSession>>,
    start_generation: u64,
    factory_claimed: bool,
    restart_attempts: u32,
    next_restart_at: Option<Instant>,
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

enum CheckDecision {
    Return(RuntimeHealth, Option<Arc<dyn ManagedRpcSession>>),
    Restart {
        binding: Box<RegisteredIpcRuntimeBinding>,
        generation: u64,
        attempt: u32,
    },
}

/// Manages lazy runtime starts, runtime sharing across panels, and reference-counted shutdown.
pub struct ActivationManager {
    catalog: ExtensionRuntimeCatalog,
    session_factory: SessionFactory,
    host_api_factory: HostApiFactory,
    supervision_policy: SupervisionPolicy,
    state: SyncMutex<ActivationState>,
}

impl ActivationManager {
    pub fn new(
        catalog: ExtensionRuntimeCatalog,
        session_factory: SessionFactory,
        host_api_factory: HostApiFactory,
    ) -> Self {
        Self {
            catalog,
            session_factory,
            host_api_factory,
            supervision_policy: SupervisionPolicy::default(),
            state: SyncMutex::new(ActivationState::default()),
        }
    }

    pub fn with_supervision_policy(mut self, policy: SupervisionPolicy) -> Self {
        self.supervision_policy = policy;
        self
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
                    restart_attempts: 0,
                    next_restart_at: None,
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

        let context = SessionContext {
            binding: starting.binding.clone(),
            host_api: (self.host_api_factory)(starting.binding.clone()),
        };
        let session = (self.session_factory)(context).await.map_err(|error| {
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

    /// Returns a typed client for the current activated session generation.
    ///
    /// A client acquired here remains bound to that process generation. After
    /// a supervisor restart, compare `generation` and acquire a new client.
    pub fn universal_plugin_client(
        &self,
        runtime_id: &str,
    ) -> Result<ManagedUniversalPluginClient, ActivationError> {
        let binding = self
            .catalog
            .ipc_runtime_bindings()
            .find(|binding| binding.runtime_key == runtime_id)
            .cloned()
            .ok_or_else(|| ActivationError::RuntimeNotFound {
                runtime_id: runtime_id.to_owned(),
            })?;
        let authorizer = Arc::new(
            ResourceOpenAuthorizer::new(binding.permissions.iter().cloned()).into_host_authorizer(),
        );

        let (generation, session) =
            {
                let state = self.state.lock();
                let runtime = state.runtimes.get(runtime_id).ok_or_else(|| {
                    ActivationError::RuntimeNotFound {
                        runtime_id: runtime_id.to_owned(),
                    }
                })?;
                (
                    runtime.start_generation,
                    runtime
                        .session
                        .clone()
                        .ok_or_else(|| ActivationError::RuntimeNotReady {
                            runtime_id: runtime_id.to_owned(),
                        })?,
                )
            };
        if session.is_closed() {
            return Err(ActivationError::RuntimeNotReady {
                runtime_id: runtime_id.to_owned(),
            });
        }
        let client = session
            .universal_plugin_client(Some(authorizer))
            .ok_or_else(|| ActivationError::RuntimeNotReady {
                runtime_id: runtime_id.to_owned(),
            })?;

        Ok(ManagedUniversalPluginClient {
            runtime_id: runtime_id.to_owned(),
            generation,
            client,
        })
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

    /// Returns the current process generation for restart-aware client use.
    pub fn runtime_generation(&self, runtime_id: &str) -> Result<u64, ActivationError> {
        self.state
            .lock()
            .runtimes
            .get(runtime_id)
            .map(|runtime| runtime.start_generation)
            .ok_or_else(|| ActivationError::RuntimeNotFound {
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

    /// Returns a UI-facing projection of all registered declarative panels.
    ///
    /// Paths and activation permissions are intentionally omitted. The UI can
    /// display these entries, but cannot activate a runtime by bypassing
    /// [`ActivationManager::activate_panel`].
    pub fn declarative_panel_catalog(&self) -> Vec<DeclarativePanelDescriptor> {
        let mut panels: Vec<_> = self
            .catalog
            .declarative_panels()
            .iter()
            .map(DeclarativePanelDescriptor::from)
            .collect();
        panels.sort_by(|left, right| left.panel_key.cmp(&right.panel_key));
        panels
    }

    /// Inspect process health without changing process state.
    pub async fn runtime_health(&self, runtime_id: &str) -> Result<RuntimeHealth, ActivationError> {
        let session = self.active_session(runtime_id);
        let ping_error = match &session {
            Some(session) if !session.is_closed() => {
                session.ping().await.err().map(|error| error.to_string())
            }
            _ => None,
        };
        self.health_snapshot(runtime_id, ping_error).await
    }

    /// Check a runtime and restart it when process closure is confirmed.
    ///
    /// This method is deterministic and pull-oriented: the host UI or process
    /// monitor decides when to poll. Elapsed backoff is observed, but this
    /// method never sleeps before attempting a restart.
    pub async fn check_runtime(&self, runtime_id: &str) -> Result<RuntimeHealth, ActivationError> {
        let health = self.runtime_health(runtime_id).await?;
        if !health.session_closed {
            return Ok(health);
        }

        let start_lock = {
            let mut state = self.state.lock();
            Arc::clone(state.start_locks.entry(runtime_id.to_owned()).or_default())
        };
        let _start_guard = start_lock.lock().await;

        let decision: Result<CheckDecision, ActivationError> = {
            let mut state = self.state.lock();
            let runtime = state.runtimes.get_mut(runtime_id).ok_or_else(|| {
                ActivationError::RuntimeNotFound {
                    runtime_id: runtime_id.to_owned(),
                }
            })?;
            let binding = self
                .catalog
                .ipc_runtime_bindings()
                .find(|binding| binding.runtime_key == runtime_id)
                .cloned()
                .ok_or_else(|| ActivationError::RuntimeNotFound {
                    runtime_id: runtime_id.to_owned(),
                })?;
            let budget = self.binding_restart_budget(runtime_id)?;

            if !binding.auto_restart {
                runtime.state = RuntimeActivationState::Failed;
                let stale_session = runtime.session.take();
                let health = self.health_locked(&mut state, runtime_id);
                Ok(CheckDecision::Return(health, stale_session))
            } else if runtime.restart_attempts >= budget {
                runtime.state = RuntimeActivationState::CrashLoop;
                let stale_session = runtime.session.take();
                let health = self.health_locked(&mut state, runtime_id);
                Ok(CheckDecision::Return(health, stale_session))
            } else if let Some(restart_at) = runtime.next_restart_at
                && restart_at > Instant::now()
            {
                runtime.state = RuntimeActivationState::Restarting;
                Ok(CheckDecision::Return(
                    self.health_locked(&mut state, runtime_id),
                    None,
                ))
            } else {
                runtime.restart_attempts += 1;
                runtime.state = RuntimeActivationState::Restarting;
                runtime.factory_claimed = true;
                Ok(CheckDecision::Restart {
                    binding: Box::new(binding),
                    generation: runtime.start_generation,
                    attempt: runtime.restart_attempts,
                })
            }
        };

        match decision? {
            CheckDecision::Return(health, stale_session) => {
                if let Some(session) = stale_session {
                    session.shutdown().await;
                }
                return Ok(health);
            }
            CheckDecision::Restart {
                binding,
                generation,
                attempt,
            } => {
                let binding = *binding;
                let stale_session = self
                    .restart_runtime(runtime_id, binding, generation, attempt)
                    .await;
                if let Some(session) = stale_session {
                    session.shutdown().await;
                }
                self.health_snapshot(runtime_id, None).await
            }
        }
    }

    async fn restart_runtime(
        &self,
        runtime_id: &str,
        binding: RegisteredIpcRuntimeBinding,
        generation: u64,
        attempt: u32,
    ) -> Option<Arc<dyn ManagedRpcSession>> {
        let context = SessionContext {
            binding: binding.clone(),
            host_api: (self.host_api_factory)(binding.clone()),
        };
        let session = (self.session_factory)(context).await.ok();

        let mut stale_session = None;
        {
            let mut state = self.state.lock();
            if let Some(runtime) = state.runtimes.get_mut(runtime_id)
                && runtime.start_generation == generation
                && runtime.restart_attempts == attempt
            {
                let backoff = self.supervision_policy.backoff_for_attempt(attempt);
                runtime.next_restart_at = Some(Instant::now() + backoff);
                match session {
                    Some(session) => {
                        runtime.state = RuntimeActivationState::Active;
                        runtime.start_generation += 1;
                        runtime.factory_claimed = false;
                        stale_session = runtime.session.replace(session);
                    }
                    None => {
                        runtime.state = if attempt >= binding.max_restart_attempts {
                            RuntimeActivationState::CrashLoop
                        } else {
                            RuntimeActivationState::Restarting
                        };
                        runtime.factory_claimed = false;
                    }
                }
            } else if let Some(session) = session {
                stale_session = Some(session);
            }
        }
        stale_session
    }

    async fn health_snapshot(
        &self,
        runtime_id: &str,
        ping_error: Option<String>,
    ) -> Result<RuntimeHealth, ActivationError> {
        let session = self.active_session(runtime_id);
        let mut state = self.state.lock();
        let runtime =
            state
                .runtimes
                .get_mut(runtime_id)
                .ok_or_else(|| ActivationError::RuntimeNotFound {
                    runtime_id: runtime_id.to_owned(),
                })?;
        let session_closed = runtime
            .session
            .as_ref()
            .is_some_and(|session| session.is_closed())
            || (session.is_none() && runtime.state != RuntimeActivationState::Starting);
        let ping_error = if session_closed { None } else { ping_error };
        match (session_closed, ping_error.as_ref()) {
            (false, None) => {
                if runtime.state == RuntimeActivationState::Degraded {
                    runtime.state = RuntimeActivationState::Active;
                }
            }
            (false, Some(_)) => runtime.state = RuntimeActivationState::Degraded,
            (true, _) => {
                if runtime.state != RuntimeActivationState::Restarting
                    && runtime.state != RuntimeActivationState::Failed
                    && runtime.state != RuntimeActivationState::CrashLoop
                {
                    runtime.state = RuntimeActivationState::Failed;
                }
            }
        }
        Ok(self.health_locked_with_ping(&mut state, runtime_id, ping_error))
    }

    fn health_locked(&self, state: &mut ActivationState, runtime_id: &str) -> RuntimeHealth {
        self.health_locked_with_ping(state, runtime_id, None)
    }

    fn health_locked_with_ping(
        &self,
        state: &mut ActivationState,
        runtime_id: &str,
        ping_error: Option<String>,
    ) -> RuntimeHealth {
        let runtime = state
            .runtimes
            .get(runtime_id)
            .expect("caller checked runtime");
        let session_closed = runtime
            .session
            .as_ref()
            .is_some_and(|session| session.is_closed());
        let restart_backoff_remaining = runtime
            .next_restart_at
            .filter(|restart_at| *restart_at > Instant::now())
            .map(|restart_at| restart_at - Instant::now());
        RuntimeHealth {
            state: runtime.state,
            session_closed: session_closed || runtime.session.is_none(),
            ping_error,
            restart_attempts: runtime.restart_attempts,
            restart_budget: self
                .binding_restart_budget(runtime_id)
                .expect("runtime exists"),
            restart_backoff_remaining,
        }
    }

    fn active_session(&self, runtime_id: &str) -> Option<Arc<dyn ManagedRpcSession>> {
        self.state
            .lock()
            .runtimes
            .get(runtime_id)
            .and_then(|runtime| runtime.session.clone())
    }

    fn binding_restart_budget(&self, runtime_id: &str) -> Result<u32, ActivationError> {
        self.catalog
            .ipc_runtime_bindings()
            .find(|binding| binding.runtime_key == runtime_id)
            .map(|binding| binding.max_restart_attempts)
            .ok_or_else(|| ActivationError::RuntimeNotFound {
                runtime_id: runtime_id.to_owned(),
            })
    }

    #[cfg(test)]
    pub(super) fn clear_restart_backoff_for_test(&self, runtime_id: &str) {
        if let Some(runtime) = self.state.lock().runtimes.get_mut(runtime_id) {
            runtime.next_restart_at = None;
        }
    }
}

/// Configuration for the independent host-side runtime monitor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeMonitorConfig {
    pub check_interval: Duration,
}

impl Default for RuntimeMonitorConfig {
    fn default() -> Self {
        Self {
            check_interval: Duration::from_secs(5),
        }
    }
}

/// A state snapshot or removal notification emitted by [`RuntimeMonitor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeMonitorEvent {
    HealthChanged {
        runtime_id: String,
        health: RuntimeHealth,
    },
    RuntimeRemoved {
        runtime_id: String,
    },
    CheckFailed {
        runtime_id: String,
        error: ActivationError,
    },
}

/// Error returned when a monitor operation cannot be started.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RuntimeMonitorError {
    #[error("runtime monitor is already running")]
    AlreadyRunning,
}

/// Periodically invokes the pull-oriented activation supervisor.
///
/// The monitor deliberately owns no process state. It only schedules checks,
/// serializes checks per runtime, collapses repeated snapshots, and publishes
/// state transitions to consumers such as a GPUI activation status entity.
impl fmt::Debug for RuntimeMonitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeMonitor")
            .field("manager", &self.manager.catalog)
            .field("config", &self.config)
            .field("tracked_runtimes", &self.tracked_runtimes())
            .finish_non_exhaustive()
    }
}

pub struct RuntimeMonitor {
    manager: Arc<ActivationManager>,
    config: RuntimeMonitorConfig,
    monitor_state: Arc<SyncMutex<BTreeMap<String, Option<RuntimeHealth>>>>,
    running: SyncMutex<Option<mpsc::Sender<()>>>,
    stopped: Arc<Notify>,
    events: broadcast::Sender<RuntimeMonitorEvent>,
}

impl RuntimeMonitor {
    pub fn new(manager: Arc<ActivationManager>, config: RuntimeMonitorConfig) -> Self {
        let (events, _) = broadcast::channel(128);
        Self {
            manager,
            config,
            monitor_state: Arc::new(SyncMutex::new(BTreeMap::new())),
            running: SyncMutex::new(None),
            stopped: Arc::new(Notify::new()),
            events,
        }
    }

    /// Subscribes to health state transitions and runtime removal events.
    ///
    /// A receiver is returned even while the monitor is stopped; new
    /// subscriptions remain connected until their receiver is dropped.
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeMonitorEvent> {
        self.events.subscribe()
    }

    /// Tracks a runtime without performing an immediate health check.
    ///
    /// Tracking is intentionally separate from activation. A tracked runtime
    /// that has never been observed does not produce a removal event; once a
    /// health snapshot has been observed, disappearance from the manager emits
    /// `RuntimeRemoved` on the next cycle.
    pub fn track(&self, runtime_id: impl Into<String>) {
        self.monitor_state
            .lock()
            .entry(runtime_id.into())
            .or_insert(None);
    }

    /// Stops tracking a runtime. This does not deactivate its provider.
    pub fn untrack(&self, runtime_id: &str) {
        self.monitor_state.lock().remove(runtime_id);
    }

    /// Returns the currently tracked runtime IDs in deterministic order.
    pub fn tracked_runtimes(&self) -> BTreeSet<String> {
        self.monitor_state.lock().keys().cloned().collect()
    }

    /// Returns the last observed health snapshot for a tracked runtime.
    pub fn runtime_health(&self, runtime_id: &str) -> Option<RuntimeHealth> {
        self.monitor_state.lock().get(runtime_id).cloned().flatten()
    }

    /// Returns all last-observed health snapshots in deterministic order.
    pub fn runtime_healths(&self) -> BTreeMap<String, RuntimeHealth> {
        self.monitor_state
            .lock()
            .iter()
            .filter_map(|(runtime_id, health)| {
                health.clone().map(|health| (runtime_id.clone(), health))
            })
            .collect()
    }

    /// Runs the periodic monitor inline.
    ///
    /// This method is useful for deterministic tests and host runtimes that
    /// already own a scheduler. It performs at most one supervision cycle.
    pub async fn run_once(&self) {
        let runtime_ids: Vec<String> = self.monitor_state.lock().keys().cloned().collect();
        for runtime_id in runtime_ids {
            if let Some(event) = self.check_once(&runtime_id).await {
                let _ = self.events.send(event);
            }
        }
    }

    /// Starts one monitor task if no task is currently running.
    pub fn start(&self) -> Result<(), RuntimeMonitorError> {
        let mut running = self.running.lock();
        if running.is_some() {
            return Err(RuntimeMonitorError::AlreadyRunning);
        }
        let (stop_tx, mut stop_rx) = mpsc::channel(1);
        running.replace(stop_tx);
        drop(running);

        let manager = Arc::clone(&self.manager);
        let monitor_state = Arc::clone(&self.monitor_state);
        let events = self.events.clone();
        let stopped = Arc::clone(&self.stopped);
        let check_interval = self.config.check_interval;

        tokio::spawn(async move {
            loop {
                if monitor_should_stop(&mut stop_rx) {
                    break;
                }

                let runtime_ids: Vec<String> = monitor_state.lock().keys().cloned().collect();
                for runtime_id in runtime_ids {
                    // Do not cancel an in-flight supervision check. The manager
                    // and production ping requests are bounded, while monitor
                    // checks are serialized here to prevent overlap per runtime.
                    if let Some(event) =
                        check_runtime_event(&manager, &monitor_state, &runtime_id).await
                    {
                        let _ = events.send(event);
                    }
                }

                tokio::select! {
                    _ = stop_rx.recv() => break,
                    _ = sleep(check_interval) => {}
                }
            }

            stopped.notify_one();
        });

        Ok(())
    }

    /// Stops the monitor task and waits for it to acknowledge shutdown.
    pub async fn stop(&self) {
        let stop = self.running.lock().take();
        if let Some(stop) = stop {
            let _ = stop.send(()).await;
        }
        self.stopped.notified().await;
    }

    async fn check_once(&self, runtime_id: &str) -> Option<RuntimeMonitorEvent> {
        check_runtime_event(&self.manager, &self.monitor_state, runtime_id).await
    }
}

async fn check_runtime_event(
    manager: &ActivationManager,
    monitor_state: &SyncMutex<BTreeMap<String, Option<RuntimeHealth>>>,
    runtime_id: &str,
) -> Option<RuntimeMonitorEvent> {
    match manager.check_runtime(runtime_id).await {
        Ok(health) => {
            let mut state = monitor_state.lock();
            let last_health = state.entry(runtime_id.to_owned()).or_default();
            if last_health.as_ref() == Some(&health) {
                return None;
            }
            last_health.replace(health.clone());
            drop(state);
            Some(RuntimeMonitorEvent::HealthChanged {
                runtime_id: runtime_id.to_owned(),
                health,
            })
        }
        Err(ActivationError::RuntimeNotFound { .. }) => {
            let mut state = monitor_state.lock();
            let last_health = state.get_mut(runtime_id)?;
            last_health.take()?;
            drop(state);
            Some(RuntimeMonitorEvent::RuntimeRemoved {
                runtime_id: runtime_id.to_owned(),
            })
        }
        Err(error) => Some(RuntimeMonitorEvent::CheckFailed {
            runtime_id: runtime_id.to_owned(),
            error,
        }),
    }
}

fn monitor_should_stop(stop_rx: &mut mpsc::Receiver<()>) -> bool {
    match stop_rx.try_recv() {
        Ok(()) | Err(mpsc::error::TryRecvError::Disconnected) => true,
        Err(mpsc::error::TryRecvError::Empty) => false,
    }
}
