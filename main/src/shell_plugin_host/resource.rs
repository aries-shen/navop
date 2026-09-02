use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::Result;
use extension_host::{CancellationToken, RequestOptions};
use extension_plugin_adapter::ManagedUniversalPluginClient;
use extension_protocol::resource::{
    ResourceCloseParams, ResourceInvokeParams, ResourceOpenParams, ResourcePingParams,
};
use gpui_shell::{HostAsyncTask, HostError, HostModule, HostObject, HostValue};

use self::task::{host_error, spawn_provider_task};
use super::value::{host_to_json, json_to_host, result_ref_to_host};
use crate::universal_plugins::UniversalPluginService;

pub(super) struct ShellResourceSession {
    service: UniversalPluginService,
    backends: BTreeMap<String, String>,
    resources: Mutex<HashMap<String, (String, u64, String)>>,
    next_resource: AtomicU64,
    tokio: tokio::runtime::Handle,
}

impl ShellResourceSession {
    pub(super) fn new(
        service: UniversalPluginService,
        backends: BTreeMap<String, String>,
        tokio: tokio::runtime::Handle,
    ) -> Self {
        Self {
            service,
            backends,
            resources: Mutex::new(HashMap::new()),
            next_resource: AtomicU64::new(1),
            tokio,
        }
    }

    fn client(&self, alias: &str) -> Result<ManagedUniversalPluginClient, HostError> {
        let runtime_id = self
            .backends
            .get(alias)
            .ok_or_else(|| HostError::new(format!("unknown backend alias `{alias}`")))?;
        self.service
            .universal_plugin_client(runtime_id)
            .map_err(|error| HostError::new(error.to_string()))
    }

    fn insert_resource(&self, alias: String, generation: u64, provider_id: String) -> String {
        let id = format!(
            "resource-{}",
            self.next_resource.fetch_add(1, Ordering::Relaxed)
        );
        self.resources
            .lock()
            .expect("shell resource registry poisoned")
            .insert(id.clone(), (alias, generation, provider_id));
        id
    }

    fn resource(&self, handle: &str) -> Result<(ManagedUniversalPluginClient, String), HostError> {
        let resources = self
            .resources
            .lock()
            .map_err(|_| HostError::new("shell resource registry poisoned"))?;
        let (alias, generation, provider_id) = resources
            .get(handle)
            .ok_or_else(|| HostError::new(format!("unknown resource handle `{handle}`")))?;
        let client = self.client(alias)?;
        if client.generation != *generation {
            return Err(HostError::new(format!(
                "stale resource handle `{handle}` after provider restart"
            )));
        }
        Ok((client, provider_id.clone()))
    }

    fn resource_for_close(
        &self,
        handle: &str,
    ) -> Result<Option<(ManagedUniversalPluginClient, String)>, HostError> {
        let record = self
            .resources
            .lock()
            .map_err(|_| HostError::new("shell resource registry poisoned"))?
            .get(handle)
            .cloned()
            .ok_or_else(|| HostError::new(format!("unknown resource handle `{handle}`")))?;
        let (alias, generation, provider_id) = record;
        let client = match self.client(&alias) {
            Ok(client) => client,
            Err(_) => return Ok(None),
        };
        if client.generation != generation {
            self.forget_resource(handle, &provider_id);
            return Ok(None);
        }
        Ok(Some((client, provider_id)))
    }

    fn forget_resource(&self, handle: &str, provider_id: &str) {
        if let Ok(mut resources) = self.resources.lock()
            && resources
                .get(handle)
                .is_some_and(|(_, _, current_id)| current_id == provider_id)
        {
            resources.remove(handle);
        }
    }
}

pub(super) fn resource_module(session: Arc<ShellResourceSession>) -> HostModule {
    HostModule::new("navop.resource")
        .declarations(
            r#"
            export function open(backend: string, resourceType: string, config: unknown): Promise<{ handle: string; capabilities: string[]; metadata: unknown }>;
            export function invoke(handle: string, method: string, params?: unknown): Promise<unknown>;
            export function ping(handle: string): Promise<void>;
            export function close(handle: string): Promise<void>;
            "#,
        )
        .cancellable_async_function("open", open_resource(Arc::clone(&session)))
        .cancellable_async_function("invoke", invoke_resource(Arc::clone(&session)))
        .cancellable_async_function("ping", ping_resource(Arc::clone(&session)))
        .cancellable_async_function("close", close_resource(session))
}

