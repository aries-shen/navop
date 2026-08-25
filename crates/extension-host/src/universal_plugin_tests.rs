//! `UniversalPluginClient` 的进程级协议契约测试。

use std::{collections::BTreeMap, time::Duration};

use extension_protocol::{
    blob::{BlobCloseParams, BlobOpenParams, BlobReadParams},
    declarative_ui::{
        UiDialogKind, UiDialogRequest, UiDialogResult, UiWindowOperation, UiWindowRequest,
    },
    envelope::{Response, RpcMessage},
    error::{ProtocolError, error_codes},
    event_stream::{EventCloseParams, EventOpenParams, EventReadParams},
    job::{JobStartParams, JobState},
    lifecycle::InitResult,
    resource::{ResourceInvokeParams, ResourceOpenParams},
    result_ref::ResultRef,
};
use serde_json::{Value, json};
use tokio::{io::duplex, sync::mpsc};

use super::*;
use crate::{
    CancellationToken, JsonRpcClient, NegotiationConfig, ProcessRpcSessionConfig, RequestOptions,
    SpawnConfig,
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
            method::BLOB_OPEN => json!({"blob_id": "blob-1", "total_bytes": 4}),
            method::BLOB_READ => {
                json!({"data": "aGVsbG8=", "bytes_read": 4, "done": true})
            }
            method::BLOB_CLOSE => Value::Null,
            method::EVENT_OPEN => json!({"stream_id": "stream-1"}),
            method::EVENT_READ => {
                json!({"events": [{"topic": "orders"}], "closed": true, "dropped_count": 0})
            }
            method::EVENT_CLOSE => Value::Null,
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

#[tokio::test]
async fn open_authorizer_rejects_before_sending_resource_open() {
    let (client, mut observed) = test_client(&[method::RESOURCE_OPEN]).await;
    let authorizer = Arc::new(|_params: &ResourceOpenParams| -> HostResult<()> {
        Err(HostError::protocol(ProtocolError::new(
            error_codes::PERMISSION_DENIED,
            "extension is not permitted to connect to this network endpoint",
        )))
    });
    let client = client.with_open_authorizer(authorizer);

    let error = client
        .open_resource(&ResourceOpenParams {
            resource_type: "elasticsearch".into(),
            config: json!({"url": "http://127.0.0.1:9201"}),
            metadata: None,
        })
        .await
        .expect_err("permission denial");

    let HostError::Protocol(protocol) = error else {
        panic!("expected protocol permission denial, got {error:?}");
    };
    assert_eq!(error_codes::PERMISSION_DENIED, protocol.code);
    assert!(observed.try_recv().is_err());
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
        method::BLOB_OPEN,
        method::BLOB_READ,
        method::BLOB_CLOSE,
        method::EVENT_OPEN,
        method::EVENT_READ,
        method::EVENT_CLOSE,
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

    let blob = client
        .open_blob(&BlobOpenParams {
            conn_id: None,
            content_type: Some("application/json".into()),
            metadata: None,
        })
        .await
        .expect("open blob");
    assert_eq!("blob-1", blob.blob_id);
    assert_eq!(
        method::BLOB_OPEN,
        observed.recv().await.expect("blob open request").0
    );

    let chunk = client
        .read_blob(&BlobReadParams {
            blob_id: blob.blob_id.clone(),
            max_bytes: Some(4),
        })
        .await
        .expect("read blob");
    assert_eq!((4, true), (chunk.bytes_read, chunk.done));
    assert_eq!("hello", base64_decode(chunk.data));
    assert_eq!(
        method::BLOB_READ,
        observed.recv().await.expect("blob read request").0
    );

    client
        .close_blob(&BlobCloseParams {
            blob_id: blob.blob_id,
        })
        .await
        .expect("close blob");
    assert_eq!(
        method::BLOB_CLOSE,
        observed.recv().await.expect("blob close request").0
    );

    let stream = client
        .open_event_stream(&EventOpenParams {
            conn_id: None,
            kind: "kafka/messages".into(),
            capacity: Some(128),
        })
        .await
        .expect("open stream");
    assert_eq!("stream-1", stream.stream_id);
    assert_eq!(
        method::EVENT_OPEN,
        observed.recv().await.expect("stream open request").0
    );

    let batch = client
        .read_event_stream(&EventReadParams {
            stream_id: stream.stream_id.clone(),
            max_events: Some(128),
            wait_ms: Some(0),
        })
        .await
        .expect("read stream");
    assert_eq!(1, batch.events.len());
    assert!(batch.closed);
    assert_eq!(0, batch.dropped_count);
    assert_eq!(
        method::EVENT_READ,
        observed.recv().await.expect("stream read request").0
    );

    client
        .close_event_stream(&EventCloseParams {
            stream_id: stream.stream_id,
        })
        .await
        .expect("close stream");
    assert_eq!(
        method::EVENT_CLOSE,
        observed.recv().await.expect("stream close request").0
    );
}

fn base64_decode(value: String) -> String {
    // Keep the contract test dependency-free and assert only a fixed fixture.
    assert_eq!("aGVsbG8=", value);
    "hello".to_owned()
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
async fn event_read_options_propagate_cancellation_to_the_provider() {
    let (client_side, extension_side) = duplex(16 * 1024);
    let (client_reader, client_writer) = tokio::io::split(client_side);
    let (mut extension_reader, mut extension_writer) = tokio::io::split(extension_side);
    let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok(message) = recv_async::<_, RpcMessage>(&mut extension_reader).await {
            match message {
                RpcMessage::Request(request) if request.method == method::INIT => {
                    let init = InitResult::new("1.0.0")
                        .with_api("extension", "1.0")
                        .with_method(method::EVENT_READ);
                    send_async(
                        &mut extension_writer,
                        &RpcMessage::Response(Response::ok(
                            request.id,
                            serde_json::to_value(init).unwrap(),
                        )),
                    )
                    .await
                    .unwrap();
                }
                RpcMessage::Request(request) => {
                    observed_tx.send((request.method, request.params)).unwrap();
                }
                RpcMessage::Notification(notification) => {
                    observed_tx
                        .send((notification.method, notification.params))
                        .unwrap();
                    break;
                }
                RpcMessage::Response(_) => {}
            }
        }
    });

    let rpc = JsonRpcClient::start(FramedTransport::new(client_reader, client_writer));
    let config = ProcessRpcSessionConfig::new(
        SpawnConfig::new("test-provider"),
        NegotiationConfig::new("1.0.0", "instance").offer_api("extension", "1.0"),
    )
    .with_request_timeout(Duration::from_secs(1));
    let session = ProcessRpcSession::start_with_client(rpc, None, config)
        .await
        .unwrap();
    let client = UniversalPluginClient::new(Arc::new(session));
    let cancel = CancellationToken::new();
    let read_cancel = cancel.clone();
    let read = tokio::spawn(async move {
        client
            .read_event_stream_with_options(
                &EventReadParams {
                    stream_id: "stream-1".into(),
                    max_events: Some(16),
                    wait_ms: Some(1_000),
                },
                RequestOptions::default().with_cancel(read_cancel),
            )
            .await
    });

    let (method, _) = observed_rx.recv().await.unwrap();
    assert_eq!(method::EVENT_READ, method);
    cancel.cancel();
    assert!(matches!(
        read.await.unwrap(),
        Err(HostError::Cancelled { method }) if method == method::EVENT_READ
    ));
    let (method, params) = observed_rx.recv().await.unwrap();
    assert_eq!(method::CANCEL_REQUEST, method);
    assert!(params.get("id").is_some());
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
