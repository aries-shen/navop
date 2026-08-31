use extension_plugin_adapter::{ActivationError, ActivationHandle};

use crate::universal_plugins::UniversalPluginService;

/// Application-owned bridge for the future gpui-shell integration.
///
/// Script loading and HostModule registration will live here. The MVP only
/// exposes runtime leases so gpui-shell can be added without reviving a UI wire
/// protocol in the provider process.
#[derive(Clone)]
pub(crate) struct ShellPluginHost {
    service: UniversalPluginService,
}

impl gpui::Global for ShellPluginHost {}

impl ShellPluginHost {
    pub(crate) fn new(service: UniversalPluginService) -> Self {
        Self { service }
    }

    #[allow(dead_code)]
    pub(crate) async fn activate_runtime(
        &self,
        runtime_id: &str,
    ) -> Result<ActivationHandle, ActivationError> {
        self.service.activate_runtime(runtime_id).await
    }

    #[allow(dead_code)]
    pub(crate) async fn deactivate_activation(
        &self,
        handle: &ActivationHandle,
    ) -> Result<(), ActivationError> {
        self.service.deactivate_activation(handle).await
    }
}