fn open_resource(
    session: Arc<ShellResourceSession>,
) -> impl Fn(&gpui_shell::HostArguments) -> Result<HostAsyncTask, HostError> {
    move |arguments| {
        let alias = arguments.string(0)?.to_owned();
        let resource_type = arguments.string(1)?.to_owned();
        let config = host_to_json(arguments.value(2)?)?;
        let client = session.client(&alias)?;
        let tokio = session.tokio.clone();
        let cancel = CancellationToken::new();
        let result_cancel = cancel.clone();
        let session = Arc::clone(&session);
        Ok(spawn_provider_task(
            &tokio,
            async move {
                let result = client
                    .client()
                    .open_resource_with_options(
                        &ResourceOpenParams {
                            resource_type,
                            config,
                            metadata: None,
                        },
                        RequestOptions::default().with_cancel(result_cancel.clone()),
                    )
                    .await
                    .map_err(host_error)?;
                if result_cancel.is_cancelled() {
                    compensate_cancelled_open(&session, &client, alias, result.resource_id).await;
                    return Err(HostError::new("resource open cancelled"));
                }
                let handle = session.insert_resource(alias, client.generation, result.resource_id);
                return Ok(HostObject::new()
                    .field("handle", handle)
                    .field("capabilities", result.capabilities)
                    .field(
                        "metadata",
                        result.metadata.as_ref().map(json_to_host).transpose()?,
                    )
                    .into());
            },
            cancel,
        ))
    }
}

async fn compensate_cancelled_open(
    session: &ShellResourceSession,
    client: &ManagedUniversalPluginClient,
    alias: String,
    resource_id: String,
) {
    if client
        .client()
        .close_resource(&ResourceCloseParams {
            resource_id: resource_id.clone(),
        })
        .await
        .is_err()
    {
        session.insert_resource(alias, client.generation, resource_id);
    }
}

fn invoke_resource(
    session: Arc<ShellResourceSession>,
) -> impl Fn(&gpui_shell::HostArguments) -> Result<HostAsyncTask, HostError> {
    move |arguments| {
        let handle = arguments.string(0)?.to_owned();
        let method = arguments.string(1)?.to_owned();
        let params = arguments
            .get(2)
            .map(host_to_json)
            .transpose()?
            .unwrap_or(serde_json::Value::Null);
        let (client, resource_id) = session.resource(&handle)?;
        let tokio = session.tokio.clone();
        let cancel = CancellationToken::new();
        let request_cancel = cancel.clone();
        Ok(spawn_provider_task(
            &tokio,
            async move {
                let result = client
                    .client()
                    .invoke_resource_with_options(
                        &ResourceInvokeParams {
                            resource_id,
                            method,
                            params,
                        },
                        RequestOptions::default().with_cancel(request_cancel),
                    )
                    .await
                    .map_err(host_error)?;
                result_ref_to_host(&result.result)
            },
            cancel,
        ))
    }
}

fn ping_resource(
    session: Arc<ShellResourceSession>,
) -> impl Fn(&gpui_shell::HostArguments) -> Result<HostAsyncTask, HostError> {
    move |arguments| {
        let (client, resource_id) = session.resource(arguments.string(0)?)?;
        let tokio = session.tokio.clone();
        let cancel = CancellationToken::new();
        let request_cancel = cancel.clone();
        Ok(spawn_provider_task(
            &tokio,
            async move {
                if request_cancel.is_cancelled() {
                    return Err(HostError::new("resource ping cancelled"));
                }
                client
                    .client()
                    .ping_resource_with_options(
                        &ResourcePingParams { resource_id },
                        RequestOptions::default().with_cancel(request_cancel),
                    )
                    .await
                    .map_err(host_error)?;
                Ok(HostValue::Null)
            },
            cancel,
        ))
    }
}

fn close_resource(
    session: Arc<ShellResourceSession>,
) -> impl Fn(&gpui_shell::HostArguments) -> Result<HostAsyncTask, HostError> {
    move |arguments| {
        let handle = arguments.string(0)?.to_owned();
        let Some((client, resource_id)) = session.resource_for_close(&handle)? else {
            return Ok(spawn_provider_task(
                &session.tokio,
                async { Ok(HostValue::Null) },
                CancellationToken::new(),
            ));
        };
        let tokio = session.tokio.clone();
        let cancel = CancellationToken::new();
        let request_cancel = cancel.clone();
        let session = Arc::clone(&session);
        Ok(spawn_provider_task(
            &tokio,
            async move {
                if request_cancel.is_cancelled() {
                    return Err(HostError::new("resource close cancelled"));
                }
                client
                    .client()
                    .close_resource_with_options(
                        &ResourceCloseParams {
                            resource_id: resource_id.clone(),
                        },
                        RequestOptions::default().with_cancel(request_cancel),
                    )
                    .await
                    .map_err(host_error)?;
                session.forget_resource(&handle, &resource_id);
                Ok(HostValue::Null)
            },
            cancel,
        ))
    }
}

mod task;
