use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use declarative_ui_demo::NodePath;
use extension_host::{
    DEFAULT_SESSION_REQUEST_TIMEOUT, FramedTransport, JsonRpcClient, NegotiationConfig,
    ProcessRpcSession, ProcessRpcSessionConfig, SpawnConfig, SpawnTransport, UniversalPluginClient,
};
use extension_protocol::declarative_ui::{UiDialogKind, UiDialogRequest, UiDialogResult};
use extension_protocol::resource::ResourceInvokeParams;
use extension_runtime::ExtensionRuntimeCatalog;
use extension_runtime::extension::manifest::{DeclarativePanelPlacement, load_from_dir};
use futures::future::BoxFuture;
use tokio::io::duplex;
use tokio::sync::oneshot;

use super::*;

fn binding(extension_root: &std::path::Path) -> RegisteredIpcRuntimeBinding {
    let command = extension_root.join("bin/provider");
    std::fs::create_dir_all(command.parent().unwrap()).unwrap();
    std::fs::write(&command, b"provider").unwrap();
    RegisteredIpcRuntimeBinding {
        extension_id: "com.navop.kafka".into(),
        runtime_key: "com.navop.kafka::main".into(),
        extension_root: extension_root.to_path_buf(),
        command,
        required_spawn_permission: "spawn:./bin/provider".into(),
        args: vec!["--mode".into(), "extension".into()],
        working_dir: Some(extension_root.to_path_buf()),
        env: BTreeMap::from([("RUST_LOG".into(), "info".into())]),
        transport_kind: "local_socket".into(),
        connect_timeout_ms: Some(2_500),
        auto_restart: true,
        max_restart_attempts: 3,
        shutdown_grace_ms: 4_000,
        permissions: vec![
            "net:tcp:localhost:9092".into(),
            "spawn:./bin/provider".into(),
        ],
    }
}

#[derive(Debug)]
struct FakeManagedSession {
    shutdowns: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
}

struct FakeUniversalManagedSession {
    inner: Arc<ProcessRpcSession>,
}

impl ManagedRpcSession for FakeUniversalManagedSession {
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.inner.shutdown().await;
        })
    }

    fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }

    fn universal_plugin_client(
        &self,
        open_authorizer: Option<extension_host::OpenAuthorizer>,
    ) -> Option<UniversalPluginClient> {
        let client = UniversalPluginClient::new(Arc::new(self.inner.clone_session()));
        Some(match open_authorizer {
            Some(authorizer) => client.with_open_authorizer(authorizer),
            None => client,
        })
    }
}

async fn fake_universal_provider_session() -> Arc<ProcessRpcSession> {
    let (client_side, provider_side) = duplex(64 * 1024);
    let (client_reader, client_writer) = tokio::io::split(client_side);
    let (provider_reader, provider_writer) = tokio::io::split(provider_side);
    tokio::spawn(fake_universal_provider(provider_reader, provider_writer));

    let client = JsonRpcClient::start(FramedTransport::new(client_reader, client_writer));
    let config = ProcessRpcSessionConfig::new(
        SpawnConfig::new("fake-universal-provider"),
        NegotiationConfig::new("0.0.0-test", "activation-test").offer_api("extension", "1.0"),
    )
    .with_request_timeout(Duration::from_secs(5));
    Arc::new(
        ProcessRpcSession::start_with_client(client, None, config)
            .await
            .expect("start fake universal provider"),
    )
}

async fn fake_universal_provider<R, W>(mut reader: R, mut writer: W)
where
    R: extension_host::ReadFramed,
    W: extension_host::WriteFramed,
{
    use extension_host::transport::{recv_async, send_async};
    use extension_protocol::envelope::{Response, RpcMessage};
    use extension_protocol::lifecycle::InitResult;
    use extension_protocol::method;

    while let Ok(message) = recv_async::<_, RpcMessage>(&mut reader).await {
        let extension_protocol::envelope::RpcMessage::Request(request) = message else {
            continue;
        };
        let response = match request.method.as_str() {
            method::INIT => serde_json::to_value(
                InitResult::new("0.0.0-test")
                    .with_api("extension", "1.0")
                    .with_method(method::RESOURCE_INVOKE)
                    .with_method(method::JOB_START)
                    .with_method(method::JOB_STATUS)
                    .with_method(method::JOB_CANCEL)
                    .with_method(method::JOB_RESULT)
                    .with_method(method::JOB_CLOSE)
                    .with_method(method::EVENT_OPEN)
                    .with_method(method::EVENT_READ)
                    .with_method(method::EVENT_CLOSE),
            )
            .expect("serialize fake init"),
            method::RESOURCE_INVOKE => serde_json::json!({
                "result": {
                    "kind": "inline",
                    "value": {
                        "payload": "x".repeat(4 * 1024 * 1024 + 1)
                    }
                }
            }),
            method::JOB_START => serde_json::json!({
                "job_id": request.params
                    .get("params")
                    .and_then(|params| params.get("job_id"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("job-1"),
                "state": "queued"
            }),
            method::JOB_STATUS => serde_json::json!({
                "job_id": request.params.get("job_id").and_then(|value| value.as_str()).unwrap(),
                "state": "running",
                "progress_percent": 50,
                "message": "working"
            }),
            method::JOB_CANCEL => serde_json::Value::Null,
            method::JOB_CLOSE
                if request
                    .params
                    .get("job_id")
                    .and_then(|value| value.as_str())
                    == Some("job-close-error") =>
            {
                let error = extension_protocol::ProtocolError::new(
                    extension_protocol::error::error_codes::INTERNAL_ERROR,
                    "provider refused job close",
                );
                send_async(
                    &mut writer,
                    &RpcMessage::Response(Response::err(request.id, error)),
                )
                .await
                .expect("send fake universal provider close error");
                continue;
            }
            method::JOB_CLOSE => serde_json::Value::Null,
            method::JOB_RESULT => serde_json::json!({
                "result": {
                    "kind": "inline",
                    "value": if request.params.get("job_id").and_then(|value| value.as_str())
                        == Some("job-large")
                    {
                        serde_json::json!({"payload": "x".repeat(4 * 1024 * 1024 + 1)})
                    } else {
                        serde_json::json!({"job": "completed"})
                    }
                }
            }),
            method::EVENT_OPEN => serde_json::json!({
                "stream_id": format!(
                    "stream-{}",
                    request.params.get("kind").and_then(|value| value.as_str()).unwrap_or("default")
                )
            }),
            method::EVENT_READ => serde_json::json!({
                "events": [
                    {"kind": "observer-ready"}
                ],
                "closed": false,
                "dropped_count": 0
            }),
            method::EVENT_CLOSE => serde_json::Value::Null,
            method::SHUTDOWN => {
                send_async(
                    &mut writer,
                    &RpcMessage::Response(Response::ok(request.id, serde_json::Value::Null)),
                )
                .await
                .expect("send fake universal provider shutdown response");
                break;
            }
            _ => serde_json::Value::Null,
        };
        send_async(
            &mut writer,
            &RpcMessage::Response(Response::ok(request.id, response)),
        )
        .await
        .expect("send fake universal provider response");
    }
}

impl ManagedRpcSession for FakeManagedSession {
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        let shutdowns = Arc::clone(&self.shutdowns);
        Box::pin(async move {
            shutdowns.fetch_add(1, Ordering::SeqCst);
        })
    }

    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }
}

