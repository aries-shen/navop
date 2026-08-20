//! Read-only Elasticsearch universal resource provider.
//!
//! The provider owns only transport translation. Elasticsearch permissions
//! remain host-authoritative: connection endpoints are checked by the host
//! before `resource/open`, and API keys are resolved through the reverse Host
//! API after the extension manifest's `secrets:read:*` permission is checked.

use extension_protocol::{
    conn::SecretRef,
    declarative_ui::{UiActionRequest, UiStateOperation, UiStatePatch},
    envelope::{Request, RequestId, Response, RpcMessage},
    error::{ProtocolError, error_codes},
    framing::{recv_msg_async, send_msg_async},
    host::ResolveSecretParams,
    lifecycle::InitResult,
    method,
    resource::{
        ResourceCloseParams, ResourceInvokeParams, ResourceInvokeResult, ResourceOpenParams,
        ResourceOpenResult, ResourcePingParams,
    },
    result_ref::ResultRef,
};
use futures::StreamExt;
use interprocess::local_socket::{
    GenericNamespaced, ToNsName,
    tokio::{Stream, prelude::*},
};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicI64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::Duration;
use url::Url;

const SOCKET_ENV_VAR: &str = "ONETCLI_EXT_SOCKET";
const PROVIDER_VERSION: &str = env!("CARGO_PKG_VERSION");
const RESOURCE_TYPE: &str = "elasticsearch";
const RESOURCE_ID: &str = "elasticsearch-resource";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HTTP_BODY_BYTES: usize = 8 * 1024 * 1024;

type ProviderResult = Result<Value, Box<ProtocolError>>;

#[derive(Debug)]
struct ElasticsearchResource {
    base_url: String,
    api_key: Vec<u8>,
}

struct ProviderState {
    resource: Option<ElasticsearchResource>,
    next_reverse_request_id: AtomicI64,
}

struct IpcParts<R, W>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    reader: R,
    writer: W,
}

impl ProviderState {
    fn new() -> Self {
        Self {
            resource: None,
            next_reverse_request_id: AtomicI64::new(1),
        }
    }
}

async fn resolve_secret<R, W>(
    ipc: &mut IpcParts<R, W>,
    secret_ref: &SecretRef,
    next_id: &AtomicI64,
) -> Result<Vec<u8>, Box<ProtocolError>>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let id = RequestId::Number(next_id.fetch_add(1, Ordering::SeqCst));
    let request = Request::new(
        id,
        method::HOST_RESOLVE_SECRET,
        serde_json::to_value(ResolveSecretParams {
            secret_ref: secret_ref.clone(),
        })
        .map_err(|error| boxed_invalid_params(error.to_string()))?,
    );
    send_msg_async(&mut ipc.writer, &RpcMessage::Request(request))
        .await
        .map_err(|error| {
            boxed_error(
                error_codes::INTERNAL_ERROR,
                format!("failed to request secret resolution: {error}"),
            )
        })?;

    let message = recv_msg_async::<_, RpcMessage>(&mut ipc.reader)
        .await
        .map_err(|error| {
            boxed_error(
                error_codes::INTERNAL_ERROR,
                format!("failed to receive secret resolution response: {error}"),
            )
        })?;
    let RpcMessage::Response(response) = message else {
        return Err(boxed_error(
            error_codes::INTERNAL_ERROR,
            "secret resolution returned an invalid RPC response",
        ));
    };
    if let Some(error) = response.error() {
        return Err(Box::new(error.clone()));
    }
    let Some(result_value) = response.result() else {
        return Err(boxed_error(
            error_codes::INTERNAL_ERROR,
            "secret resolution returned neither a result nor an error",
        ));
    };
    let result: extension_protocol::host::ResolveSecretResult =
        serde_json::from_value(result_value.clone())
            .map_err(|error| boxed_invalid_params(error.to_string()))?;
    Ok(result.value)
}

