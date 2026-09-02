use gpui_shell::{HostModule, HostObject};

pub(super) fn context_module(
    contribution: &extension_runtime::RegisteredShellViewContribution,
) -> HostModule {
    let extension_id = contribution.extension_id.clone();
    let view_id = contribution.id.clone();
    let backends = contribution.backends.keys().cloned().collect::<Vec<_>>();
    HostModule::new("navop.context")
        .declarations(
            "export function current(): { extensionId: string; viewId: string; backends: string[] };",
        )
        .function("current", move |_| {
            Ok(HostObject::new()
                .field("extensionId", extension_id.clone())
                .field("viewId", view_id.clone())
                .field("backends", backends.clone())
                .into())
        })
}