fn activation_fixture(
    panel_specs: &[(&str, &str)],
) -> (
    tempfile::TempDir,
    ExtensionRuntimeCatalog,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicBool>,
) {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(root.path().join("bin")).unwrap();
    std::fs::write(root.path().join("bin/provider"), b"provider").unwrap();
    std::fs::create_dir_all(root.path().join("ui")).unwrap();

    let panels: Vec<serde_json::Value> = panel_specs
        .iter()
        .map(|(id, runtime_id)| {
            let mut panel = serde_json::json!({
                "id": id,
                "title": id,
                "runtimeId": runtime_id,
                "template": "ui/main.html",
            });
            if *id == "overview" {
                panel["style"] = serde_json::json!("ui/main.css");
            }
            panel
        })
        .collect();
    let manifest = serde_json::json!({
        "schema_version": 1,
        "id": "com.navop.kafka",
        "name": "Kafka",
        "version": "0.1.0",
        "engines": {"onetcli": ">=0.1.0"},
        "permissions": ["spawn:./bin/provider"],
        "runtime": {
            "ipc": [
                {
                    "id": "main",
                    "entry": {"command": "bin/provider"},
                    "transport": {"kind": "local_socket", "connect_timeout_ms": 2_500},
                    "auto_restart": false,
                    "max_restart_attempts": 0,
                    "shutdown_grace_ms": 4_000
                },
                {
                    "id": "secondary",
                    "entry": {"command": "bin/provider"},
                    "transport": {"kind": "local_socket", "connect_timeout_ms": 2_500},
                    "auto_restart": true,
                    "max_restart_attempts": 2,
                    "shutdown_grace_ms": 4_000
                }
            ]
        },
        "contributes": {"declarativePanels": panels}
    });
    std::fs::create_dir_all(root.path()).unwrap();
    std::fs::write(root.path().join("extension.json"), manifest.to_string()).unwrap();
    std::fs::write(root.path().join("ui/main.html"), "<div></div>").unwrap();
    std::fs::write(root.path().join("ui/main.css"), "div { color: red; }").unwrap();

    let loaded = load_from_dir(root.path()).unwrap();
    let catalog = ExtensionRuntimeCatalog::from_manifests(vec![loaded]).unwrap();
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let session_closed = Arc::new(AtomicBool::new(false));
    (root, catalog, factory_calls, shutdowns, session_closed)
}

fn activation_manager(
    panel_specs: &[(&str, &str)],
) -> (
    tempfile::TempDir,
    ActivationManager,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicBool>,
) {
    let (root, catalog, calls, shutdowns, session_closed) = activation_fixture(panel_specs);
    let factory_calls = Arc::clone(&calls);
    let session_shutdowns = Arc::clone(&shutdowns);
    #[derive(Default)]
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
            Ok(extension_protocol::host::NotifyResult { clicked: None })
        }

        async fn quick_pick(
            &self,
            _params: extension_protocol::host::QuickPickParams,
        ) -> extension_host::HostResult<extension_protocol::host::QuickPickResult> {
            Ok(extension_protocol::host::QuickPickResult {
                selected: Vec::new(),
                cancelled: true,
            })
        }

        async fn open_view(
            &self,
            _params: extension_protocol::host::OpenViewParams,
        ) -> extension_host::HostResult<()> {
            Ok(())
        }

        async fn storage_get(
            &self,
            _params: extension_protocol::host::StorageGetParams,
        ) -> extension_host::HostResult<extension_protocol::host::StorageGetResult> {
            Ok(extension_protocol::host::StorageGetResult { value: None })
        }

        async fn storage_set(
            &self,
            _params: extension_protocol::host::StorageSetParams,
        ) -> extension_host::HostResult<()> {
            Ok(())
        }

        async fn log(
            &self,
            _params: extension_protocol::host::LogParams,
        ) -> extension_host::HostResult<()> {
            Ok(())
        }

        async fn show_dialog(
            &self,
            _params: extension_protocol::declarative_ui::UiDialogRequest,
        ) -> extension_host::HostResult<extension_protocol::declarative_ui::UiDialogResult>
        {
            Err(extension_host::HostError::NotImplemented(
                "test host does not present dialogs".into(),
            ))
        }
    }

    let factory_closed = Arc::clone(&session_closed);
    let factory: SessionFactory = Arc::new(move |_context| {
        let calls = Arc::clone(&factory_calls);
        let shutdowns = Arc::clone(&session_shutdowns);
        let closed = Arc::clone(&factory_closed);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            // The shared test control represents the crashed generation while
            // `check_runtime` captures its health. A factory call represents a
            // replacement generation, so reset that control to open here.
            closed.store(false, Ordering::SeqCst);
            Ok(Arc::new(FakeManagedSession {
                shutdowns: Arc::clone(&shutdowns),
                closed,
            }) as Arc<dyn ManagedRpcSession>)
        })
    });
    let host_api_factory = Arc::new(|_binding, _generation| {
        Arc::new(extension_host::HostApiHandler::new(Arc::new(NoopHost)))
    });
    (
        root,
        ActivationManager::new(catalog, factory, host_api_factory),
        calls,
        shutdowns,
        Arc::clone(&session_closed),
    )
}

async fn activation_universal_manager() -> ActivationManager {
    activation_universal_manager_with_sessions().await.0
}

