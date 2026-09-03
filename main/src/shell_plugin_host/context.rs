use gpui_shell::{HostModule, HostObject, HostValue};

#[derive(Clone)]
pub(crate) struct ShellConnectionContext {
    pub connection_id: i64,
    pub name: String,
    pub contribution_id: String,
    pub resource_type: String,
    pub config: serde_json::Map<String, serde_json::Value>,
    pub credential_refs: serde_json::Map<String, serde_json::Value>,
}

pub(super) fn context_module(
    contribution: &extension_runtime::RegisteredShellViewContribution,
    connection: Option<ShellConnectionContext>,
) -> HostModule {
    let extension_id = contribution.extension_id.clone();
    let view_id = contribution.id.clone();
    let backends = contribution.backends.keys().cloned().collect::<Vec<_>>();
    let connection = connection
        .and_then(|connection| {
            super::value::json_to_host(&serde_json::json!({
                "id": connection.connection_id,
                "name": connection.name,
                "contributionId": connection.contribution_id,
                "resourceType": connection.resource_type,
                "config": connection.config,
                "credentialRefs": connection.credential_refs,
            }))
            .ok()
        })
        .unwrap_or(HostValue::Null);
    HostModule::new("navop.context")
        .declarations(
            "export function current(): { extensionId: string; viewId: string; backends: string[]; connection: unknown | null };",
        )
        .function("current", move |_| {
            Ok(HostObject::new()
                .field("extensionId", extension_id.clone())
                .field("viewId", view_id.clone())
                .field("backends", backends.clone())
                .field("connection", connection.clone())
                .into())
        })
}