fn parse_open_params(params: Value) -> Result<(String, SecretRef), Box<ProtocolError>> {
    let params: ResourceOpenParams =
        serde_json::from_value(params).map_err(|error| boxed_invalid_params(error.to_string()))?;
    if params.resource_type != RESOURCE_TYPE {
        return Err(boxed_invalid_params(format!(
            "resource type must be `{RESOURCE_TYPE}`"
        )));
    }
    let url = params
        .config
        .get("url")
        .and_then(Value::as_str)
        .and_then(|value| value.trim().parse::<Url>().ok())
        .ok_or_else(|| boxed_invalid_params("a valid `http` or `https` `url` is required"))?;
    if !matches!(url.scheme(), "http" | "https")
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(boxed_invalid_params(
            "`url` must be an HTTP(S) endpoint without path, query, or fragment",
        ));
    }
    let credential = params
        .config
        .get("credential_ref")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| boxed_invalid_params("`credential_ref` is required"))?;
    Ok((url.to_string(), SecretRef::new(credential)))
}

async fn execute(
    resource: &ElasticsearchResource,
    method_name: &str,
    params: &Value,
) -> ProviderResult {
    let client = reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|error| boxed_error(error_codes::INTERNAL_ERROR, error.to_string()))?;
    let mut request = match method_name {
        "elasticsearch/cluster/info" => client.get(resource.base_url.as_str()),
        "elasticsearch/index/list" => client
            .get(format!("{}/_cat/indices?format=json", resource.base_url))
            .header("Accept", "application/json"),
        "elasticsearch/index/get" => {
            let index = index_name(params)?;
            client
                .get(format!("{}/{}", resource.base_url, index))
                .header("Accept", "application/json")
        }
        "elasticsearch/search" => client
            .post(format!("{}/_search", resource.base_url))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
            .body(search_body(params)?),
        _ => {
            return Err(boxed_error(
                error_codes::METHOD_NOT_FOUND,
                format!("unknown Elasticsearch method `{method_name}`"),
            ));
        }
    };
    request = request.header(
        "Authorization",
        format!("ApiKey {}", String::from_utf8_lossy(&resource.api_key)),
    );
    let response = request.send().await.map_err(|_error| {
        boxed_error(
            error_codes::IO_CONNECTION_REFUSED,
            "Elasticsearch request failed",
        )
    })?;
    let status = response.status();
    if !status.is_success() {
        let _body = bounded_body(response).await?;
        return Err(boxed_error(
            error_codes::IO_CONNECTION_REFUSED,
            format!("Elasticsearch returned HTTP status {status}; response body omitted"),
        ));
    }
    let body = bounded_body(response).await?;
    serde_json::from_slice::<Value>(&body).map_err(|_error| {
        boxed_error(
            error_codes::DATA_INVALID_ENCODING,
            "Elasticsearch returned invalid JSON",
        )
    })
}

async fn validate_connection(resource: &ElasticsearchResource) -> Result<(), Box<ProtocolError>> {
    let value = execute(resource, "elasticsearch/cluster/info", &Value::Null).await?;
    if !value.is_object() {
        return Err(boxed_error(
            error_codes::DATA_INVALID_ENCODING,
            "Elasticsearch returned an invalid cluster information response",
        ));
    }
    Ok(())
}