async fn activation_universal_manager_with_sessions() -> (
    ActivationManager,
    Arc<std::sync::Mutex<Vec<Arc<ProcessRpcSession>>>>,
) {
    let (_root, catalog, _calls, _shutdowns, _closed) =
        activation_fixture(&[("topics", "main"), ("consumers", "secondary")]);
    // `TempDir` and the fixture counters are intentionally retained for the
    // test body even though this helper does not return them.
    let fixture = (_root, catalog, _calls, _shutdowns, _closed);
    let (root, catalog, calls, shutdowns, closed) = fixture;
    let sessions = Arc::new(std::sync::Mutex::new(Vec::new()));
    let factory_sessions = Arc::clone(&sessions);
    let factory: SessionFactory = Arc::new(move |_context| {
        let sessions = Arc::clone(&factory_sessions);
        Box::pin(async move {
            let session = fake_universal_provider_session().await;
            sessions.lock().unwrap().push(Arc::clone(&session));
            Ok(Arc::new(FakeUniversalManagedSession { inner: session })
                as Arc<dyn ManagedRpcSession>)
        })
    });
    let host_api_factory: HostApiFactory = Arc::new(|_, _| {
        Arc::new(extension_host::HostApiHandler::new(Arc::new(
            UniversalProviderHost::new(
                Vec::<String>::new(),
                Arc::new(MapSecretResolver::default()),
            ),
        )))
    });
    let manager = ActivationManager::new(catalog, factory, host_api_factory)
        .with_blob_store(BlobStore::default())
        .with_job_activation(Arc::new(JobActivationManager::new()))
        .with_event_activation(Arc::new(EventActivationManager::new()));
    // Keep the fixture-owned resources alive as long as the activation
    // manager; dropping the temp dir or counters early would not affect the
    // in-memory provider, but this documents the shared fixture lifetime.
    std::mem::forget((root, calls, shutdowns, closed));
    (manager, sessions)
}

#[derive(Default)]
struct HoldDialogPresenter {
    requests: std::sync::Mutex<Vec<DialogActivationRequest>>,
    completions: std::sync::Mutex<HashMap<DialogActivationKey, oneshot::Sender<DialogUserResult>>>,
}

