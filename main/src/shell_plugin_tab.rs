use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use extension_host::CancellationToken;
use extension_plugin_adapter::ActivationHandle;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, SharedString, Styled, Task, Window, div,
};
use one_core::tab_container::{TabContent, TabContentEvent};

use crate::shell_plugin_host::{PreparedShellView, ShellPluginHost};

struct PreparationCompletion {
    cancel: CancellationToken,
    done: AtomicBool,
    late_activations: Mutex<Vec<ActivationHandle>>,
    release_tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    notify: tokio::sync::Notify,
}

impl PreparationCompletion {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            cancel: CancellationToken::new(),
            done: AtomicBool::new(false),
            late_activations: Mutex::new(Vec::new()),
            release_tasks: Mutex::new(Vec::new()),
            notify: tokio::sync::Notify::new(),
        })
    }

    fn finish(&self) {
        self.done.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.done.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }

    fn push_late(&self, activations: Vec<ActivationHandle>) {
        self.late_activations
            .lock()
            .expect("shell activation completion poisoned")
            .extend(activations);
    }

    fn take_late(&self) -> Vec<ActivationHandle> {
        std::mem::take(
            &mut *self
                .late_activations
                .lock()
                .expect("shell activation completion poisoned"),
        )
    }

    fn push_release_task(&self, task: tokio::task::JoinHandle<()>) {
        self.release_tasks
            .lock()
            .expect("shell activation completion poisoned")
            .push(task);
    }

    fn take_release_tasks(&self) -> Vec<tokio::task::JoinHandle<()>> {
        std::mem::take(
            &mut *self
                .release_tasks
                .lock()
                .expect("shell activation completion poisoned"),
        )
    }
}

enum ShellPluginTabState {
    Loading,
    Ready(gpui_shell::LoadedScriptView),
    Failed(String),
}

pub(crate) struct ShellPluginTab {
    title: SharedString,
    focus_handle: FocusHandle,
    host: ShellPluginHost,
    state: ShellPluginTabState,
    activations: Vec<ActivationHandle>,
    preparation: Arc<PreparationCompletion>,
    closing: bool,
    runtime_ids: Vec<String>,
}

impl ShellPluginTab {
    pub(crate) fn load(
        host: ShellPluginHost,
        contribution: extension_runtime::RegisteredShellViewContribution,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let title = SharedString::from(contribution.title.clone());
        let runtime_ids = contribution.backends.values().cloned().collect();
        let preparation = PreparationCompletion::new();
        let view = cx.new(|cx| Self {
            title,
            focus_handle: cx.focus_handle(),
            host: host.clone(),
            state: ShellPluginTabState::Loading,
            activations: Vec::new(),
            preparation: Arc::clone(&preparation),
            closing: false,
            runtime_ids,
        });
        let activation_task = host.start_prepare(contribution, preparation.cancel.clone());
        let cleanup_host = host.clone();
        let task_completion = Arc::clone(&preparation);
        view.update(cx, |_, cx| {
            cx.spawn_in(window, async move |this, cx| {
                let result = activation_task
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("extension activation cancelled")));
                match result {
                    Ok(prepared) => {
                        let activations = prepared.activations.clone();
                        if this
                            .update_in(cx, |this, window, cx| {
                                if this.closing {
                                    this.preparation.push_late(prepared.activations);
                                } else {
                                    this.finish_load(prepared, window, cx);
                                }
                            })
                            .is_err()
                        {
                            cleanup_host.release(activations);
                        }
                    }
                    Err(error) => {
                        let _ = this.update_in(cx, |this, _, cx| {
                            if !this.closing {
                                this.state = ShellPluginTabState::Failed(error.to_string());
                                cx.notify();
                            }
                        });
                    }
                }
                task_completion.finish();
            })
            .detach();
        });
        view
    }

    fn finish_load(
        &mut self,
        prepared: PreparedShellView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let activations = prepared.activations.clone();
        match self.host.load(prepared, window, cx) {
            Ok(loaded) => {
                self.activations = activations;
                self.state = ShellPluginTabState::Ready(loaded);
            }
            Err(error) => {
                if let Some(task) = self.host.release_task(activations) {
                    self.preparation.push_release_task(task);
                }
                self.state = ShellPluginTabState::Failed(error.to_string());
            }
        }
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) -> Vec<ActivationHandle> {
        self.closing = true;
        self.preparation.cancel.cancel();
        if let ShellPluginTabState::Ready(loaded) = &mut self.state {
            loaded.unload(cx);
        }
        std::mem::take(&mut self.activations)
    }

    pub(crate) fn close_for_extension(&mut self, cx: &mut Context<Self>) -> Task<bool> {
        self.state = ShellPluginTabState::Failed("Extension closed".to_string());
        cx.notify();
        self.close_task(true, cx)
    }

    pub(crate) fn runtime_changed(&mut self, runtime_id: &str, cx: &mut Context<Self>) {
        if !self
            .runtime_ids
            .iter()
            .any(|candidate| candidate == runtime_id)
        {
            return;
        }
        if let ShellPluginTabState::Ready(loaded) = &self.state {
            loaded.view().update(cx, |view, cx| view.refresh(cx));
        }
    }

    fn close_task(&mut self, request_removal: bool, cx: &mut Context<Self>) -> Task<bool> {
        let activations = self.close(cx);
        let preparation = Arc::clone(&self.preparation);
        let service = self.host.service();
        let release = one_core::gpui_tokio::Tokio::spawn_result(cx, async move {
            preparation.wait().await;
            let mut activations = activations;
            activations.extend(preparation.take_late());
            for activation in activations {
                let _ = service.deactivate_activation(&activation).await;
            }
            for task in preparation.take_release_tasks() {
                let _ = task.await;
            }
            Ok(())
        });
        cx.spawn(async move |this, cx| {
            let closed = release.await.is_ok();
            if closed && request_removal {
                let _ = this.update(cx, |_, cx| cx.emit(TabContentEvent::CloseRequested));
            }
            closed
        })
    }
}

impl Drop for ShellPluginTab {
    fn drop(&mut self) {
        self.preparation.cancel.cancel();
        let mut activations = std::mem::take(&mut self.activations);
        activations.extend(self.preparation.take_late());
        self.host.release(activations);
    }
}

impl EventEmitter<TabContentEvent> for ShellPluginTab {}

impl Focusable for ShellPluginTab {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ShellPluginTab {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(match &self.state {
                ShellPluginTabState::Loading => div().p_4().child("Loading extension..."),
                ShellPluginTabState::Failed(error) => {
                    div().p_4().child(format!("Extension failed: {error}"))
                }
                ShellPluginTabState::Ready(loaded) => {
                    div().size_full().child(loaded.view().clone())
                }
            })
    }
}

impl TabContent for ShellPluginTab {
    fn content_key(&self) -> &'static str {
        "ShellPlugin"
    }

    fn title(&self, _cx: &App) -> SharedString {
        self.title.clone()
    }

    fn can_rename(&self, _cx: &App) -> bool {
        false
    }

    fn try_close(
        &mut self,
        _tab_id: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<bool> {
        if self.closing {
            return Task::ready(true);
        }
        self.close_task(false, cx)
    }
}
