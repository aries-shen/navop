//! Host-owned lifecycle for provider-requested panel windows.

use std::{collections::HashMap, sync::Arc};

use extension_protocol::declarative_ui::{
    UiWindowOperation, UiWindowRequest, validate_ui_window_request,
};
use parking_lot::Mutex;
use tokio::sync::oneshot;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WindowActivationKey {
    pub extension_id: String,
    pub runtime_id: String,
    pub generation: u64,
    pub panel_key: String,
    pub panel_activation_id: u64,
    pub window_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowActivationRequest {
    pub key: WindowActivationKey,
    pub request: UiWindowRequest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentedWindow {
    pub native_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WindowPresentationError {
    #[error("window presentation failed: {0}")]
    Failed(String),
}

/// Presents host-owned windows. `close` must be idempotent and must release any
/// pending completion sender for the matching request.
#[async_trait::async_trait]
pub trait WindowPresenter: Send + Sync {
    async fn open(
        &self,
        request: WindowActivationRequest,
        complete: oneshot::Sender<Result<PresentedWindow, WindowPresentationError>>,
    );
    fn close(&self, request: &WindowActivationRequest);
    fn set_title(&self, request: &WindowActivationRequest, title: &str);
}

#[derive(Clone)]
pub struct WindowActivationManager {
    presenter: Arc<dyn WindowPresenter>,
    state: Arc<Mutex<State>>,
}

struct State {
    next_attempt: u64,
    windows: HashMap<WindowActivationKey, WindowEntry>,
}

struct WindowEntry {
    attempt: u64,
    request: WindowActivationRequest,
    native: Option<PresentedWindow>,
}

impl WindowActivationManager {
    pub fn new(presenter: Arc<dyn WindowPresenter>) -> Self {
        Self {
            presenter,
            state: Arc::new(Mutex::new(State {
                next_attempt: 1,
                windows: HashMap::new(),
            })),
        }
    }

    pub async fn open(
        &self,
        request: WindowActivationRequest,
    ) -> Result<PresentedWindow, WindowPresentationError> {
        validate_open_request(&request)?;
        let key = request.key.clone();
        let attempt = self.insert_opening(request.clone())?;
        let (sender, receiver) = oneshot::channel();
        self.presenter.open(request.clone(), sender).await;
        let result = receive_completion(receiver).await;
        self.finish_open(&key, attempt, request, result)
    }

    pub fn close(&self, key: &WindowActivationKey) {
        if let Some(entry) = self.remove(key) {
            self.presenter.close(&entry.request);
        }
    }

    pub fn native_closed(&self, key: &WindowActivationKey) {
        let _ = self.remove(key);
    }

    pub fn set_title(&self, key: &WindowActivationKey, title: &str) {
        let request = self
            .state
            .lock()
            .windows
            .get(key)
            .map(|entry| entry.request.clone());
        if let Some(request) = request {
            self.presenter.set_title(&request, title);
        }
    }

    pub fn remove_panel(&self, panel_key: &str, panel_activation_id: u64) {
        self.cleanup(|key| {
            key.panel_key == panel_key && key.panel_activation_id == panel_activation_id
        });
    }

    pub fn retire_generation(&self, runtime_id: &str, generation: u64) {
        self.cleanup(|key| key.runtime_id == runtime_id && key.generation == generation);
    }

    pub fn remove_runtime(&self, runtime_id: &str) {
        self.cleanup(|key| key.runtime_id == runtime_id);
    }

    pub fn remove_extension(&self, extension_id: &str) {
        self.cleanup(|key| key.extension_id == extension_id);
    }

    fn insert_opening(
        &self,
        request: WindowActivationRequest,
    ) -> Result<u64, WindowPresentationError> {
        let mut state = self.state.lock();
        if state.windows.contains_key(&request.key) {
            return Err(failed("window activation already exists"));
        }
        let attempt = state.next_attempt;
        state.next_attempt = state.next_attempt.wrapping_add(1).max(1);
        state.windows.insert(
            request.key.clone(),
            WindowEntry {
                attempt,
                request,
                native: None,
            },
        );
        Ok(attempt)
    }

    fn finish_open(
        &self,
        key: &WindowActivationKey,
        attempt: u64,
        request: WindowActivationRequest,
        result: Result<PresentedWindow, WindowPresentationError>,
    ) -> Result<PresentedWindow, WindowPresentationError> {
        match result {
            Ok(window) if self.attach_native(key, attempt, window.clone()) => Ok(window),
            Ok(_) => {
                self.presenter.close(&request);
                Err(failed("window activation became stale"))
            }
            Err(error) => {
                self.remove_attempt(key, attempt);
                Err(error)
            }
        }
    }

    fn attach_native(
        &self,
        key: &WindowActivationKey,
        attempt: u64,
        window: PresentedWindow,
    ) -> bool {
        let mut state = self.state.lock();
        let Some(entry) = state.windows.get_mut(key) else {
            return false;
        };
        if entry.attempt != attempt {
            return false;
        }
        entry.native = Some(window);
        true
    }

    fn cleanup(&self, predicate: impl Fn(&WindowActivationKey) -> bool) {
        let entries = {
            let mut state = self.state.lock();
            let keys = state
                .windows
                .keys()
                .filter(|key| predicate(key))
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| state.windows.remove(&key))
                .collect::<Vec<_>>()
        };
        for entry in entries {
            self.presenter.close(&entry.request);
        }
    }

    fn remove_attempt(&self, key: &WindowActivationKey, attempt: u64) {
        let mut state = self.state.lock();
        if state
            .windows
            .get(key)
            .is_some_and(|entry| entry.attempt == attempt)
        {
            state.windows.remove(key);
        }
    }

    fn remove(&self, key: &WindowActivationKey) -> Option<WindowEntry> {
        self.state.lock().windows.remove(key)
    }
}

fn validate_open_request(request: &WindowActivationRequest) -> Result<(), WindowPresentationError> {
    validate_ui_window_request(&request.request).map_err(|error| failed(error.to_string()))?;
    if !matches!(request.request.operation, UiWindowOperation::Open { .. }) {
        return Err(failed("window activation open requires an open operation"));
    }
    if request.key.window_id != request.request.window_id {
        return Err(failed(
            "window activation key does not match request window id",
        ));
    }
    Ok(())
}

async fn receive_completion(
    receiver: oneshot::Receiver<Result<PresentedWindow, WindowPresentationError>>,
) -> Result<PresentedWindow, WindowPresentationError> {
    receiver
        .await
        .unwrap_or_else(|_| Err(failed("window presenter dropped completion")))
}

fn failed(message: impl Into<String>) -> WindowPresentationError {
    WindowPresentationError::Failed(message.into())
}
