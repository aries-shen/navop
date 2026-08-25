use std::{collections::HashMap, sync::Arc};

use extension_protocol::declarative_ui::{UiWindowOperation, UiWindowRequest};
use parking_lot::Mutex;
use tokio::sync::oneshot;

use crate::{
    PresentedWindow, WindowActivationKey, WindowActivationManager, WindowActivationRequest,
    WindowPresentationError, WindowPresenter,
};

type Completion = oneshot::Sender<Result<PresentedWindow, WindowPresentationError>>;

#[derive(Default)]
struct TestPresenter {
    pending: Mutex<HashMap<String, Completion>>,
    opened: Mutex<Vec<String>>,
    closed: Mutex<Vec<String>>,
    titles: Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait]
impl WindowPresenter for TestPresenter {
    async fn open(&self, request: WindowActivationRequest, complete: Completion) {
        self.opened.lock().push(request.request.request_id.clone());
        self.pending
            .lock()
            .insert(request.request.request_id.clone(), complete);
    }

    fn close(&self, request: &WindowActivationRequest) {
        let request_id = request.request.request_id.clone();
        self.closed.lock().push(request_id.clone());
        self.pending.lock().remove(&request_id);
    }

    fn set_title(&self, request: &WindowActivationRequest, title: &str) {
        self.titles
            .lock()
            .push((request.request.request_id.clone(), title.into()));
    }
}

impl TestPresenter {
    fn complete(&self, request_id: &str, native_id: &str) {
        let sender = self.pending.lock().remove(request_id).unwrap();
        sender
            .send(Ok(PresentedWindow {
                native_id: native_id.into(),
            }))
            .unwrap();
    }
}

fn key(generation: u64, activation: u64) -> WindowActivationKey {
    WindowActivationKey {
        extension_id: "ext".into(),
        runtime_id: "ext::main".into(),
        generation,
        panel_key: "ext::panel".into(),
        panel_activation_id: activation,
        window_id: "details".into(),
    }
}

fn request(request_id: &str, key: WindowActivationKey) -> WindowActivationRequest {
    WindowActivationRequest {
        request: UiWindowRequest {
            request_id: request_id.into(),
            window_id: key.window_id.clone(),
            operation: UiWindowOperation::Open {
                title: "Details".into(),
                width: 640,
                height: 480,
                panel_id: "panel".into(),
                modal: false,
            },
        },
        key,
    }
}

#[tokio::test]
async fn open_registers_presented_window_and_rejects_duplicate() {
    let presenter = Arc::new(TestPresenter::default());
    let manager = WindowActivationManager::new(presenter.clone());
    let activation = request("open-1", key(1, 1));
    let task = tokio::spawn({
        let manager = manager.clone();
        let activation = activation.clone();
        async move { manager.open(activation).await }
    });
    tokio::task::yield_now().await;
    let duplicate = manager.open(request("open-2", key(1, 1))).await;
    assert!(duplicate.is_err());
    presenter.complete("open-1", "native-1");
    assert_eq!(task.await.unwrap().unwrap().native_id, "native-1");
    assert_eq!(&*presenter.opened.lock(), &["open-1"]);
}

#[tokio::test]
async fn open_rejects_non_open_operation_and_mismatched_window_id() {
    let presenter = Arc::new(TestPresenter::default());
    let manager = WindowActivationManager::new(presenter.clone());
    let mut close = request("close", key(1, 1));
    close.request.operation = UiWindowOperation::Close;
    assert!(manager.open(close).await.is_err());
    let mut mismatch = request("mismatch", key(1, 1));
    mismatch.request.window_id = "other".into();
    assert!(manager.open(mismatch).await.is_err());
    assert!(presenter.opened.lock().is_empty());
}

#[tokio::test]
async fn exact_panel_cleanup_does_not_close_replacement_lease() {
    let presenter = Arc::new(TestPresenter::default());
    let manager = WindowActivationManager::new(presenter.clone());
    let first = tokio::spawn({
        let manager = manager.clone();
        async move { manager.open(request("old", key(1, 1))).await }
    });
    tokio::task::yield_now().await;
    manager.remove_panel("ext::panel", 1);
    assert!(first.await.unwrap().is_err());

    let replacement = tokio::spawn({
        let manager = manager.clone();
        async move { manager.open(request("new", key(1, 2))).await }
    });
    tokio::task::yield_now().await;
    manager.remove_panel("ext::panel", 1);
    presenter.complete("new", "native-new");
    assert!(replacement.await.unwrap().is_ok());
    assert_eq!(&*presenter.closed.lock(), &["old"]);
}

#[tokio::test]
async fn retired_generation_does_not_affect_current_generation() {
    let presenter = Arc::new(TestPresenter::default());
    let manager = WindowActivationManager::new(presenter.clone());
    let old = tokio::spawn({
        let manager = manager.clone();
        async move { manager.open(request("old", key(1, 1))).await }
    });
    let current = tokio::spawn({
        let manager = manager.clone();
        async move { manager.open(request("current", key(2, 1))).await }
    });
    tokio::task::yield_now().await;
    manager.retire_generation("ext::main", 1);
    presenter.complete("current", "native-current");
    assert!(old.await.unwrap().is_err());
    assert!(current.await.unwrap().is_ok());
}

#[tokio::test]
async fn native_close_forgets_ownership_without_presenter_close() {
    let presenter = Arc::new(TestPresenter::default());
    let manager = WindowActivationManager::new(presenter.clone());
    let activation_key = key(1, 1);
    let task = tokio::spawn({
        let manager = manager.clone();
        let activation_key = activation_key.clone();
        async move { manager.open(request("open", activation_key)).await }
    });
    tokio::task::yield_now().await;
    presenter.complete("open", "native");
    task.await.unwrap().unwrap();
    manager.native_closed(&activation_key);
    manager.close(&activation_key);
    assert!(presenter.closed.lock().is_empty());
}

#[tokio::test]
async fn set_title_targets_only_owned_window() {
    let presenter = Arc::new(TestPresenter::default());
    let manager = WindowActivationManager::new(presenter.clone());
    let activation_key = key(1, 1);
    let task = tokio::spawn({
        let manager = manager.clone();
        let activation_key = activation_key.clone();
        async move { manager.open(request("open", activation_key)).await }
    });
    tokio::task::yield_now().await;
    presenter.complete("open", "native");
    task.await.unwrap().unwrap();
    manager.set_title(&activation_key, "Renamed");
    manager.set_title(&key(2, 1), "Stale");
    assert_eq!(
        &*presenter.titles.lock(),
        &[("open".into(), "Renamed".into())]
    );
}
