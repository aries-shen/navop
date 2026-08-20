use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use extension_host::{NegotiationConfig, ProcessRpcSession, UniversalPluginClient};
use extension_plugin_adapter::process_session_config;
use extension_protocol::{
    declarative_ui::{UiActionRequest, UiStateOperation},
    method,
    resource::{ResourceCloseParams, ResourceInvokeParams, ResourceOpenParams, ResourcePingParams},
    result_ref::ResultRef,
};
use extension_runtime::{
    ExtensionRuntimeCatalog,
    extension::manifest::{current_host_version, load_from_dir},
};
use serde_json::{Value, json};

const MANIFEST: &str = r##"{
  "schema_version": 1,
  "id": "com.navop.elasticsearch",
  "name": "Navop Elasticsearch Resource Provider",
  "version": "0.1.0",
  "engines": { "onetcli": ">=0.1.0" },
  "permissions": [
    "net:tcp:elasticsearch.example.com:9200",
    "secrets:read:elasticsearch.*",
    "spawn:./bin/elasticsearch-provider",
    "ui:tab",
    "ui:notify",
    "ui:progress"
  ],
  "runtime": {
    "ipc": [{
      "id": "main",
      "entry": { "command": "./bin/elasticsearch-provider" },
      "transport": {
        "kind": "local_socket",
        "connect_timeout_ms": 5000
      },
      "shutdown_grace_ms": 2500
    }]
  },
  "contributes": {
    "declarativePanels": [{
      "id": "elasticsearch",
      "title": "Elasticsearch",
      "runtimeId": "main",
      "template": "ui/main.html",
      "placement": "home_sidebar",
      "icon": "search",
      "activation": ["onConnectionKind:elasticsearch"]
    }]
  }
}"##;

fn copy_executable(source: &Path, destination: &Path) {
    fs::copy(source, destination).expect("copy provider executable");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(destination)
            .expect("provider executable metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(destination, permissions).expect("make provider executable");
    }
}

#[tokio::test]
async fn fixture_provider_supports_lifecycle_invoke_and_ui_patch() {
    let root = tempfile::tempdir().expect("extension temp root");
    let bin_dir = root.path().join("bin");
    let ui_dir = root.path().join("ui");
    fs::create_dir_all(&bin_dir).expect("create bin directory");
    fs::create_dir_all(&ui_dir).expect("create ui directory");
    copy_executable(
        Path::new(env!("CARGO_BIN_EXE_elasticsearch-provider")),
        &bin_dir.join("elasticsearch-provider"),
    );
    fs::write(root.path().join("extension.json"), MANIFEST).expect("write manifest");
    fs::write(
        ui_dir.join("main.html"),
        r#"<div id="elasticsearch-provider"><button id="refresh" action="refresh-resources" /></div>"#,
    )
    .expect("write declarative UI");

    let manifest = load_from_dir(root.path()).expect("load extension manifest");
    assert!(manifest.manifest_dir.starts_with(root.path()));
    let catalog = ExtensionRuntimeCatalog::from_manifests(vec![manifest]).expect("build catalog");
    let binding = catalog
        .ipc_runtime_bindings()
        .find(|binding| binding.runtime_key == "com.navop.elasticsearch::main")
        .expect("registered IPC binding")
        .clone();
    assert_eq!(
        PathBuf::from("bin/elasticsearch-provider"),
        binding.command.strip_prefix(root.path()).unwrap()
    );

    let host_version = current_host_version().to_string();
    let negotiation =
        NegotiationConfig::new(host_version, "elasticsearch-e2e").offer_api("extension", "1.0");
    let config = process_session_config(&binding, negotiation)
        .expect("resolve package-contained process session config")
        .with_request_timeout(Duration::from_secs(5))
        .with_label("elasticsearch-e2e");
    let session = Arc::new(
        ProcessRpcSession::start(config)
            .await
            .expect("start provider"),
    );
    assert!(session.declares_method(method::RESOURCE_INVOKE));
    let client = UniversalPluginClient::new(Arc::clone(&session));

    let opened = client
        .open_resource(&ResourceOpenParams {
            resource_type: "elasticsearch".into(),
            config: json!({
                "url": "elasticsearch.example.com:9200",
                "credential_ref": "secret://elasticsearch/api_key"
            }),
            metadata: None,
        })
        .await
        .expect("open resource");
    assert_eq!("fixture-elasticsearch", opened.resource_id);
    assert_eq!(
        serde_json::json!([
            "elasticsearch/index/list",
            "elasticsearch/index/get",
            "elasticsearch/search"
        ]),
        serde_json::to_value(opened.capabilities).unwrap()
    );
    assert_eq!(
        Some(&json!({ "mode": "fixture", "network": false })),
        opened.metadata.as_ref()
    );

    client
        .ping_resource(&ResourcePingParams {
            resource_id: opened.resource_id.clone(),
        })
        .await
        .expect("ping resource");

    let listed = client
        .invoke_resource(&ResourceInvokeParams {
            resource_id: opened.resource_id.clone(),
            method: "elasticsearch/index/list".into(),
            params: Value::Null,
        })
        .await
        .expect("list indices");
    let ResultRef::Inline { value: indices } = listed.result else {
        panic!("index list must be inline");
    };
    let names: Vec<&str> = indices["indices"]
        .as_array()
        .expect("indices array")
        .iter()
        .map(|index| index["name"].as_str().expect("index name"))
        .collect();
    assert_eq!(vec!["orders", "users"], names);

    let fetched = client
        .invoke_resource(&ResourceInvokeParams {
            resource_id: opened.resource_id.clone(),
            method: "elasticsearch/index/get".into(),
            params: json!({ "name": "orders" }),
        })
        .await
        .expect("get index");
    let ResultRef::Inline { value: index } = fetched.result else {
        panic!("index get must be inline");
    };
    assert_eq!(12_345, index["docs"].as_u64().expect("document count"));

    let searched = client
        .invoke_resource(&ResourceInvokeParams {
            resource_id: opened.resource_id.clone(),
            method: "elasticsearch/search".into(),
            params: json!({ "query": "alice" }),
        })
        .await
        .expect("search indices");
    let ResultRef::Inline { value: search } = searched.result else {
        panic!("search must be inline");
    };
    assert_eq!(
        2,
        search["indices"].as_array().expect("search indices").len()
    );

    let patch = client
        .ui_action(&UiActionRequest {
            request_id: "request-1".into(),
            action: "refresh-resources".into(),
            source_id: "elasticsearch-refresh".into(),
            source_path: Vec::new(),
            payload: BTreeMap::new(),
            expected_revision: Some(7),
        })
        .await
        .expect("refresh declarative UI");
    assert_eq!(Some(7), patch.expected_revision);
    assert!(patch.operations.contains(&UiStateOperation::Set {
        key: "provider_status".into(),
        value: "ready".into()
    }));
    let indices_json = patch
        .operations
        .iter()
        .find_map(|operation| match operation {
            UiStateOperation::Set { key, value } if key == "indices_json" => Some(value.clone()),
            _ => None,
        })
        .expect("indices JSON state");
    let indices_state: Value = serde_json::from_str(&indices_json).expect("valid indices JSON");
    assert_eq!(
        2,
        indices_state["indices"].as_array().expect("indices").len()
    );

    client
        .close_resource(&ResourceCloseParams {
            resource_id: opened.resource_id,
        })
        .await
        .expect("close resource");
    session.shutdown().await;
    assert!(session.is_closed());
}
