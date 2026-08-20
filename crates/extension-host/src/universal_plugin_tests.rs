//! `UniversalPluginClient` 的进程级协议契约测试。

use std::{collections::BTreeMap, time::Duration};

use extension_protocol::{
    declarative_ui::{
        UiDialogKind, UiDialogRequest, UiDialogResult, UiWindowOperation, UiWindowRequest,
    },
    envelope::{Response, RpcMessage},
    job::{JobStartParams, JobState},
    lifecycle::InitResult,
    resource::{ResourceInvokeParams, ResourceOpenParams},
    result_ref::ResultRef,
};
use serde_json::{Value, json};
use tokio::{io::duplex, sync::mpsc};

use super::*;
use crate::{
    JsonRpcClient, NegotiationConfig, ProcessRpcSessionConfig, SpawnConfig,
    transport::{FramedTransport, recv_async, send_async},
};

async fn fake_extension(
    mut reader: tokio::io::ReadHalf<tokio::io::DuplexStream>,
    mut writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    declared_methods: Vec<String>,
    observed: mpsc::UnboundedSender<(String, Value)>,
) {
    while let Ok(message) = recv_async::<_, RpcMessage>(&mut reader).await {
        let RpcMessage::Request(request) = message else {
            continue;
        };
        let result = match request.method.as_str() {
            method::INIT => {
                let mut init = InitResult::new("1.0.0").with_api("extension", "1.0");
                for method in &declared_methods {
                    init = init.with_method(method);
                }
                serde_json::to_value(init).expect("serialize init")
            }
            method::RESOURCE_OPEN => json!({
                "resource_id": "resource-1",
                "capabilities": ["kafka/topic/list"]
            }),
            method::RESOURCE_INVOKE => {
                json!({"result": {"kind": "inline", "value": {"topics": ["orders"]}}})
            }
            method::JOB_START => json!({"job_id": "job-1", "state": "queued"}),
            method::UI_ACTION => json!({
                "expected_revision": 7,
                "operations": [{"operation": "set", "key": "status", "value": "ready"}]
            }),
            method::UI_DIALOG => json!({"result": "prompt", "value": "orders"}),
            method::UI_WINDOW => Value::Null,
            method::SHUTDOWN
            | method::RESOURCE_PING
            | method::RESOURCE_CLOSE
            | method::JOB_CANCEL
            | method::JOB_CLOSE => Value::Null,
            _ => Value::Null,
        };
        if request.method != method::INIT && request.method != method::SHUTDOWN {
            observed
                .send((request.method.clone(), request.params.clone()))
                .expect("record request");
        }
        send_async(
            &mut writer,
            &RpcMessage::Response(Response::ok(request.id, result)),
        )
        .await
        .expect("send response");
        if request.method == method::SHUTDOWN {
            break;
        }
    }
}

async fn test_client(
    declared_methods: &[&str],
) -> (
    UniversalPluginClient,
    mpsc::UnboundedReceiver<(String, Value)>,
) {
    let (client_side, extension_side) = duplex(16 * 1024);
    let (client_reader, client_writer) = tokio::io::split(client_side);
    let (extension_reader, extension_writer) = tokio::io::split(extension_side);
    let (observed_tx, observed_rx) = mpsc::unbounded_channel();
    tokio::spawn(fake_extension(
        extension_reader,
        extension_writer,
        declared_methods
            .iter()
            .map(|method| (*method).to_owned())
            .collect(),
        observed_tx,
    ));

    let client = JsonRpcClient::start(FramedTransport::new(client_reader, client_writer));
    let config = ProcessRpcSessionConfig::new(
        SpawnConfig::new("test-provider"),
        NegotiationConfig::new("1.0.0", "instance").offer_api("extension", "1.0"),
    )
    .with_request_timeout(Duration::from_secs(1));
    let session = ProcessRpcSession::start_with_client(client, None, config)
        .await
        .expect("start session");
    (UniversalPluginClient::new(Arc::new(session)), observed_rx)
}

