//! 通用资源插件的 typed RPC facade。
//!
//! 这里仅封装稳定的机制方法。Kafka、Kubernetes 等领域操作仍通过
//! `ResourceInvokeParams::method` 或 `JobStartParams::method` 传递 namespaced
//! method，不在宿主侧扩展成领域枚举。

use extension_protocol::declarative_ui::{validate_ui_dialog_request, validate_ui_window_request};
use extension_protocol::{
    declarative_ui::{
        UiActionRequest, UiDialogRequest, UiDialogResult, UiStatePatch, UiWindowRequest,
    },
    job::{
        JobCancelParams, JobCloseParams, JobResultParams, JobResultResult, JobStartParams,
        JobStartResult, JobStatusParams, JobStatusResult,
    },
    method,
    resource::{
        ResourceCloseParams, ResourceInvokeParams, ResourceInvokeResult, ResourceOpenParams,
        ResourceOpenResult, ResourcePingParams,
    },
};
use serde::{Serialize, de::DeserializeOwned};
use std::sync::Arc;

use crate::{HostError, HostResult, ProcessRpcSession};

pub type OpenAuthorizer = Arc<dyn Fn(&ResourceOpenParams) -> HostResult<()> + Send + Sync>;

/// 在一个已协商的进程 session 上调用通用资源、任务与 Declarative UI 协议。
#[derive(Clone)]
pub struct UniversalPluginClient {
    session: Arc<ProcessRpcSession>,
    open_authorizer: Option<OpenAuthorizer>,
}

impl UniversalPluginClient {
    pub fn new(session: Arc<ProcessRpcSession>) -> Self {
        Self {
            session,
            open_authorizer: None,
        }
    }

    pub fn with_open_authorizer(mut self, authorizer: OpenAuthorizer) -> Self {
        self.open_authorizer = Some(authorizer);
        self
    }

    pub fn session(&self) -> &Arc<ProcessRpcSession> {
        &self.session
    }

    pub async fn open_resource(
        &self,
        params: &ResourceOpenParams,
    ) -> HostResult<ResourceOpenResult> {
        if let Some(authorizer) = &self.open_authorizer {
            authorizer(params)?;
        }
        self.request(method::RESOURCE_OPEN, params).await
    }

    pub async fn ping_resource(&self, params: &ResourcePingParams) -> HostResult<()> {
        self.request(method::RESOURCE_PING, params).await
    }

    pub async fn invoke_resource(
        &self,
        params: &ResourceInvokeParams,
    ) -> HostResult<ResourceInvokeResult> {
        self.request(method::RESOURCE_INVOKE, params).await
    }

    pub async fn close_resource(&self, params: &ResourceCloseParams) -> HostResult<()> {
        self.request(method::RESOURCE_CLOSE, params).await
    }

    pub async fn start_job(&self, params: &JobStartParams) -> HostResult<JobStartResult> {
        self.request(method::JOB_START, params).await
    }

    pub async fn job_status(&self, params: &JobStatusParams) -> HostResult<JobStatusResult> {
        self.request(method::JOB_STATUS, params).await
    }

    pub async fn cancel_job(&self, params: &JobCancelParams) -> HostResult<()> {
        self.request(method::JOB_CANCEL, params).await
    }

    pub async fn job_result(&self, params: &JobResultParams) -> HostResult<JobResultResult> {
        self.request(method::JOB_RESULT, params).await
    }

    pub async fn close_job(&self, params: &JobCloseParams) -> HostResult<()> {
        self.request(method::JOB_CLOSE, params).await
    }

    pub async fn ui_action(&self, params: &UiActionRequest) -> HostResult<UiStatePatch> {
        self.request(method::UI_ACTION, params).await
    }

    pub async fn ui_dialog(&self, params: &UiDialogRequest) -> HostResult<UiDialogResult> {
        validate_ui_dialog_request(params)
            .map_err(|error| HostError::invalid_params(method::UI_DIALOG, error.to_string()))?;
        self.request(method::UI_DIALOG, params).await
    }

    pub async fn ui_window(&self, params: &UiWindowRequest) -> HostResult<()> {
        validate_ui_window_request(params)
            .map_err(|error| HostError::invalid_params(method::UI_WINDOW, error.to_string()))?;
        self.request(method::UI_WINDOW, params).await
    }

    async fn request<P, R>(&self, method_name: &str, params: &P) -> HostResult<R>
    where
        P: Serialize + ?Sized,
        R: DeserializeOwned,
    {
        let negotiated = self.session.session();
        if negotiated.has_method_declarations() && !negotiated.declares_method(method_name) {
            return Err(HostError::NotImplemented(format!(
                "extension did not declare wire method `{method_name}`"
            )));
        }

        let params = serde_json::to_value(params)?;
        self.session.request(method_name, params).await
    }
}

#[cfg(test)]
#[path = "universal_plugin_tests.rs"]
mod tests;
