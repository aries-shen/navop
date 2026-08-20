//! Fixture-backed Elasticsearch universal resource provider.
//!
//! This trial intentionally performs no Elasticsearch network I/O. It proves the
//! end-to-end IPC path: manifest loading, package-contained spawn, init/shutdown,
//! resource lifecycle, namespaced invokes, and declarative UI state updates.

use extension_protocol::{
    envelope::{Response, RpcMessage},
    error::{ProtocolError, error_codes},
    framing::{recv_msg_async, send_msg_async},
    lifecycle::InitResult,
    method,
    resource::{
        ResourceCloseParams, ResourceInvokeParams, ResourceInvokeResult, ResourceOpenParams,
        ResourceOpenResult, ResourcePingParams,
    },
    result_ref::ResultRef,
};
use interprocess::local_socket::{
    GenericNamespaced, ToNsName,
    tokio::{Stream, prelude::*},
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Duration;

const SOCKET_ENV_VAR: &str = "ONETCLI_EXT_SOCKET";
const PROVIDER_VERSION: &str = env!("CARGO_PKG_VERSION");
const RESOURCE_TYPE: &str = "elasticsearch";
const RESOURCE_ID: &str = "fixture-elasticsearch";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

type ProviderResult = Result<Value, Box<ProtocolError>>;

fn fixtures() -> Value {
    json!([
        {
            "name": "orders",
            "health": "green",
            "docs": 12_345,
            "size_bytes": 2_048_576,
            "documents": [
                {"id": "order-1", "customer": "alice", "amount": 120},
                {"id": "order-2", "customer": "bob", "amount": 75}
            ]
        },
        {
            "name": "users",
            "health": "yellow",
            "docs": 802,
            "size_bytes": 102_400,
            "documents": [
                {"id": "user-1", "name": "alice"},
                {"id": "user-2", "name": "bob"}
            ]
        }
    ])
}

fn index_by_name(name: &str) -> Option<Value> {
    fixtures()
        .as_array()?
        .iter()
        .find(|index| index.get("name").and_then(Value::as_str) == Some(name))
        .cloned()
}

fn invoke(method_name: &str, params: &Value) -> ProviderResult {
    match method_name {
        "elasticsearch/index/list" => Ok(json!({ "indices": fixtures() })),
        "elasticsearch/index/get" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.trim().is_empty())
                .ok_or_else(|| boxed_invalid_params("index name is required"))?;
            index_by_name(name)
                .ok_or_else(|| boxed_invalid_params(format!("unknown index `{name}`")))
        }
        "elasticsearch/search" => {
            let query = params
                .get("query")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_lowercase();
            let indices: Vec<Value> = fixtures()
                .as_array()
                .into_iter()
                .flatten()
                .filter(|index| {
                    index
                        .get("documents")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .any(|document| document.to_string().to_ascii_lowercase().contains(&query))
                })
                .map(|index| {
                    let documents: Vec<Value> = index
                        .get("documents")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|document| {
                            document.to_string().to_ascii_lowercase().contains(&query)
                        })
                        .cloned()
                        .collect();
                    let mut index = index.clone();
                    if let Some(object) = index.as_object_mut() {
                        object.insert("documents".into(), Value::Array(documents));
                    }
                    index
                })
                .collect();
            Ok(json!({ "indices": indices }))
        }
        _ => Err(boxed_error(
            error_codes::METHOD_NOT_FOUND,
            format!("unknown Elasticsearch method `{method_name}`"),
        )),
    }
}

fn ui_patch(request_id: &str, action: &str) -> ProviderResult {
    if action != "refresh-resources" {
        return Err(boxed_error(
            error_codes::METHOD_NOT_FOUND,
            format!("unknown UI action `{action}`"),
        ));
    }

    let indices = serde_json::to_string(&json!({ "indices": fixtures() }))
        .map_err(|error| boxed_error(error_codes::INTERNAL_ERROR, error.to_string()))?;
    Ok(json!({
        "expected_revision": 7,
        "operations": [
            {"operation": "set", "key": "provider_status", "value": "ready"},
            {"operation": "set", "key": "indices_json", "value": indices},
            {"operation": "set", "key": "last_request_id", "value": request_id}
        ]
    }))
}