impl HoldDialogPresenter {
    fn requests(&self) -> Vec<DialogActivationRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl DialogPresenter for HoldDialogPresenter {
    async fn show(
        &self,
        request: DialogActivationRequest,
        complete: oneshot::Sender<DialogUserResult>,
    ) {
        self.requests.lock().unwrap().push(request.clone());
        self.completions
            .lock()
            .unwrap()
            .insert(request.key, complete);
    }

    fn dismiss(&self, request: &DialogActivationRequest) {
        if let Some(complete) = self.completions.lock().unwrap().remove(&request.key) {
            let _ = complete.send(DialogUserResult::Terminal(DialogTerminalResult::Dismissed));
        }
    }
}

#[test]
fn binding_maps_to_process_session_config_without_losing_spawn_fields() {
    let extension = tempfile::TempDir::new().unwrap();
    let config = process_session_config(
        &binding(extension.path()),
        NegotiationConfig::new("1.0.0", "instance").offer_api("extension", "1.0"),
    )
    .expect("valid binding");

    assert_eq!(extension.path().join("bin/provider"), config.spawn.program);
    assert_eq!(
        Some(extension.path().to_path_buf()),
        config.spawn.program_root
    );
    assert_eq!(Some(extension.path().to_path_buf()), config.spawn.cwd_root);
    assert_eq!(vec!["--mode", "extension"], config.spawn.args);
    assert_eq!(Some(extension.path().to_path_buf()), config.spawn.cwd);
    assert_eq!(
        HashMap::from([("RUST_LOG".into(), "info".into())]),
        config.spawn.env
    );
    assert!(matches!(
        config.spawn.transport,
        SpawnTransport::LocalSocket { .. }
    ));
    assert_eq!(Duration::from_millis(2_500), config.spawn.ready_timeout);
    assert_eq!(DEFAULT_SESSION_REQUEST_TIMEOUT, config.request_timeout);
    assert_eq!(4_000, config.shutdown_grace_ms);
    assert_eq!("com.navop.kafka::main", config.label);
}

#[tokio::test]
async fn activation_manager_starts_runtimes_lazily_and_shares_sessions() {
    let (_root, manager, calls, shutdowns, _closed) = activation_manager(&[
        ("topics", "main"),
        ("consumers", "main"),
        ("brokers", "main"),
    ]);

    assert_eq!(0, calls.load(Ordering::SeqCst));
    assert_eq!(0, shutdowns.load(Ordering::SeqCst));
    assert!(manager.active_panel_keys().is_empty());

    let topics = manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
    assert_eq!(RuntimeActivationState::Active, topics.state);
    assert_eq!(1, calls.load(Ordering::SeqCst));

    manager
        .activate_panel("com.navop.kafka::consumers")
        .await
        .unwrap();
    assert_eq!(1, calls.load(Ordering::SeqCst));
    assert_eq!(0, shutdowns.load(Ordering::SeqCst));

    manager
        .deactivate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
    assert_eq!(0, shutdowns.load(Ordering::SeqCst));
    assert_eq!(
        ["com.navop.kafka::consumers".to_owned()]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        manager.active_panel_keys()
    );

    manager
        .deactivate_panel("com.navop.kafka::consumers")
        .await
        .unwrap();
    assert_eq!(1, shutdowns.load(Ordering::SeqCst));
    assert!(manager.active_panel_keys().is_empty());

    manager
        .deactivate_panel("com.navop.kafka::consumers")
        .await
        .unwrap();
    assert_eq!(1, shutdowns.load(Ordering::SeqCst));
}

#[tokio::test]
async fn stale_panel_lease_cannot_deactivate_reactivated_panel() {
    let (_root, manager, calls, shutdowns, _closed) = activation_manager(&[("topics", "main")]);

    let first = manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
    manager.deactivate_activation(&first).await.unwrap();
    assert_eq!(1, shutdowns.load(Ordering::SeqCst));

    let second = manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
    assert_ne!(first.activation_id, second.activation_id);
    assert_eq!(first.runtime_generation + 1, second.runtime_generation);
    assert_eq!(2, calls.load(Ordering::SeqCst));

    manager.deactivate_activation(&first).await.unwrap();
    assert_eq!(1, shutdowns.load(Ordering::SeqCst));
    assert_eq!(
        ["com.navop.kafka::topics".to_owned()]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        manager.active_panel_keys()
    );

    manager.deactivate_activation(&second).await.unwrap();
    assert_eq!(2, shutdowns.load(Ordering::SeqCst));
    assert!(manager.active_panel_keys().is_empty());
}

#[tokio::test]
async fn panel_lease_survives_provider_generation_restart() {
    let (_root, manager, _calls, shutdowns, closed) =
        activation_manager(&[("consumers", "secondary")]);
    let activation = manager
        .activate_panel("com.navop.kafka::consumers")
        .await
        .unwrap();

    closed.store(true, Ordering::SeqCst);
    manager
        .check_runtime("com.navop.kafka::secondary")
        .await
        .unwrap();
    assert_eq!(
        activation.runtime_generation + 1,
        manager
            .runtime_generation("com.navop.kafka::secondary")
            .unwrap()
    );

    manager.deactivate_activation(&activation).await.unwrap();
    assert!(manager.active_panel_keys().is_empty());
    assert_eq!(2, shutdowns.load(Ordering::SeqCst));
}

#[tokio::test]
async fn concurrent_panels_sharing_runtime_start_one_session() {
    let (_root, manager, calls, _shutdowns, _closed) =
        activation_manager(&[("topics", "main"), ("consumers", "main")]);

    let first = manager.activate_panel("com.navop.kafka::topics");
    let second = manager.activate_panel("com.navop.kafka::consumers");
    let (first, second) = tokio::join!(first, second);
    first.unwrap();
    second.unwrap();

    assert_eq!(1, calls.load(Ordering::SeqCst));
    assert_eq!(2, manager.active_panel_keys().len());
}

#[tokio::test]
async fn deactivating_extension_closes_all_owned_runtimes_once() {
    let (_root, manager, calls, shutdowns, _closed) =
        activation_manager(&[("topics", "main"), ("consumers", "secondary")]);
    manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
    manager
        .activate_panel("com.navop.kafka::consumers")
        .await
        .unwrap();
    assert_eq!(2, calls.load(Ordering::SeqCst));

    manager
        .deactivate_extension("com.navop.kafka")
        .await
        .unwrap();
    assert_eq!(2, shutdowns.load(Ordering::SeqCst));
    assert!(manager.active_panel_keys().is_empty());

    manager
        .deactivate_extension("com.navop.kafka")
        .await
        .unwrap();
    assert_eq!(2, shutdowns.load(Ordering::SeqCst));
}

#[tokio::test]
async fn activation_rejects_unknown_panel_without_calling_factory() {
    let (_root, manager, calls, _shutdowns, _closed) = activation_manager(&[("topics", "main")]);
    let error = manager
        .activate_panel("com.navop.other/topics")
        .await
        .unwrap_err();

    assert_eq!(
        ActivationError::PanelNotFound {
            panel_key: "com.navop.other/topics".into()
        },
        error
    );
    assert_eq!(0, calls.load(Ordering::SeqCst));
}

#[tokio::test]
async fn runtime_health_reports_active_and_degraded_sessions() {
    let (_root, manager, _calls, _shutdowns, closed) = activation_manager(&[("topics", "main")]);
    manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();

    let health = manager
        .runtime_health("com.navop.kafka::main")
        .await
        .unwrap();
    assert_eq!(RuntimeActivationState::Active, health.state);
    assert!(!health.session_closed);
    assert_eq!(None, health.ping_error);

    closed.store(true, Ordering::SeqCst);
    let health = manager
        .runtime_health("com.navop.kafka::main")
        .await
        .unwrap();
    assert_eq!(RuntimeActivationState::Failed, health.state);
    assert!(health.session_closed);
}

#[tokio::test]
async fn managed_client_acquisition_tracks_restart_generations() {
    let (_root, manager, calls, _shutdowns, closed) =
        activation_manager(&[("consumers", "secondary")]);
    manager
        .activate_panel("com.navop.kafka::consumers")
        .await
        .unwrap();

    let first_generation = manager
        .runtime_generation("com.navop.kafka::secondary")
        .unwrap();

    closed.store(true, Ordering::SeqCst);
    assert!(
        manager
            .universal_plugin_client("com.navop.kafka::secondary")
            .is_err()
    );

    manager
        .check_runtime("com.navop.kafka::secondary")
        .await
        .unwrap();
    let second_generation = manager
        .runtime_generation("com.navop.kafka::secondary")
        .unwrap();
    assert_eq!(first_generation + 1, second_generation);
    assert_eq!(2, calls.load(Ordering::SeqCst));
}

#[tokio::test]
async fn runtime_lifecycle_releases_generation_owned_host_blobs() {
    let (_root, manager, _calls, _shutdowns, closed) =
        activation_manager(&[("consumers", "secondary")]);
    let manager = manager.with_blob_store(BlobStore::default());
    manager
        .activate_panel("com.navop.kafka::consumers")
        .await
        .unwrap();
    let blobs = manager.blob_store().unwrap();
    let first_owner = BlobOwner {
        runtime_id: "com.navop.kafka::secondary".into(),
        generation: 0,
    };
    let first_blob = blobs
        .open(
            &first_owner,
            &extension_protocol::blob::BlobOpenParams::default(),
            vec![7; 12],
        )
        .unwrap();
    assert_eq!(12, blobs.total_bytes());

    // A replacement process may retain data for its own generation, but the
    // old generation must disappear atomically with restart bookkeeping.
    closed.store(true, Ordering::SeqCst);
    manager
        .check_runtime("com.navop.kafka::secondary")
        .await
        .unwrap();
    assert_eq!(
        1,
        manager
            .runtime_generation("com.navop.kafka::secondary")
            .unwrap()
    );
    assert!(matches!(
        blobs.info(&first_owner, &first_blob.blob_id),
        Err(BlobStoreError::Unknown(_))
    ));

    let second_owner = BlobOwner {
        runtime_id: first_owner.runtime_id.clone(),
        generation: 1,
    };
    let second_blob = blobs
        .open(
            &second_owner,
            &extension_protocol::blob::BlobOpenParams::default(),
            vec![8; 9],
        )
        .unwrap();
    assert_eq!(9, blobs.total_bytes());

    manager
        .deactivate_runtime("com.navop.kafka::secondary")
        .await
        .unwrap();
    assert!(matches!(
        blobs.info(&second_owner, &second_blob.blob_id),
        Err(BlobStoreError::Unknown(_))
    ));
    assert!(blobs.is_empty());
}

#[tokio::test]
async fn provider_dialogs_are_lifecycle_managed_by_the_host() {
    let held_presenter = Arc::new(HoldDialogPresenter::default());
    let presenter: Arc<dyn DialogPresenter> = held_presenter.clone();
    let dialogs = Arc::new(DialogActivationManager::new(Arc::clone(&presenter)));
    let (_root, manager, _calls, _shutdowns, _closed) = activation_manager(&[("topics", "main")]);
    let manager = manager.with_dialog_activation(Arc::clone(&dialogs));
    let manager = Arc::new(manager);
    manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();

    let shown_manager = Arc::clone(&manager);
    let shown = tokio::spawn(async move {
        shown_manager
            .activate_dialog(
                "com.navop.kafka",
                "com.navop.kafka::main",
                UiDialogRequest {
                    request_id: "request-1".into(),
                    dialog_id: "delete-topic".into(),
                    kind: UiDialogKind::Confirm,
                    title: "Delete topic".into(),
                    message: Some("This operation cannot be undone.".into()),
                    confirm_label: None,
                    cancel_label: None,
                    danger: true,
                    expected_revision: Some(7),
                },
            )
            .await
    });
    tokio::time::timeout(Duration::from_millis(100), async {
        while held_presenter.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("dialog reaches host presenter");

    let presented = held_presenter.requests();
    assert_eq!(1, presented.len());
    assert_eq!("com.navop.kafka", presented[0].key.extension_id);
    assert_eq!("com.navop.kafka::main", presented[0].key.runtime_id);
    assert_eq!(0, presented[0].key.generation);
    assert_eq!("request-1", presented[0].key.request_id);
    assert!(presented[0].dialog.danger);
    assert_eq!(1, dialogs.pending_count("com.navop.kafka::main"));

    manager
        .deactivate_runtime("com.navop.kafka::main")
        .await
        .unwrap();
    assert_eq!(
        UiDialogResult::Dismissed,
        shown
            .await
            .expect("dialog activation task succeeds")
            .expect("runtime cleanup returns a dialog result")
    );
    assert_eq!(0, dialogs.pending_count("com.navop.kafka::main"));
}

#[tokio::test]
async fn large_inline_provider_results_are_cached_as_host_blobs() {
    let manager = activation_universal_manager().await;
    manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
    let runtime_id = "com.navop.kafka::main";

    let invoked = manager
        .invoke_resource_and_cache_blob(
            runtime_id,
            &ResourceInvokeParams {
                resource_id: "resource-1".into(),
                method: "kafka/topic/export".into(),
                params: serde_json::Value::Null,
            },
        )
        .await
        .unwrap();
    let extension_protocol::result_ref::ResultRef::Blob { id } = invoked.result else {
        panic!("large inline result must move into the host blob store");
    };
    assert!(id.starts_with("host-blob-"));
    assert!(
        manager.blob_store().unwrap().total_bytes()
            > extension_protocol::blob::INLINE_BLOB_THRESHOLD_BYTES as usize
    );

    let client = manager.universal_plugin_client(runtime_id).unwrap();
    let info = client
        .blob_store()
        .unwrap()
        .info(&client.blob_owner(), &id)
        .unwrap();
    assert_eq!("application/json", info.content_type.unwrap());
    assert_eq!(
        Some(&serde_json::json!({
            "source": "resource_invoke",
            "source_id": "kafka/topic/export",
        })),
        info.metadata.as_ref()
    );

    let first = client
        .read_blob(&extension_protocol::blob::BlobReadParams {
            blob_id: id.clone(),
            max_bytes: Some(128),
        })
        .await
        .unwrap();
    assert_eq!(128, first.bytes_read);
    assert!(!first.done);
    assert_eq!(
        128,
        client
            .blob_store()
            .unwrap()
            .info(&client.blob_owner(), &id)
            .unwrap()
            .read_offset
    );

    client
        .close_blob(&extension_protocol::blob::BlobCloseParams { blob_id: id })
        .await
        .unwrap();
    assert!(manager.blob_store().unwrap().is_empty());

    manager
        .deactivate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
}

#[tokio::test]
async fn small_job_results_stay_inline_and_host_blob_ids_do_not_reach_providers() {
    let manager = activation_universal_manager().await;
    manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
    let client = manager
        .universal_plugin_client("com.navop.kafka::main")
        .unwrap();
    let handle = client
        .start_job(&extension_protocol::job::JobStartParams {
            resource_id: None,
            method: "test/small".into(),
            params: serde_json::json!({"job_id": "job-1"}),
        })
        .await
        .unwrap();

    let small = manager
        .job_result_and_cache_blob(
            "com.navop.kafka::main",
            &extension_protocol::job::JobResultParams {
                job_id: "job-1".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        extension_protocol::result_ref::ResultRef::Inline {
            value: serde_json::json!({"job": "completed"})
        },
        small.result
    );
    assert!(manager.blob_store().unwrap().is_empty());
    client.close_job(&handle).await.unwrap();

    // A host-prefixed id names a host-owned object even if no such object
    // exists. Routing it to the provider would let a replacement process turn
    // the namespace into an information oracle.
    let error = client
        .read_blob(&extension_protocol::blob::BlobReadParams {
            blob_id: "host-blob-does-not-exist".into(),
            max_bytes: Some(1),
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("closed or unknown"));

    manager
        .deactivate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
}

#[tokio::test]
async fn job_lifecycle_owns_and_reclaims_large_result_blob() {
    let manager = activation_universal_manager().await;
    manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
    let runtime_id = "com.navop.kafka::main";
    let client = manager.universal_plugin_client(runtime_id).unwrap();
    let handle = client
        .start_job(&extension_protocol::job::JobStartParams {
            resource_id: None,
            method: "test/large".into(),
            params: serde_json::json!({"job_id": "job-large"}),
        })
        .await
        .unwrap();
    assert_eq!(
        1,
        manager.job_activation().unwrap().active_count(runtime_id)
    );

    let status = client.job_status(&handle).await.unwrap();
    assert_eq!(extension_protocol::job::JobState::Running, status.state);
    client.cancel_job(&handle).await.unwrap();
    assert_eq!(
        extension_protocol::job::JobState::Running,
        manager.job_activation().unwrap().validate(&handle).unwrap()
    );

    let result = manager
        .job_result_and_cache_blob(
            runtime_id,
            &extension_protocol::job::JobResultParams {
                job_id: handle.job_id.clone(),
            },
        )
        .await
        .unwrap();
    let extension_protocol::result_ref::ResultRef::Blob { id } = result.result else {
        panic!("large job result should use host blob storage");
    };
    assert!(
        manager
            .blob_store()
            .unwrap()
            .info(&client.blob_owner(), &id)
            .is_ok()
    );

    client.close_job(&handle).await.unwrap();
    assert_eq!(
        0,
        manager.job_activation().unwrap().active_count(runtime_id)
    );
    assert!(matches!(
        manager
            .blob_store()
            .unwrap()
            .info(&client.blob_owner(), &id),
        Err(BlobStoreError::Unknown(_))
    ));
}

#[tokio::test]
async fn provider_close_failure_still_reclaims_host_job_ownership() {
    let manager = activation_universal_manager().await;
    manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
    let runtime_id = "com.navop.kafka::main";
    let client = manager.universal_plugin_client(runtime_id).unwrap();
    let handle = client
        .start_job(&extension_protocol::job::JobStartParams {
            resource_id: None,
            method: "test/close-error".into(),
            params: serde_json::json!({"job_id": "job-close-error"}),
        })
        .await
        .unwrap();

    let error = client.close_job(&handle).await.unwrap_err();

    assert!(error.to_string().contains("provider refused job close"));
    assert_eq!(
        0,
        manager.job_activation().unwrap().active_count(runtime_id)
    );
    assert!(manager.job_activation().unwrap().validate(&handle).is_err());
}

#[tokio::test]
async fn runtime_restart_recovers_job_under_replacement_generation() {
    let (manager, sessions) = activation_universal_manager_with_sessions().await;
    manager
        .activate_panel("com.navop.kafka::consumers")
        .await
        .unwrap();
    let runtime_id = "com.navop.kafka::secondary";
    let old_client = manager.universal_plugin_client(runtime_id).unwrap();
    let old_handle = old_client
        .start_job(&extension_protocol::job::JobStartParams {
            resource_id: None,
            method: "test/recover".into(),
            params: serde_json::json!({"job_id": "job-recover"}),
        })
        .await
        .unwrap();
    let old_session = sessions.lock().unwrap()[0].clone();
    old_session.shutdown().await;

    manager.check_runtime(runtime_id).await.unwrap();
    let new_client = manager.universal_plugin_client(runtime_id).unwrap();
    let new_handle = manager
        .job_activation()
        .unwrap()
        .handle(
            "com.navop.kafka",
            runtime_id,
            new_client.runtime_generation(),
            &old_handle.job_id,
        )
        .unwrap();

    assert_eq!(old_handle.generation + 1, new_handle.generation);
    assert!(old_client.job_status(&old_handle).await.is_err());
    assert_eq!(
        extension_protocol::job::JobState::Running,
        new_client.job_status(&new_handle).await.unwrap().state
    );
    assert!(matches!(
        manager
            .job_result_and_cache_blob(
                runtime_id,
                &extension_protocol::job::JobResultParams {
                    job_id: new_handle.job_id.clone(),
                },
            )
            .await
            .unwrap()
            .result,
        extension_protocol::result_ref::ResultRef::Inline { .. }
    ));
    new_client.close_job(&new_handle).await.unwrap();
    assert_eq!(
        0,
        manager.job_activation().unwrap().active_count(runtime_id)
    );
}

#[tokio::test]
async fn provider_event_streams_are_lifecycle_managed_by_the_host() {
    let manager = activation_universal_manager().await;
    manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
    let runtime_id = "com.navop.kafka::main";
    let old_client = manager.universal_plugin_client(runtime_id).unwrap();

    let opened = old_client
        .open_event_stream(&extension_protocol::event_stream::EventOpenParams {
            conn_id: None,
            kind: "observer".into(),
            capacity: Some(16),
        })
        .await
        .unwrap();
    assert_eq!("stream-observer", opened.stream_id);
    assert_eq!(
        1,
        manager.event_activation().unwrap().open_count(runtime_id)
    );

    let read = old_client
        .read_event_stream(&extension_protocol::event_stream::EventReadParams {
            stream_id: opened.stream_id.clone(),
            max_events: Some(1),
            wait_ms: Some(0),
        })
        .await
        .unwrap();
    assert_eq!(1, read.events.len());
    assert!(!read.closed);

    manager.deactivate_runtime(runtime_id).await.unwrap();
    assert_eq!(
        0,
        manager.event_activation().unwrap().open_count(runtime_id)
    );

    manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
    let new_client = manager.universal_plugin_client(runtime_id).unwrap();
    assert_eq!(1, new_client.runtime_generation());

    // The old client remains generation-bound. Its stream ID must not be
    // routed to the replacement process even if the old transport is reused.
    let stale_error = old_client
        .read_event_stream(&extension_protocol::event_stream::EventReadParams {
            stream_id: opened.stream_id.clone(),
            max_events: Some(1),
            wait_ms: Some(0),
        })
        .await
        .unwrap_err();
    assert!(stale_error.to_string().contains("generation"));

    // A replacement generation may reuse the provider-local stream namespace.
    let reopened = new_client
        .open_event_stream(&extension_protocol::event_stream::EventOpenParams {
            conn_id: None,
            kind: "observer".into(),
            capacity: Some(16),
        })
        .await
        .unwrap();
    assert_eq!(opened.stream_id, reopened.stream_id);
    assert_eq!(
        1,
        manager.event_activation().unwrap().open_count(runtime_id)
    );

    new_client
        .close_event_stream(&extension_protocol::event_stream::EventCloseParams {
            stream_id: reopened.stream_id,
        })
        .await
        .unwrap();
    assert_eq!(
        0,
        manager.event_activation().unwrap().open_count(runtime_id)
    );

    manager
        .deactivate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
}

#[tokio::test]
async fn check_runtime_does_not_restart_open_sessions() {
    let (_root, manager, calls, shutdowns, _closed) = activation_manager(&[("topics", "main")]);
    manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();

    let health = manager
        .check_runtime("com.navop.kafka::main")
        .await
        .unwrap();

    assert_eq!(RuntimeActivationState::Active, health.state);
    assert_eq!(1, calls.load(Ordering::SeqCst));
    assert_eq!(0, shutdowns.load(Ordering::SeqCst));
}

#[tokio::test]
async fn check_runtime_fails_closed_sessions_when_restart_is_disabled() {
    let (_root, manager, calls, shutdowns, closed) = activation_manager(&[("topics", "main")]);
    manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
    closed.store(true, Ordering::SeqCst);

    let health = manager
        .check_runtime("com.navop.kafka::main")
        .await
        .unwrap();

    assert_eq!(RuntimeActivationState::Failed, health.state);
    assert!(health.session_closed);
    assert_eq!(0, health.restart_attempts);
    assert_eq!(0, health.restart_budget);
    assert_eq!(1, calls.load(Ordering::SeqCst));
    assert_eq!(1, shutdowns.load(Ordering::SeqCst));
}

#[tokio::test]
async fn closed_secondary_runtime_restarts_with_backoff_and_budget() {
    let (_root, manager, calls, shutdowns, closed) =
        activation_manager(&[("consumers", "secondary")]);
    manager
        .activate_panel("com.navop.kafka::consumers")
        .await
        .unwrap();
    closed.store(true, Ordering::SeqCst);

    let first = manager
        .check_runtime("com.navop.kafka::secondary")
        .await
        .unwrap();
    assert_eq!(RuntimeActivationState::Active, first.state);
    assert!(!first.session_closed);
    assert_eq!(1, first.restart_attempts);
    assert_eq!(2, first.restart_budget);
    assert!(first.restart_backoff_remaining.is_some());
    assert_eq!(2, calls.load(Ordering::SeqCst));
    assert_eq!(1, shutdowns.load(Ordering::SeqCst));
    closed.store(true, Ordering::SeqCst);

    // Backoff is observed without sleeping in `check_runtime`.
    let second = manager
        .check_runtime("com.navop.kafka::secondary")
        .await
        .unwrap();
    assert_eq!(RuntimeActivationState::Restarting, second.state);
    assert!(second.session_closed);
    assert!(second.restart_backoff_remaining.is_some());
    assert_eq!(2, calls.load(Ordering::SeqCst));

    // Simulate backoff elapsed. The second successful restart exhausts the
    // configured budget, and another closure is classified as a crash loop.
    manager.clear_restart_backoff_for_test("com.navop.kafka::secondary");
    let third = manager
        .check_runtime("com.navop.kafka::secondary")
        .await
        .unwrap();
    assert_eq!(RuntimeActivationState::Active, third.state);
    assert_eq!(2, third.restart_attempts);
    assert_eq!(3, calls.load(Ordering::SeqCst));
    closed.store(true, Ordering::SeqCst);

    let crash_loop = manager
        .check_runtime("com.navop.kafka::secondary")
        .await
        .unwrap();
    assert_eq!(RuntimeActivationState::CrashLoop, crash_loop.state);
    assert_eq!(2, crash_loop.restart_attempts);
    assert_eq!(2, crash_loop.restart_budget);
    assert_eq!(3, calls.load(Ordering::SeqCst));
    // Both replaced sessions and the terminal crash-loop session are retired.
    assert_eq!(3, shutdowns.load(Ordering::SeqCst));
}

#[tokio::test]
async fn runtime_monitor_emits_health_transitions_and_removals() {
    let (_root, manager, calls, _shutdowns, closed) =
        activation_manager(&[("consumers", "secondary")]);
    let manager = Arc::new(manager);
    manager
        .activate_panel("com.navop.kafka::consumers")
        .await
        .unwrap();
    let monitor = RuntimeMonitor::new(
        Arc::clone(&manager),
        RuntimeMonitorConfig {
            check_interval: Duration::from_millis(5),
        },
    );
    let mut events = monitor.subscribe();
    monitor.track("com.navop.kafka::secondary");

    monitor.run_once().await;
    let RuntimeMonitorEvent::HealthChanged { runtime_id, health } = events.recv().await.unwrap()
    else {
        panic!("expected initial health event");
    };
    assert_eq!("com.navop.kafka::secondary", runtime_id);
    assert_eq!(RuntimeActivationState::Active, health.state);
    assert_eq!(
        Some(health.clone()),
        monitor.runtime_health("com.navop.kafka::secondary")
    );
    assert_eq!(
        BTreeMap::from([("com.navop.kafka::secondary".into(), health.clone())]),
        monitor.runtime_healths()
    );

    // A stable snapshot is collapsed instead of flooding UI subscribers.
    monitor.run_once().await;
    assert!(events.try_recv().is_err());

    closed.store(true, Ordering::SeqCst);
    monitor.run_once().await;
    let RuntimeMonitorEvent::HealthChanged { health, .. } = events.recv().await.unwrap() else {
        panic!("expected restart health event");
    };
    assert_eq!(RuntimeActivationState::Active, health.state);
    assert_eq!(1, health.restart_attempts);
    assert_eq!(2, calls.load(Ordering::SeqCst));

    manager
        .deactivate_runtime("com.navop.kafka::secondary")
        .await
        .unwrap();
    monitor.run_once().await;
    let RuntimeMonitorEvent::RuntimeRemoved { runtime_id } = events.recv().await.unwrap() else {
        panic!("expected runtime removal event");
    };
    assert_eq!("com.navop.kafka::secondary", runtime_id);

    // Once removed, the monitor does not repeat removal notifications.
    monitor.run_once().await;
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn runtime_monitor_task_starts_stops_and_rejects_double_start() {
    let (_root, manager, _calls, _shutdowns, _closed) = activation_manager(&[("topics", "main")]);
    let manager = Arc::new(manager);
    manager
        .activate_panel("com.navop.kafka::topics")
        .await
        .unwrap();
    let monitor = RuntimeMonitor::new(
        manager,
        RuntimeMonitorConfig {
            check_interval: Duration::from_millis(5),
        },
    );
    monitor.track("com.navop.kafka::main");
    let mut events = monitor.subscribe();

    monitor.start().unwrap();
    assert_eq!(
        RuntimeMonitorError::AlreadyRunning,
        monitor.start().unwrap_err()
    );
    let RuntimeMonitorEvent::HealthChanged { runtime_id, health } = events.recv().await.unwrap()
    else {
        panic!("expected monitor health event");
    };
    assert_eq!("com.navop.kafka::main", runtime_id);
    assert_eq!(RuntimeActivationState::Active, health.state);
    assert_eq!(
        Some(health),
        monitor.runtime_health("com.navop.kafka::main")
    );

    monitor.stop().await;
    assert!(events.try_recv().is_err());
    assert!(monitor.start().is_ok());
    monitor.stop().await;
}

#[test]
fn activation_catalog_projects_ui_metadata_without_trusting_paths() {
    let (_root, manager, _calls, _shutdowns, _closed) =
        activation_manager(&[("topics", "main"), ("overview", "secondary")]);

    let panels = manager.declarative_panel_catalog();
    assert_eq!(
        vec![
            DeclarativePanelDescriptor {
                extension_id: "com.navop.kafka".into(),
                panel_key: "com.navop.kafka::overview".into(),
                title: "overview".into(),
                runtime_id: "com.navop.kafka::secondary".into(),
                placement: DeclarativePanelPlacement::HomeSidebar,
                icon: None,
            },
            DeclarativePanelDescriptor {
                extension_id: "com.navop.kafka".into(),
                panel_key: "com.navop.kafka::topics".into(),
                title: "topics".into(),
                runtime_id: "com.navop.kafka::main".into(),
                placement: DeclarativePanelPlacement::HomeSidebar,
                icon: None,
            },
        ],
        panels
    );
}

#[test]
fn declarative_panel_source_returns_validated_text_without_paths() {
    let (root, manager, _calls, _shutdowns, _closed) = activation_manager(&[("overview", "main")]);
    std::fs::write(root.path().join("ui/main.html"), "<span>Topics</span>").unwrap();
    std::fs::write(root.path().join("ui/main.css"), "span { color: red; }").unwrap();

    let source = manager
        .declarative_panel_source("com.navop.kafka::overview")
        .unwrap();

    assert_eq!("com.navop.kafka", source.extension_id);
    assert_eq!("com.navop.kafka::overview", source.panel_key);
    assert_eq!("overview", source.title);
    assert_eq!("<span>Topics</span>", source.template);
    assert_eq!(Some("span { color: red; }".to_owned()), source.style);
}

#[test]
fn declarative_panel_source_allows_templates_without_styles() {
    let (_root, manager, _calls, _shutdowns, _closed) = activation_manager(&[("topics", "main")]);

    let source = manager
        .declarative_panel_source("com.navop.kafka::topics")
        .unwrap();

    assert_eq!("<div></div>", source.template);
    assert_eq!(None, source.style);
}

#[test]
fn declarative_panel_source_rejects_unknown_panels() {
    let (_root, manager, _calls, _shutdowns, _closed) = activation_manager(&[("topics", "main")]);

    assert_eq!(
        PanelSourceError::PanelNotFound {
            panel_key: "com.navop.kafka::missing".to_owned(),
        },
        manager
            .declarative_panel_source("com.navop.kafka::missing")
            .unwrap_err()
    );
}

#[test]
fn binding_rejects_transport_or_shutdown_values_the_host_cannot_represent() {
    let extension = tempfile::TempDir::new().unwrap();
    let mut unsupported = binding(extension.path());
    unsupported.transport_kind = "stdio".into();
    assert_eq!(
        PluginAdapterError::UnsupportedTransport("stdio".into()),
        process_session_config(&unsupported, NegotiationConfig::new("1.0.0", "instance"))
            .expect_err("unsupported transport")
    );

    let mut overflow = binding(extension.path());
    overflow.shutdown_grace_ms = u64::from(u32::MAX) + 1;
    assert_eq!(
        PluginAdapterError::ShutdownGraceOverflow(u64::from(u32::MAX) + 1),
        process_session_config(&overflow, NegotiationConfig::new("1.0.0", "instance"))
            .expect_err("overflow")
    );
}

#[test]
fn binding_rejects_missing_spawn_permission() {
    let extension = tempfile::TempDir::new().unwrap();
    let mut unauthorized = binding(extension.path());
    unauthorized
        .permissions
        .retain(|permission| !permission.starts_with("spawn:"));

    assert_eq!(
        PluginAdapterError::MissingSpawnPermission("spawn:./bin/provider".into()),
        process_session_config(&unauthorized, NegotiationConfig::new("1.0.0", "instance"))
            .expect_err("missing permission")
    );
}

#[test]
fn allowlisted_absolute_program_still_constrains_extension_working_directory() {
    let extension = tempfile::TempDir::new().unwrap();
    let mut absolute = binding(extension.path());
    absolute.command = PathBuf::from("/usr/bin/true");
    absolute.required_spawn_permission = "spawn:/usr/bin/true".into();
    absolute.permissions = vec!["spawn:/usr/bin/true".into()];

    let config = process_session_config(&absolute, NegotiationConfig::new("1.0.0", "instance"))
        .expect("allowlisted absolute command");
    assert_eq!(Some(PathBuf::from("/usr/bin")), config.spawn.program_root);
    assert_eq!(Some(extension.path().to_path_buf()), config.spawn.cwd_root);
}

#[cfg(unix)]
#[test]
fn binding_rejects_symlink_escape_before_spawn() {
    use std::os::unix::fs::symlink;

    let extension = tempfile::TempDir::new().unwrap();
    let outside = tempfile::TempDir::new().unwrap();
    std::fs::write(outside.path().join("provider"), b"provider").unwrap();
    let escaped = binding(extension.path());
    std::fs::remove_file(&escaped.command).unwrap();
    symlink(outside.path().join("provider"), &escaped.command).unwrap();

    assert!(matches!(
        process_session_config(&escaped, NegotiationConfig::new("1.0.0", "instance")),
        Err(PluginAdapterError::PathEscapesAllowedRoot { .. })
    ));
}

#[test]
fn declarative_ui_wire_types_preserve_action_and_state_semantics() {
    let event = ActionEvent::new("refresh", "topics", NodePath(vec![0, 2]))
        .with_payload(BTreeMap::from([("filter".into(), "orders".into())]));
    let request = ui_action_request(&event, "request-1", Some(7));
    assert_eq!("request-1", request.request_id);
    assert_eq!("refresh", request.action);
    assert_eq!("topics", request.source_id);
    assert_eq!(vec![0, 2], request.source_path);
    assert_eq!(
        Some("orders"),
        request.payload.get("filter").map(String::as_str)
    );
    assert_eq!(Some(7), request.expected_revision);

    let operations = state_operations(&UiStatePatch {
        expected_revision: Some(7),
        operations: vec![
            UiStateOperation::Set {
                key: "status".into(),
                value: "ready".into(),
            },
            UiStateOperation::Remove {
                key: "error".into(),
            },
        ],
        event_subscriptions: Vec::new(),
    });
    assert_eq!(
        vec![
            StateOperation::Set {
                key: "status".into(),
                value: "ready".into()
            },
            StateOperation::Remove {
                key: "error".into()
            }
        ],
        operations
    );
}
