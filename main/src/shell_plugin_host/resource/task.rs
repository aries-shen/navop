use extension_host::CancellationToken;
use extension_protocol::resource::ResourceCloseParams;
use gpui_shell::{HostAsyncTask, HostError, HostResult};
use std::time::Duration;

use super::ShellResourceSession;

const DROP_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) fn spawn_provider_task<F>(
    tokio: &tokio::runtime::Handle,
    future: F,
    cancel: CancellationToken,
) -> HostAsyncTask
where
    F: std::future::Future<Output = HostResult> + Send + 'static,
{
    let task = tokio.spawn(future);
    HostAsyncTask::new(
        async move {
            task.await
                .map_err(|error| HostError::new(format!("provider task failed: {error}")))?
        },
        move || cancel.cancel(),
    )
}

pub(super) fn host_error(error: extension_host::HostError) -> HostError {
    HostError::new(error.to_string())
}

impl Drop for ShellResourceSession {
    fn drop(&mut self) {
        let Ok(resources) = self.resources.get_mut() else {
            return;
        };
        let records = resources
            .drain()
            .map(|(_, resource)| resource)
            .collect::<Vec<_>>();
        let resources = records
            .into_iter()
            .filter_map(|(alias, generation, resource_id)| {
                self.client(&alias)
                    .ok()
                    .filter(|client| client.generation == generation)
                    .map(|client| (client, resource_id))
            })
            .collect::<Vec<_>>();
        self.tokio.spawn(async move {
            futures::future::join_all(resources.into_iter().map(
                |(client, resource_id)| async move {
                    let _ = tokio::time::timeout(
                        DROP_CLOSE_TIMEOUT,
                        client
                            .client()
                            .close_resource(&ResourceCloseParams { resource_id }),
                    )
                    .await;
                },
            ))
            .await;
        });
    }
}
