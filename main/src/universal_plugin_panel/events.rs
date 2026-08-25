use declarative_ui_demo::StateOperation;
use extension_host::CancellationToken;
use extension_plugin_adapter::{
    EventStreamBatch, EventStreamSubscription, EventStreamSubscriptionConfig,
    ManagedUniversalPluginClient,
};
use extension_protocol::{
    declarative_ui::UiEventSubscriptionOperation, event_stream::EventOpenParams,
};
use gpui::{AsyncApp, Context, Task};
use one_core::gpui_tokio::Tokio;
use serde_json::json;
use tokio::sync::mpsc;

use super::UniversalPluginPanel;

pub(super) struct PanelEventSubscription {
    pub(super) epoch: u64,
    cancel: CancellationToken,
    task: Task<()>,
}

impl PanelEventSubscription {
    pub(super) fn cancel(self) -> Task<()> {
        self.cancel.cancel();
        self.task
    }
}

pub(super) fn subscribe(
    panel: &mut UniversalPluginPanel,
    operation: &UiEventSubscriptionOperation,
    cx: &mut Context<UniversalPluginPanel>,
) -> Result<(), String> {
    let UiEventSubscriptionOperation::Subscribe {
        subscription_id,
        kind,
        conn_id,
        capacity,
        max_events,
        wait_ms,
        state_key,
    } = operation
    else {
        return Ok(());
    };
    panel.cancel_event_subscription(subscription_id);
    let epoch = panel.allocate_event_subscription_epoch()?;
    let client = panel
        .service
        .universal_plugin_client(&panel.runtime_id)
        .map_err(|error| error.to_string())?;
    let generation = client.generation;
    let cancel = CancellationToken::new();
    let task = spawn_bridge(
        BridgeConfig {
            client,
            open: EventOpenParams {
                conn_id: *conn_id,
                kind: kind.clone(),
                capacity: *capacity,
            },
            subscription: EventStreamSubscriptionConfig {
                channel_capacity: capacity.unwrap_or_default() as usize,
                max_events: max_events.unwrap_or_default(),
                wait_ms: wait_ms.unwrap_or_default(),
            },
            subscription_id: subscription_id.clone(),
            state_key: state_key.clone(),
            generation,
            epoch,
            cancel: cancel.clone(),
        },
        cx,
    );
    panel.event_subscriptions.insert(
        subscription_id.clone(),
        PanelEventSubscription {
            epoch,
            cancel,
            task,
        },
    );
    Ok(())
}

struct BridgeConfig {
    client: ManagedUniversalPluginClient,
    open: EventOpenParams,
    subscription: EventStreamSubscriptionConfig,
    subscription_id: String,
    state_key: String,
    generation: u64,
    epoch: u64,
    cancel: CancellationToken,
}

struct WorkerConfig {
    client: ManagedUniversalPluginClient,
    open: EventOpenParams,
    subscription: EventStreamSubscriptionConfig,
    cancel: CancellationToken,
}

fn spawn_bridge(config: BridgeConfig, cx: &Context<UniversalPluginPanel>) -> Task<()> {
    let (sender, mut receiver) = mpsc::channel(1);
    let worker_cancel = config.cancel.clone();
    let worker = Tokio::spawn(cx, async move {
        run_worker(
            WorkerConfig {
                client: config.client,
                open: config.open,
                subscription: config.subscription,
                cancel: worker_cancel,
            },
            sender,
        )
        .await;
    });
    cx.spawn(async move |this, cx: &mut AsyncApp| {
        while let Some(result) = receiver.recv().await {
            let update = this.update(cx, |panel, cx| {
                panel.apply_event_result(
                    &config.subscription_id,
                    config.epoch,
                    &config.state_key,
                    config.generation,
                    result,
                    cx,
                );
            });
            if update.is_err() {
                config.cancel.cancel();
                break;
            }
        }
        let _ = worker.await;
    })
}

async fn run_worker(config: WorkerConfig, sender: mpsc::Sender<Result<EventStreamBatch, String>>) {
    let opened = tokio::select! {
        result = config.client.open_event_stream(&config.open) => result,
        _ = config.cancel.cancelled() => return,
    };
    let result = match opened {
        Ok(result) => result,
        Err(error) => {
            send_result(&sender, Err(error.to_string()), &config.cancel).await;
            return;
        }
    };
    let mut subscription = EventStreamSubscription::spawn(
        config.client,
        result.stream_id,
        normalize_config(config.subscription),
    );
    loop {
        tokio::select! {
            _ = config.cancel.cancelled() => {
                subscription.close().await;
                return;
            }
            item = subscription.recv() => {
                match item {
                    Some(Ok(batch)) => {
                        let closed = batch.closed;
                        if !send_result(&sender, Ok(batch), &config.cancel).await || closed {
                            subscription.close().await;
                            return;
                        }
                    }
                    Some(Err(error)) => {
                        send_result(&sender, Err(error.to_string()), &config.cancel).await;
                        subscription.close().await;
                        return;
                    }
                    None => return,
                }
            }
        }
    }
}

async fn send_result(
    sender: &mpsc::Sender<Result<EventStreamBatch, String>>,
    result: Result<EventStreamBatch, String>,
    cancel: &CancellationToken,
) -> bool {
    tokio::select! {
        sent = sender.send(result) => sent.is_ok(),
        _ = cancel.cancelled() => false,
        _ = sender.closed() => false,
    }
}

fn normalize_config(mut config: EventStreamSubscriptionConfig) -> EventStreamSubscriptionConfig {
    let defaults = EventStreamSubscriptionConfig::default();
    if config.channel_capacity == 0 {
        config.channel_capacity = defaults.channel_capacity;
    }
    if config.max_events == 0 {
        config.max_events = defaults.max_events;
    }
    if config.wait_ms == 0 {
        config.wait_ms = defaults.wait_ms;
    }
    config
}

pub(super) fn event_state(batch: EventStreamBatch) -> String {
    json!({
        "events": batch.events,
        "dropped_count": batch.dropped_count,
        "closed": batch.closed,
    })
    .to_string()
}

pub(super) fn state_operation(state_key: &str, value: String) -> StateOperation {
    StateOperation::Set {
        key: state_key.to_owned(),
        value,
    }
}