async fn bounded_body(response: reqwest::Response) -> Result<Vec<u8>, Box<ProtocolError>> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_error| {
            boxed_error(
                error_codes::IO_CONNECTION_REFUSED,
                "Elasticsearch response read failed",
            )
        })?;
        if body.len() + chunk.len() > MAX_HTTP_BODY_BYTES {
            return Err(boxed_error(
                error_codes::DATA_VALUE_OUT_OF_RANGE,
                "Elasticsearch response exceeds the provider limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn index_name(params: &Value) -> Result<String, Box<ProtocolError>> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .ok_or_else(|| boxed_invalid_params("index name is required"))?;
    if name.len() > 256 || name.contains('/') || name.contains('?') || name.contains('#') {
        return Err(boxed_invalid_params("invalid index name"));
    }
    Ok(name.to_owned())
}

fn search_body(params: &Value) -> Result<String, Box<ProtocolError>> {
    let query = params
        .get("query")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|query| !query.is_empty())
        .ok_or_else(|| boxed_invalid_params("non-empty search query is required"))?;
    let body = json!({ "query": { "match": { "_all": query } } });
    serde_json::to_string(&body).map_err(|error| boxed_invalid_params(error.to_string()))
}

fn normalize_indices(value: Value) -> Value {
    let Some(indices) = value.as_array() else {
        return json!({ "indices": value });
    };
    let normalized: Vec<Value> = indices
        .iter()
        .map(|index| {
            json!({
                "name": index.get("index").or_else(|| index.get("name")).cloned().unwrap_or(Value::Null),
                "health": index.get("health").cloned().unwrap_or(Value::Null),
                "docs": index.get("docs.count").cloned().unwrap_or(Value::Null),
                "size_bytes": index.get("store.size").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({ "indices": normalized })
}

fn normalize_index(value: Value) -> Value {
    value
}

fn normalize_search(value: Value) -> Value {
    json!({ "raw": value })
}

async fn handle_request<R, W>(
    ipc: &mut IpcParts<R, W>,
    state: &mut ProviderState,
    request: extension_protocol::Request,
) -> (Response, bool)
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let should_exit = request.method == method::SHUTDOWN;
    let result = match request.method.as_str() {
        method::INIT => serde_json::to_value(
            InitResult::new(PROVIDER_VERSION)
                .with_api("extension", "1.0")
                .with_method(method::RESOURCE_OPEN)
                .with_method(method::RESOURCE_PING)
                .with_method(method::RESOURCE_INVOKE)
                .with_method(method::RESOURCE_CLOSE)
                .with_method(method::UI_ACTION),
        )
        .map_err(|error| boxed_invalid_params(error.to_string())),
        method::RESOURCE_OPEN => {
            match state.resource.is_none() {
                true => {
                    let (url, credential_ref) = match parse_open_params(request.params.clone()) {
                        Ok(value) => value,
                        Err(error) => return (Response::err(request.id, *error), false),
                    };
                    let api_key =
                        match resolve_secret(ipc, &credential_ref, &state.next_reverse_request_id)
                            .await
                        {
                            Ok(value) => value,
                            Err(error) => return (Response::err(request.id, *error), false),
                        };
                    let resource = ElasticsearchResource {
                        base_url: url.trim_end_matches('/').to_owned(),
                        api_key,
                    };
                    if let Err(error) = validate_connection(&resource).await {
                        return (Response::err(request.id, *error), false);
                    }
                    state.resource = Some(resource);
                    serde_json::to_value(ResourceOpenResult {
                        resource_id: RESOURCE_ID.to_owned(),
                        capabilities: vec![
                            "elasticsearch/index/list".to_owned(),
                            "elasticsearch/index/get".to_owned(),
                            "elasticsearch/search".to_owned(),
                        ],
                        metadata: Some(
                            json!({ "mode": "http", "network": true, "operations": "read-only" }),
                        ),
                    })
                    .map_err(|error| boxed_invalid_params(error.to_string()))
                }
                false => Err(boxed_invalid_params(
                    "Elasticsearch resource is already open",
                )),
            }
        }
        method::RESOURCE_PING => {
            let params: ResourcePingParams = match serde_json::from_value(request.params) {
                Ok(value) => value,
                Err(error) => {
                    return (
                        Response::err(request.id, *boxed_invalid_params(error.to_string())),
                        false,
                    );
                }
            };
            if &params.resource_id != RESOURCE_ID || state.resource.is_none() {
                return (Response::err(request.id, *resource_error()), false);
            }
            Ok(Value::Null)
        }
        method::RESOURCE_INVOKE => {
            let params: ResourceInvokeParams = match serde_json::from_value(request.params) {
                Ok(value) => value,
                Err(error) => {
                    return (
                        Response::err(request.id, *boxed_invalid_params(error.to_string())),
                        false,
                    );
                }
            };
            let Some(resource) = state.resource.as_ref() else {
                return (Response::err(request.id, *resource_error()), false);
            };
            execute(resource, &params.method, &params.params)
                .await
                .map(|value| {
                    let value = match params.method.as_str() {
                        "elasticsearch/index/list" => normalize_indices(value),
                        "elasticsearch/index/get" => normalize_index(value),
                        "elasticsearch/search" => normalize_search(value),
                        _ => value,
                    };
                    serde_json::to_value(ResourceInvokeResult {
                        result: ResultRef::Inline { value },
                    })
                })
                .and_then(|value| value.map_err(|error| boxed_invalid_params(error.to_string())))
        }
        method::RESOURCE_CLOSE => {
            let params: ResourceCloseParams = match serde_json::from_value(request.params) {
                Ok(value) => value,
                Err(error) => {
                    return (
                        Response::err(request.id, *boxed_invalid_params(error.to_string())),
                        false,
                    );
                }
            };
            if &params.resource_id != RESOURCE_ID || state.resource.is_none() {
                return (Response::err(request.id, *resource_error()), false);
            }
            state.resource = None;
            Ok(Value::Null)
        }
        method::UI_ACTION => {
            let params: UiActionRequest = match serde_json::from_value(request.params) {
                Ok(value) => value,
                Err(error) => {
                    return (
                        Response::err(request.id, *boxed_invalid_params(error.to_string())),
                        false,
                    );
                }
            };
            let Some(resource) = state.resource.as_ref() else {
                return (Response::err(request.id, *resource_error()), false);
            };
            ui_patch(resource, &params).await
        }
        method::SHUTDOWN => {
            state.resource = None;
            Ok(Value::Null)
        }
        _ => Err(boxed_error(
            error_codes::METHOD_NOT_FOUND,
            format!("unknown method `{}`", request.method),
        )),
    };
    let response = match result {
        Ok(result) => Response::ok(request.id, result),
        Err(error) => Response::err(request.id, *error),
    };
    (response, should_exit)
}

async fn ui_patch(resource: &ElasticsearchResource, request: &UiActionRequest) -> ProviderResult {
    if request.action != "refresh-resources" {
        return Err(boxed_error(
            error_codes::METHOD_NOT_FOUND,
            format!("unknown UI action `{}`", request.action),
        ));
    }
    let indices = execute(resource, "elasticsearch/index/list", &Value::Null).await?;
    let normalized = normalize_indices(indices);
    let indices_json = serde_json::to_string(&normalized)
        .map_err(|error| boxed_invalid_params(error.to_string()))?;
    let patch = UiStatePatch {
        expected_revision: request.expected_revision,
        operations: vec![
            UiStateOperation::Set {
                key: "provider_status".to_owned(),
                value: "ready".to_owned(),
            },
            UiStateOperation::Set {
                key: "indices_json".to_owned(),
                value: indices_json,
            },
            UiStateOperation::Set {
                key: "last_request_id".to_owned(),
                value: request.request_id.clone(),
            },
        ],
    };
    serde_json::to_value(patch).map_err(|error| boxed_invalid_params(error.to_string()))
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

async fn run<R, W>(mut reader: R, mut writer: W) -> (R, W)
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut state = ProviderState::new();
    while let Ok(message) = recv_msg_async::<_, RpcMessage>(&mut reader).await {
        let RpcMessage::Request(request) = message else {
            continue;
        };
        let mut ipc = IpcParts { reader, writer };
        let (response, should_exit) = handle_request(&mut ipc, &mut state, request).await;
        reader = ipc.reader;
        writer = ipc.writer;
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
