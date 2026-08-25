//! Declarative UI 与 native IPC provider 之间的 wire DTO。
//!
//! 这些类型刻意与进程内 `declarative-ui-demo::ActionEvent` 分离，避免把
//! request id、revision 和跨进程生命周期泄漏到渲染 runtime 的内部事件模型。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiActionRequest {
    pub request_id: String,
    pub action: String,
    pub source_id: String,
    #[serde(default)]
    pub source_path: Vec<usize>,
    #[serde(default)]
    pub payload: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum UiStateOperation {
    Set { key: String, value: String },
    Remove { key: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiStatePatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
    #[serde(default)]
    pub operations: Vec<UiStateOperation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_subscriptions: Vec<UiEventSubscriptionOperation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum UiEventSubscriptionOperation {
    Subscribe {
        subscription_id: String,
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        conn_id: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capacity: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_events: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wait_ms: Option<u32>,
        state_key: String,
    },
    Unsubscribe {
        subscription_id: String,
    },
}

// ============================================================================
// Dialog / window wire contracts
//
// Provider 只能发送描述性 request；真实 GPUI window、dialog、focus 恢复、
// modal owner 和生命周期清理都由 host 的 activation manager 权威执行。
// ============================================================================

/// 请求 host 展示一个受控 dialog，并等待一次显式用户结果。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiDialogRequest {
    /// Host 去重的 request id，不承担 UI 状态 revision 语义。
    pub request_id: String,
    /// provider 侧的幂等 dialog id；同一 id 已关闭后可复用打开新 dialog。
    pub dialog_id: String,
    pub kind: UiDialogKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancel_label: Option<String>,
    /// danger=true 时 host 必须使用 destructive 确认样式与更强确认。
    #[serde(default)]
    pub danger: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiDialogKind {
    Alert,
    Confirm,
    Prompt,
}

/// Dialog 的终态必须显式建模；host 不得把关闭窗口折叠成“确认”。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum UiDialogResult {
    Confirmed,
    Cancelled,
    /// 用户按 Esc、点击关闭按钮或点击 modal mask。
    Dismissed,
    Prompt {
        value: String,
    },
}

/// 请求 host 管理一个已注册 declarative panel 的窗口生命周期。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiWindowRequest {
    pub request_id: String,
    /// host 负责按 owner runtime/panel/extension 限定该 id 的命名空间。
    pub window_id: String,
    pub operation: UiWindowOperation,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum UiWindowOperation {
    Open {
        title: String,
        width: u32,
        height: u32,
        /// 必须引用当前 extension manifest 注册的 declarative panel。
        panel_id: String,
        #[serde(default)]
        modal: bool,
    },
    Close,
    SetTitle {
        title: String,
    },
}

pub const MAX_UI_ID_BYTES: usize = 128;
pub const MAX_UI_TITLE_BYTES: usize = 512;
pub const MAX_UI_MESSAGE_BYTES: usize = 8_192;
pub const MAX_UI_LABEL_BYTES: usize = 128;
pub const MIN_UI_WINDOW_SIZE: u32 = 200;
pub const MAX_UI_WINDOW_SIZE: u32 = 16_384;
pub const MAX_UI_EVENT_KIND_BYTES: usize = 256;
pub const MAX_UI_STATE_KEY_BYTES: usize = 256;
pub const MAX_UI_EVENT_CAPACITY: u32 = 65_536;
pub const MAX_UI_EVENT_BATCH: u32 = 1_024;
pub const MAX_UI_EVENT_WAIT_MS: u32 = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UiContractError {
    #[error("UI identifier is missing, too long, or uses unsupported characters")]
    InvalidId,
    #[error("UI title is empty, too long, or contains control characters")]
    InvalidTitle,
    #[error("UI message is too long or contains unsupported control characters")]
    InvalidMessage,
    #[error("UI label is empty, too long, or contains control characters")]
    InvalidLabel,
    #[error("UI window size must be between {MIN_UI_WINDOW_SIZE} and {MAX_UI_WINDOW_SIZE} pixels")]
    InvalidWindowSize,
    #[error("UI event stream kind is empty, too long, or contains control characters")]
    InvalidEventKind,
    #[error("UI state key is empty, too long, or contains control characters")]
    InvalidStateKey,
    #[error("UI event stream limits are outside the supported range")]
    InvalidEventLimit,
}

pub fn validate_ui_state_patch(patch: &UiStatePatch) -> Result<(), UiContractError> {
    for operation in &patch.event_subscriptions {
        validate_ui_event_subscription(operation)?;
    }
    Ok(())
}

fn validate_ui_event_subscription(
    operation: &UiEventSubscriptionOperation,
) -> Result<(), UiContractError> {
    match operation {
        UiEventSubscriptionOperation::Subscribe {
            subscription_id,
            kind,
            capacity,
            max_events,
            wait_ms,
            state_key,
            ..
        } => {
            validate_id(subscription_id)?;
            validate_bounded_text(
                kind,
                MAX_UI_EVENT_KIND_BYTES,
                UiContractError::InvalidEventKind,
            )?;
            validate_bounded_text(
                state_key,
                MAX_UI_STATE_KEY_BYTES,
                UiContractError::InvalidStateKey,
            )?;
            validate_optional_limit(*capacity, MAX_UI_EVENT_CAPACITY)?;
            validate_optional_limit(*max_events, MAX_UI_EVENT_BATCH)?;
            validate_optional_limit(*wait_ms, MAX_UI_EVENT_WAIT_MS)
        }
        UiEventSubscriptionOperation::Unsubscribe { subscription_id } => {
            validate_id(subscription_id)
        }
    }
}

pub fn validate_ui_dialog_request(request: &UiDialogRequest) -> Result<(), UiContractError> {
    validate_id(&request.request_id)?;
    validate_id(&request.dialog_id)?;
    validate_title(&request.title)?;
    if let Some(message) = &request.message {
        validate_message(message)?;
    }
    if let Some(label) = &request.confirm_label {
        validate_label(label)?;
    }
    if let Some(label) = &request.cancel_label {
        validate_label(label)?;
    }
    Ok(())
}

pub fn validate_ui_window_request(request: &UiWindowRequest) -> Result<(), UiContractError> {
    validate_id(&request.request_id)?;
    validate_id(&request.window_id)?;
    match &request.operation {
        UiWindowOperation::Open {
            title,
            width,
            height,
            panel_id,
            ..
        } => {
            validate_title(title)?;
            validate_id(panel_id)?;
            validate_window_size(*width)?;
            validate_window_size(*height)
        }
        UiWindowOperation::Close => Ok(()),
        UiWindowOperation::SetTitle { title } => validate_title(title),
    }
}

fn validate_id(value: &str) -> Result<(), UiContractError> {
    (!value.is_empty()
        && value.len() <= MAX_UI_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')))
    .then_some(())
    .ok_or(UiContractError::InvalidId)
}

fn validate_title(value: &str) -> Result<(), UiContractError> {
    validate_text(value, MAX_UI_TITLE_BYTES, UiContractError::InvalidTitle)
}

fn validate_message(value: &str) -> Result<(), UiContractError> {
    if value.len() > MAX_UI_MESSAGE_BYTES
        || value
            .chars()
            .any(|char| char.is_control() && !matches!(char, '\n' | '\r' | '\t'))
    {
        return Err(UiContractError::InvalidMessage);
    }
    Ok(())
}

fn validate_label(value: &str) -> Result<(), UiContractError> {
    validate_text(value, MAX_UI_LABEL_BYTES, UiContractError::InvalidLabel)
}

fn validate_text(
    value: &str,
    max_bytes: usize,
    error: UiContractError,
) -> Result<(), UiContractError> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(error);
    }
    Ok(())
}

fn validate_bounded_text(
    value: &str,
    max_bytes: usize,
    error: UiContractError,
) -> Result<(), UiContractError> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(error);
    }
    Ok(())
}

fn validate_optional_limit(value: Option<u32>, maximum: u32) -> Result<(), UiContractError> {
    match value {
        Some(value) if value == 0 || value > maximum => Err(UiContractError::InvalidEventLimit),
        _ => Ok(()),
    }
}

fn validate_window_size(value: u32) -> Result<(), UiContractError> {
    (MIN_UI_WINDOW_SIZE..=MAX_UI_WINDOW_SIZE)
        .contains(&value)
        .then_some(())
        .ok_or(UiContractError::InvalidWindowSize)
}
