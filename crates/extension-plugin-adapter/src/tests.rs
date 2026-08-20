use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use declarative_ui_demo::NodePath;
use extension_host::{DEFAULT_SESSION_REQUEST_TIMEOUT, SpawnTransport};
use extension_runtime::ExtensionRuntimeCatalog;
use extension_runtime::extension::manifest::load_from_dir;
use futures::future::BoxFuture;

use super::*;

fn binding(extension_root: &std::path::Path) -> RegisteredIpcRuntimeBinding {
    let command = extension_root.join("bin/provider");
    std::fs::create_dir_all(command.parent().unwrap()).unwrap();
    std::fs::write(&command, b"provider").unwrap();
    RegisteredIpcRuntimeBinding {
        extension_id: "com.navop.kafka".into(),
        runtime_key: "com.navop.kafka::runtime/main".into(),
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
}

impl ManagedRpcSession for FakeManagedSession {
    fn shutdown<'a>(&'a self) -> BoxFuture<'a, ()> {
        let shutdowns = Arc::clone(&self.shutdowns);
        Box::pin(async move {
            shutdowns.fetch_add(1, Ordering::SeqCst);
        })
    }
}

fn activation_fixture(
    panel_specs: &[(&str, &str)],
) -> (
    tempfile::TempDir,
    ExtensionRuntimeCatalog,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let root = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(root.path().join("bin")).unwrap();
    std::fs::write(root.path().join("bin/provider"), b"provider").unwrap();
    std::fs::create_dir_all(root.path().join("ui")).unwrap();

    let panels: Vec<serde_json::Value> = panel_specs
        .iter()
        .map(|(id, runtime_id)| {
            serde_json::json!({
                "id": id,
                "title": id,
                "runtimeId": runtime_id,
                "template": "ui/main.html",
            })
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
                {"id": "main", "entry": {"command": "bin/provider"}},
                {"id": "secondary", "entry": {"command": "bin/provider"}}
            ]
        },
        "contributes": {"declarativePanels": panels}
    });
    std::fs::create_dir_all(root.path()).unwrap();
    std::fs::write(root.path().join("extension.json"), manifest.to_string()).unwrap();
    std::fs::write(root.path().join("ui/main.html"), "<div></div>").unwrap();

    let loaded = load_from_dir(root.path()).unwrap();
    let catalog = ExtensionRuntimeCatalog::from_manifests(vec![loaded]).unwrap();
    let factory_calls = Arc::new(AtomicUsize::new(0));
    let shutdowns = Arc::new(AtomicUsize::new(0));
    (root, catalog, factory_calls, shutdowns)
}

fn activation_manager(
    panel_specs: &[(&str, &str)],
) -> (
    tempfile::TempDir,
    ActivationManager,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let (root, catalog, calls, shutdowns) = activation_fixture(panel_specs);
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
    }

    let factory: SessionFactory = Arc::new(move |_context| {
        let calls = Arc::clone(&factory_calls);
        let shutdowns = Arc::clone(&session_shutdowns);
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(FakeManagedSession {
                shutdowns: Arc::clone(&shutdowns),
            }) as Arc<dyn ManagedRpcSession>)
        })
    });
    let host_api_factory =
        Arc::new(|_binding| Arc::new(extension_host::HostApiHandler::new(Arc::new(NoopHost))));
    (
        root,
        ActivationManager::new(catalog, factory, host_api_factory),
        calls,
        shutdowns,
    )
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
    assert_eq!("com.navop.kafka::runtime/main", config.label);
}

#[tokio::test]
async fn activation_manager_starts_runtimes_lazily_and_shares_sessions() {
    let (_root, manager, calls, shutdowns) = activation_manager(&[
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
async fn concurrent_panels_sharing_runtime_start_one_session() {
    let (_root, manager, calls, _shutdowns) =
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
    let (_root, manager, calls, shutdowns) =
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
    let (_root, manager, calls, _shutdowns) = activation_manager(&[("topics", "main")]);
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
