//! Host-owned activation lifecycle for provider-initiated declarative dialogs.
//!
//! A provider may only submit a validated declarative request. The host owns
//! request identity, pending capacity, presentation, focus, the GPUI modal, and
//! all cleanup. In particular, provider code never receives a window or dialog
//! object and cannot choose a native close outcome.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use extension_host::{HostError, HostResult};
use extension_protocol::declarative_ui::{
    UiDialogRequest, UiDialogResult, validate_ui_dialog_request,
};
use parking_lot::Mutex as SyncMutex;
use tokio::sync::oneshot;

/// Default maximum number of dialogs one provider generation may have pending.
pub const DEFAULT_MAX_PENDING_DIALOGS: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DialogActivationKey {
    pub extension_id: String,
    pub runtime_id: String,
    pub generation: u64,
    pub request_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogActivationRequest {
    pub key: DialogActivationKey,
    pub dialog: UiDialogRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialogTerminalResult {
    Confirmed,
    Cancelled,
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogUserResult {
    Terminal(DialogTerminalResult),
    Prompt(String),
}

impl From<DialogUserResult> for UiDialogResult {
    fn from(result: DialogUserResult) -> Self {
        match result {
            DialogUserResult::Terminal(DialogTerminalResult::Confirmed) => Self::Confirmed,
            DialogUserResult::Terminal(DialogTerminalResult::Cancelled) => Self::Cancelled,
            DialogUserResult::Terminal(DialogTerminalResult::Dismissed) => Self::Dismissed,
            DialogUserResult::Prompt(value) => Self::Prompt { value },
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DialogActivationError {
    #[error("dialog request validation failed: {0}")]
    InvalidRequest(String),
    #[error("runtime `{runtime_id}` generation `{generation}` is not active")]
    StaleGeneration { runtime_id: String, generation: u64 },
    #[error("dialog request `{request_id}` is already pending")]
    DuplicateRequest { request_id: String },
    #[error("runtime `{runtime_id}` has reached its pending dialog limit")]
    TooManyPending { runtime_id: String },
}

/// A presenter owns the actual GPUI modal/window.
///
/// It receives only declarative data and a completion sender. Returning from
/// `show` must not imply completion; the presenter finishes the oneshot after a
/// user action or host-owned cleanup.
#[async_trait::async_trait]
pub trait DialogPresenter: Send + Sync {
    async fn show(
        &self,
        request: DialogActivationRequest,
        complete: oneshot::Sender<DialogUserResult>,
    );

    /// Closes a request already handed to the presenter.
    ///
    /// The presenter must resolve it as `Dismissed` and must make this
    /// operation idempotent. The host calls this when it retires a runtime
    /// generation before the presenter has returned from `show`.
    fn dismiss(&self, request: &DialogActivationRequest);
}

/// A presenter used before a real GPUI integration is mounted.
#[derive(Debug, Default)]
pub struct QueueingDialogPresenter {
    queued: SyncMutex<Vec<(DialogActivationRequest, oneshot::Sender<DialogUserResult>)>>,
}

impl QueueingDialogPresenter {
    /// Removes and completes queued requests with an explicit host dismissal.
    ///
    /// A real GPUI presenter will finish each entry only after a user action or
    /// native cleanup. This fallback remains useful for tests and bootstrapping
    /// because it never converts a cancelled presentation into confirmation.
    pub fn dismiss_all(&self) {
        for (_request, complete) in self.queued.lock().drain(..) {
            let _ = complete.send(DialogUserResult::Terminal(DialogTerminalResult::Dismissed));
        }
    }

    pub fn take_requests(&self) -> Vec<DialogActivationRequest> {
        self.queued
            .lock()
            .drain(..)
            .map(|(request, _)| request)
            .collect()
    }

    pub fn dismiss(&self, request: &DialogActivationRequest) {
        let mut queued = self.queued.lock();
        if let Some(index) = queued
            .iter()
            .position(|(queued_request, _)| queued_request.key == request.key)
        {
            let (_, complete) = queued.remove(index);
            let _ = complete.send(DialogUserResult::Terminal(DialogTerminalResult::Dismissed));
        }
    }
}

#[async_trait::async_trait]
impl DialogPresenter for QueueingDialogPresenter {
    async fn show(
        &self,
        request: DialogActivationRequest,
        complete: oneshot::Sender<DialogUserResult>,
    ) {
        self.queued.lock().push((request, complete));
    }

    fn dismiss(&self, request: &DialogActivationRequest) {
        self.dismiss(request);
    }
}

#[derive(Default)]
struct DialogActivationState {
    pending: BTreeMap<DialogActivationKey, PendingDialog>,
    runtime_generations: BTreeMap<String, u64>,
}

struct PendingDialog {
    request: UiDialogRequest,
    complete: Option<oneshot::Sender<DialogUserResult>>,
}

/// Namespaces and bounds provider dialogs by runtime generation.
pub struct DialogActivationManager {
    presenter: Arc<dyn DialogPresenter>,
    max_pending_per_runtime: usize,
    state: Arc<SyncMutex<DialogActivationState>>,
}

impl DialogActivationManager {
    pub fn new(presenter: Arc<dyn DialogPresenter>) -> Self {
        Self {
            presenter,
            max_pending_per_runtime: DEFAULT_MAX_PENDING_DIALOGS,
            state: Arc::new(SyncMutex::new(DialogActivationState::default())),
        }
    }

    pub fn with_max_pending_per_runtime(mut self, limit: usize) -> Self {
        self.max_pending_per_runtime = limit.max(1);
        self
    }

    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    pub fn mark_runtime_active(&self, runtime_id: &str, generation: u64) {
        self.state
            .lock()
            .runtime_generations
            .insert(runtime_id.to_owned(), generation);
    }

    /// Presents a dialog and waits for one explicit user result.
    ///
    /// If the provider is restarted, deactivated, or the generation changes,
    /// the pending request is resolved as `Dismissed`. This is a terminal host
    /// cleanup result rather than a fabricated user confirmation.
    pub async fn show(
        &self,
        extension_id: &str,
        runtime_id: &str,
        generation: u64,
        request: UiDialogRequest,
    ) -> HostResult<UiDialogResult> {
        validate_ui_dialog_request(&request).map_err(|error| {
            dialog_protocol_error(DialogActivationError::InvalidRequest(error.to_string()))
        })?;

        let key = DialogActivationKey {
            extension_id: extension_id.to_owned(),
            runtime_id: runtime_id.to_owned(),
            generation,
            request_id: request.request_id.clone(),
        };
        let activation = DialogActivationRequest {
            key: key.clone(),
            dialog: request.clone(),
        };
        let (complete_tx, complete_rx) = oneshot::channel();
        let presenter_tx;

        {
            let mut state = self.state.lock();
            let active_generation = state.runtime_generations.get(runtime_id).copied();
            if active_generation != Some(generation) {
                return Err(dialog_protocol_error(
                    DialogActivationError::StaleGeneration {
                        runtime_id: runtime_id.to_owned(),
                        generation,
                    },
                ));
            }
            if state.pending.contains_key(&key) {
                return Err(dialog_protocol_error(
                    DialogActivationError::DuplicateRequest {
                        request_id: key.request_id,
                    },
                ));
            }
            let runtime_pending = state
                .pending
                .keys()
                .filter(|pending| pending.runtime_id == runtime_id)
                .count();
            if runtime_pending >= self.max_pending_per_runtime {
                return Err(dialog_protocol_error(
                    DialogActivationError::TooManyPending {
                        runtime_id: runtime_id.to_owned(),
                    },
                ));
            }
            state.pending.insert(
                key.clone(),
                PendingDialog {
                    request: request.clone(),
                    complete: Some(complete_tx),
                },
            );
            presenter_tx = state
                .pending
                .get_mut(&key)
                .and_then(|pending| pending.complete.take())
                .expect("pending sender was just inserted");
        }

        self.presenter.show(activation, presenter_tx).await;

        let user_result = complete_rx
            .await
            .unwrap_or(DialogUserResult::Terminal(DialogTerminalResult::Dismissed));
        let still_pending = self.state.lock().pending.remove(&key).is_some();
        let user_result = if still_pending {
            user_result
        } else {
            DialogUserResult::Terminal(DialogTerminalResult::Dismissed)
        };
        Ok(user_result.into())
    }

    /// Marks a runtime inactive and dismisses every pending request it owns.
    pub fn remove_runtime(&self, runtime_id: &str) {
        let removed: Vec<_> = {
            let mut state = self.state.lock();
            state.runtime_generations.remove(runtime_id);
            state
                .pending
                .keys()
                .filter(|key| key.runtime_id == runtime_id)
                .cloned()
                .collect()
        };
        for key in removed {
            self.dismiss_pending(&key);
        }
    }

    /// Marks a generation retired and dismisses only requests from that generation.
    pub fn retire_generation(&self, runtime_id: &str, generation: u64) {
        let removed: Vec<_> = {
            let mut state = self.state.lock();
            if state
                .runtime_generations
                .get(runtime_id)
                .is_none_or(|active| *active == generation)
            {
                state.runtime_generations.remove(runtime_id);
            }
            state
                .pending
                .keys()
                .filter(|key| key.runtime_id == runtime_id && key.generation == generation)
                .cloned()
                .collect()
        };
        for key in removed {
            self.dismiss_pending(&key);
        }
    }

    pub fn remove_extension(&self, extension_id: &str) {
        let removed: Vec<_> = {
            let mut state = self.state.lock();
            let runtime_ids: BTreeSet<String> = state
                .pending
                .keys()
                .filter(|key| key.extension_id == extension_id)
                .map(|key| key.runtime_id.clone())
                .collect();
            for runtime_id in runtime_ids {
                state.runtime_generations.remove(&runtime_id);
            }
            state
                .pending
                .keys()
                .filter(|key| key.extension_id == extension_id)
                .cloned()
                .collect()
        };
        for key in removed {
            self.dismiss_pending(&key);
        }
    }

    pub fn pending_count(&self, runtime_id: &str) -> usize {
        self.state
            .lock()
            .pending
            .keys()
            .filter(|key| key.runtime_id == runtime_id)
            .count()
    }

    pub fn pending_keys(&self) -> BTreeSet<DialogActivationKey> {
        self.state.lock().pending.keys().cloned().collect()
    }

    fn dismiss_pending(&self, key: &DialogActivationKey) {
        let Some(pending) = self.state.lock().pending.remove(key) else {
            return;
        };
        if let Some(complete) = pending.complete {
            let _ = complete.send(DialogUserResult::Terminal(DialogTerminalResult::Dismissed));
        } else {
            self.presenter.dismiss(&DialogActivationRequest {
                key: key.clone(),
                dialog: pending.request,
            });
        }
    }
}

fn dialog_protocol_error(error: DialogActivationError) -> HostError {
    let code = match &error {
        DialogActivationError::InvalidRequest(_) => {
            extension_protocol::error::error_codes::INVALID_PARAMS
        }
        DialogActivationError::StaleGeneration { .. } => {
            extension_protocol::error::error_codes::PERMISSION_DENIED
        }
        DialogActivationError::DuplicateRequest { .. }
        | DialogActivationError::TooManyPending { .. } => {
            extension_protocol::error::error_codes::RESOURCE_BUSY
        }
    };
    HostError::protocol(extension_protocol::ProtocolError::new(
        code,
        error.to_string(),
    ))
}

/// Reverse Host API provider for provider-initiated dialogs.
pub struct DialogHostProvider {
    extension_id: String,
    runtime_id: String,
    generation: u64,
    dialogs: Arc<DialogActivationManager>,
    upstream: Arc<dyn extension_host::HostApiProvider>,
}

impl DialogHostProvider {
    pub fn new(
        extension_id: impl Into<String>,
        runtime_id: impl Into<String>,
        generation: u64,
        dialogs: Arc<DialogActivationManager>,
        upstream: Arc<dyn extension_host::HostApiProvider>,
    ) -> Self {
        Self {
            extension_id: extension_id.into(),
            runtime_id: runtime_id.into(),
            generation,
            dialogs,
            upstream,
        }
    }
}

#[async_trait::async_trait]
impl extension_host::HostApiProvider for DialogHostProvider {
    async fn request_credential(
        &self,
        params: extension_protocol::host::RequestCredentialParams,
    ) -> HostResult<extension_protocol::host::RequestCredentialResult> {
        self.upstream.request_credential(params).await
    }

    async fn resolve_secret(
        &self,
        params: extension_protocol::host::ResolveSecretParams,
    ) -> HostResult<extension_protocol::host::ResolveSecretResult> {
        self.upstream.resolve_secret(params).await
    }

    async fn notify(
        &self,
        params: extension_protocol::host::NotifyParams,
    ) -> HostResult<extension_protocol::host::NotifyResult> {
        self.upstream.notify(params).await
    }

    async fn quick_pick(
        &self,
        params: extension_protocol::host::QuickPickParams,
    ) -> HostResult<extension_protocol::host::QuickPickResult> {
        self.upstream.quick_pick(params).await
    }

    async fn open_view(&self, params: extension_protocol::host::OpenViewParams) -> HostResult<()> {
        self.upstream.open_view(params).await
    }

    async fn storage_get(
        &self,
        params: extension_protocol::host::StorageGetParams,
    ) -> HostResult<extension_protocol::host::StorageGetResult> {
        self.upstream.storage_get(params).await
    }

    async fn storage_set(
        &self,
        params: extension_protocol::host::StorageSetParams,
    ) -> HostResult<()> {
        self.upstream.storage_set(params).await
    }

    async fn log(&self, params: extension_protocol::host::LogParams) -> HostResult<()> {
        self.upstream.log(params).await
    }

    async fn show_dialog(&self, params: UiDialogRequest) -> HostResult<UiDialogResult> {
        self.dialogs
            .show(
                &self.extension_id,
                &self.runtime_id,
                self.generation,
                params,
            )
            .await
    }

    async fn host_blob_begin(
        &self,
        params: extension_protocol::host_blob::HostBlobBeginParams,
    ) -> HostResult<extension_protocol::host_blob::HostBlobBeginResult> {
        self.upstream.host_blob_begin(params).await
    }

    async fn host_blob_write(
        &self,
        params: extension_protocol::host_blob::HostBlobWriteParams,
    ) -> HostResult<extension_protocol::host_blob::HostBlobWriteResult> {
        self.upstream.host_blob_write(params).await
    }

    async fn host_blob_finish(
        &self,
        params: extension_protocol::host_blob::HostBlobFinishParams,
    ) -> HostResult<extension_protocol::host_blob::HostBlobFinishResult> {
        self.upstream.host_blob_finish(params).await
    }

    async fn host_blob_abort(
        &self,
        params: extension_protocol::host_blob::HostBlobAbortParams,
    ) -> HostResult<()> {
        self.upstream.host_blob_abort(params).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension_protocol::declarative_ui::UiDialogKind;

    struct CompletePresenter(DialogUserResult);

    #[async_trait::async_trait]
    impl DialogPresenter for CompletePresenter {
        async fn show(
            &self,
            _request: DialogActivationRequest,
            complete: oneshot::Sender<DialogUserResult>,
        ) {
            let _ = complete.send(self.0.clone());
        }

        fn dismiss(&self, _request: &DialogActivationRequest) {}
    }

    #[derive(Default)]
    struct HeldPresenter {
        requests: SyncMutex<Vec<DialogActivationRequest>>,
        completions: SyncMutex<BTreeMap<DialogActivationKey, oneshot::Sender<DialogUserResult>>>,
    }

    #[async_trait::async_trait]
    impl DialogPresenter for HeldPresenter {
        async fn show(
            &self,
            request: DialogActivationRequest,
            complete: oneshot::Sender<DialogUserResult>,
        ) {
            self.requests.lock().push(request.clone());
            self.completions.lock().insert(request.key, complete);
        }

        fn dismiss(&self, request: &DialogActivationRequest) {
            if let Some(complete) = self.completions.lock().remove(&request.key) {
                let _ = complete.send(DialogUserResult::Terminal(DialogTerminalResult::Dismissed));
            }
        }
    }

    fn request(id: &str) -> UiDialogRequest {
        UiDialogRequest {
            request_id: id.into(),
            dialog_id: "delete-topic".into(),
            kind: UiDialogKind::Confirm,
            title: "Delete topic".into(),
            message: Some("This operation cannot be undone.".into()),
            confirm_label: None,
            cancel_label: None,
            danger: true,
            expected_revision: None,
        }
    }

    #[tokio::test]
    async fn dialogs_wait_for_explicit_user_results() {
        let manager = DialogActivationManager::new(Arc::new(CompletePresenter(
            DialogUserResult::Prompt("orders".into()),
        )));
        manager.mark_runtime_active("runtime", 0);

        let result = manager
            .show("extension", "runtime", 0, request("request-1"))
            .await
            .unwrap();

        assert_eq!(
            UiDialogResult::Prompt {
                value: "orders".into()
            },
            result
        );
        assert_eq!(0, manager.pending_count("runtime"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_and_stale_requests_are_rejected() {
        let presenter = Arc::new(QueueingDialogPresenter::default());
        let manager = Arc::new(DialogActivationManager::new(
            Arc::clone(&presenter) as Arc<dyn DialogPresenter>
        ));
        manager.mark_runtime_active("runtime", 0);

        let first = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .show("extension", "runtime", 0, request("same"))
                    .await
            }
        });
        while manager.pending_count("runtime") == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        presenter.dismiss_all();
        manager.mark_runtime_active("runtime", 1);
        assert_eq!(
            UiDialogResult::Dismissed,
            first
                .await
                .expect("dialog task succeeds")
                .expect("queued fallback can be dismissed")
        );

        let error = manager
            .show("extension", "runtime", 0, request("other"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("generation"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_requests_are_rejected_only_while_pending() {
        let presenter = Arc::new(QueueingDialogPresenter::default());
        let manager = Arc::new(DialogActivationManager::new(
            Arc::clone(&presenter) as Arc<dyn DialogPresenter>
        ));
        manager.mark_runtime_active("runtime", 0);

        let first = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .show("extension", "runtime", 0, request("same"))
                    .await
            }
        });
        while manager.pending_count("runtime") == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        let duplicate = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .show("extension", "runtime", 0, request("same"))
                    .await
            }
        });
        let duplicate = tokio::time::timeout(std::time::Duration::from_secs(1), duplicate)
            .await
            .expect("duplicate call terminates")
            .expect("duplicate task succeeds");
        let error = duplicate.unwrap_err();
        assert!(error.to_string().contains("already pending"));
        assert_eq!(1, manager.pending_count("runtime"));

        presenter.dismiss_all();
        manager.remove_runtime("runtime");
        assert_eq!(
            UiDialogResult::Dismissed,
            first
                .await
                .expect("dialog task succeeds")
                .expect("cleanup produces a result")
        );
        assert_eq!(0, manager.pending_count("runtime"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generation_retirement_dismisses_only_old_generation() {
        let presenter = Arc::new(HeldPresenter::default());
        let manager = Arc::new(DialogActivationManager::new(
            Arc::clone(&presenter) as Arc<dyn DialogPresenter>
        ));
        manager.mark_runtime_active("runtime", 0);

        let old = tokio::spawn({
            let manager = manager.clone();
            async move {
                manager
                    .show("extension", "runtime", 0, request("old"))
                    .await
            }
        });
        while presenter.requests.lock().is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        manager.retire_generation("runtime", 0);
        manager.mark_runtime_active("runtime", 1);

        let new = tokio::spawn({
            let manager = Arc::clone(&manager);
            async move {
                manager
                    .show("extension", "runtime", 1, request("new"))
                    .await
            }
        });
        while presenter.requests.lock().len() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
        assert_eq!(
            UiDialogResult::Dismissed,
            old.await
                .expect("old dialog task succeeds")
                .expect("old generation is dismissed")
        );
        assert_eq!(1, manager.pending_count("runtime"));

        let completion = presenter
            .completions
            .lock()
            .remove(&presenter.requests.lock()[1].key)
            .expect("new completion");
        let _ = completion.send(DialogUserResult::Terminal(DialogTerminalResult::Confirmed));
        assert_eq!(
            UiDialogResult::Confirmed,
            new.await
                .expect("new dialog task succeeds")
                .expect("new generation remains active")
        );
        assert_eq!(0, manager.pending_count("runtime"));
    }
}