#[tokio::test]
async fn resource_job_and_ui_methods_use_typed_wire_contracts() {
    let (client, mut observed) = test_client(&[
        method::RESOURCE_OPEN,
        method::RESOURCE_INVOKE,
        method::JOB_START,
        method::UI_ACTION,
        method::UI_DIALOG,
        method::UI_WINDOW,
    ])
    .await;

    let opened = client
        .open_resource(&ResourceOpenParams {
            resource_type: "kafka".into(),
            config: json!({"brokers": ["localhost:9092"]}),
            metadata: None,
        })
        .await
        .expect("open resource");
    assert_eq!("resource-1", opened.resource_id);
    assert_eq!(
        (
            method::RESOURCE_OPEN.to_owned(),
            json!({
                "resource_type": "kafka",
                "config": {"brokers": ["localhost:9092"]}
            })
        ),
        observed.recv().await.expect("open request")
    );

    let invoked = client
        .invoke_resource(&ResourceInvokeParams {
            resource_id: "resource-1".into(),
            method: "kafka/topic/list".into(),
            params: json!({"include_internal": false}),
        })
        .await
        .expect("invoke resource");
    assert_eq!(
        ResultRef::Inline {
            value: json!({"topics": ["orders"]})
        },
        invoked.result
    );
    assert_eq!(
        method::RESOURCE_INVOKE,
        observed.recv().await.expect("invoke request").0
    );

    let job = client
        .start_job(&JobStartParams {
            resource_id: Some("resource-1".into()),
            method: "kafka/message/consume".into(),
            params: json!({"topic": "orders"}),
        })
        .await
        .expect("start job");
    assert_eq!(JobState::Queued, job.state);
    assert_eq!(
        method::JOB_START,
        observed.recv().await.expect("job request").0
    );

    let patch = client
        .ui_action(&UiActionRequest {
            request_id: "request-1".into(),
            action: "refresh".into(),
            source_id: "topics".into(),
            source_path: vec![0, 1],
            payload: BTreeMap::new(),
            expected_revision: Some(7),
        })
        .await
        .expect("ui action");
    assert_eq!(Some(7), patch.expected_revision);
    assert_eq!(1, patch.operations.len());
    assert_eq!(
        method::UI_ACTION,
        observed.recv().await.expect("ui request").0
    );

    let dialog_result = client
        .ui_dialog(&UiDialogRequest {
            request_id: "request-2".into(),
            dialog_id: "topic-name".into(),
            kind: UiDialogKind::Prompt,
            title: "Topic name".into(),
            message: None,
            confirm_label: None,
            cancel_label: None,
            danger: false,
            expected_revision: None,
        })
        .await
        .expect("ui dialog");
    assert_eq!(
        UiDialogResult::Prompt {
            value: "orders".into()
        },
        dialog_result
    );
    assert_eq!(
        method::UI_DIALOG,
        observed.recv().await.expect("ui dialog request").0
    );

    client
        .ui_window(&UiWindowRequest {
            request_id: "request-3".into(),
            window_id: "topic-detail".into(),
            operation: UiWindowOperation::SetTitle {
                title: "orders".into(),
            },
        })
        .await
        .expect("ui window");
    assert_eq!(
        method::UI_WINDOW,
        observed.recv().await.expect("ui window request").0
    );
}

#[tokio::test]
async fn explicit_method_declarations_reject_missing_methods_locally() {
    let (client, mut observed) = test_client(&[method::RESOURCE_OPEN]).await;
    let error = client
        .start_job(&JobStartParams {
            resource_id: None,
            method: "kubernetes/resource/list".into(),
            params: Value::Null,
        })
        .await
        .expect_err("missing declaration");

    assert!(matches!(
        error,
        HostError::NotImplemented(message) if message.contains(method::JOB_START)
    ));
    assert!(observed.try_recv().is_err());
}

#[tokio::test]
async fn invalid_ui_requests_fail_locally_without_sending_rpc() {
    let (client, mut observed) = test_client(&[method::UI_DIALOG, method::UI_WINDOW]).await;

    let dialog_error = client
        .ui_dialog(&UiDialogRequest {
            request_id: "bad id".into(),
            dialog_id: "topic-name".into(),
            kind: UiDialogKind::Prompt,
            title: "Topic name".into(),
            message: None,
            confirm_label: None,
            cancel_label: None,
            danger: false,
            expected_revision: None,
        })
        .await
        .expect_err("invalid dialog");
    assert!(matches!(
        dialog_error,
        HostError::InvalidParams { method, .. } if method == "ui/dialog"
    ));

    let window_error = client
        .ui_window(&UiWindowRequest {
            request_id: "request-1".into(),
            window_id: "topic-detail".into(),
            operation: UiWindowOperation::Open {
                title: "Topic detail".into(),
                width: 199,
                height: 768,
                panel_id: "kafka.topic-detail".into(),
                modal: false,
            },
        })
        .await
        .expect_err("invalid window");
    assert!(matches!(
        window_error,
        HostError::InvalidParams { method, .. } if method == "ui/window"
    ));

    assert!(observed.try_recv().is_err());
}

#[tokio::test]
async fn legacy_sessions_without_method_declarations_are_still_callable() {
    let (client, mut observed) = test_client(&[]).await;
    client
        .ping_resource(&ResourcePingParams {
            resource_id: "resource-1".into(),
        })
        .await
        .expect("legacy request");

    assert_eq!(
        method::RESOURCE_PING,
        observed.recv().await.expect("legacy request").0
    );
}