fn handle_request(request: extension_protocol::Request, opened: &mut bool) -> Response {
    let result = match request.method.as_str() {
        method::INIT => {
            *opened = false;
            serde_json::to_value(
                InitResult::new(PROVIDER_VERSION)
                    .with_api("extension", "1.0")
                    .with_method(method::RESOURCE_OPEN)
                    .with_method(method::RESOURCE_PING)
                    .with_method(method::RESOURCE_INVOKE)
                    .with_method(method::RESOURCE_CLOSE)
                    .with_method(method::UI_ACTION),
            )
            .map_err(|error| boxed_invalid_params(error.to_string()))
        }
        method::RESOURCE_OPEN => match serde_json::from_value::<ResourceOpenParams>(request.params)
        {
            Ok(params) if params.resource_type == RESOURCE_TYPE && !*opened => {
                *opened = true;
                serde_json::to_value(ResourceOpenResult {
                    resource_id: RESOURCE_ID.to_owned(),
                    capabilities: vec![
                        "elasticsearch/index/list".to_owned(),
                        "elasticsearch/index/get".to_owned(),
                        "elasticsearch/search".to_owned(),
                    ],
                    metadata: Some(json!({ "mode": "fixture", "network": false })),
                })
                .map_err(|error| boxed_invalid_params(error.to_string()))
            }
            Ok(_) => Err(boxed_invalid_params(
                "resource must be `elasticsearch` and cannot be opened twice",
            )),
            Err(error) => Err(boxed_invalid_params(error.to_string())),
        },
        method::RESOURCE_PING => match serde_json::from_value::<ResourcePingParams>(request.params)
        {
            Ok(params) if params.resource_id == RESOURCE_ID && *opened => Ok(Value::Null),
            Ok(_) => Err(resource_error()),
            Err(error) => Err(boxed_invalid_params(error.to_string())),
        },
        method::RESOURCE_INVOKE => {
            match serde_json::from_value::<ResourceInvokeParams>(request.params) {
                Ok(params) if params.resource_id == RESOURCE_ID && *opened => {
                    invoke(&params.method, &params.params).and_then(|value| {
                        serde_json::to_value(ResourceInvokeResult {
                            result: ResultRef::Inline { value },
                        })
                        .map_err(|error| boxed_invalid_params(error.to_string()))
                    })
                }
                Ok(_) => Err(resource_error()),
                Err(error) => Err(boxed_invalid_params(error.to_string())),
            }
        }
        method::RESOURCE_CLOSE => {
            match serde_json::from_value::<ResourceCloseParams>(request.params) {
                Ok(params) if params.resource_id == RESOURCE_ID && *opened => {
                    *opened = false;
                    Ok(Value::Null)
                }
                Ok(_) => Err(resource_error()),
                Err(error) => Err(boxed_invalid_params(error.to_string())),
            }
        }
        method::UI_ACTION => {
            let request_id = request
                .params
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let action = request
                .params
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            ui_patch(&request_id, &action)
        }
        method::SHUTDOWN => {
            *opened = false;
            Ok(Value::Null)
        }
        _ => Err(boxed_error(
            error_codes::METHOD_NOT_FOUND,
            format!("unknown method `{}`", request.method),
        )),
    };

    match result {
        Ok(result) => Response::ok(request.id, result),
        Err(error) => Response::err(request.id, *error),
    }
}

fn boxed_error(
    code: extension_protocol::error::ErrorCode,
    message: impl Into<String>,
) -> Box<ProtocolError> {
    Box::new(ProtocolError::new(code, message))
}

fn boxed_invalid_params(message: impl Into<String>) -> Box<ProtocolError> {
    boxed_error(error_codes::INVALID_PARAMS, message)
}

fn resource_error() -> Box<ProtocolError> {
    Box::new(ProtocolError::new(
        error_codes::RESOURCE_CLOSED,
        "Elasticsearch resource is not open",
    ))
}

#[tokio::main]
async fn main() {
    let socket_name = std::env::var(SOCKET_ENV_VAR).unwrap_or_else(|error| {
        eprintln!("missing {SOCKET_ENV_VAR}: {error}");
        std::process::exit(2);
    });
    let name = socket_name
        .to_ns_name::<GenericNamespaced>()
        .expect("valid host-provided local socket name");
    let stream = match tokio::time::timeout(CONNECT_TIMEOUT, Stream::connect(name)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            eprintln!("failed to connect extension socket: {error}");
            std::process::exit(3);
        }
        Err(_) => {
            eprintln!("timed out connecting extension socket");
            std::process::exit(4);
        }
    };

    let (reader, writer) = tokio::io::split(stream);
    let (reader, mut writer) = run(reader, writer).await;
    let _ = writer.shutdown().await;
    let _ = reader;
}

async fn run<R, W>(mut reader: R, mut writer: W) -> (R, W)
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut opened = false;
    while let Ok(message) = recv_msg_async(&mut reader).await {
        let RpcMessage::Request(request) = message else {
            continue;
        };
        let should_exit = request.method == method::SHUTDOWN;
        let response = handle_request(request, &mut opened);
        if send_msg_async(&mut writer, &RpcMessage::Response(response))
            .await
            .is_err()
        {
            break;
        }
        if should_exit {
            break;
        }
    }
    (reader, writer)
}
