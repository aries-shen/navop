use std::collections::{HashMap, HashSet};

use one_core::{
    connection_notifier::ConnectionDataEvent,
    storage::{
        ConnectionRepository, ExtensionConnectionParams, StoredConnection, traits::Repository,
    },
};

pub(super) fn build_connection(
    existing: Option<&StoredConnection>,
    contribution: &extension_runtime::RegisteredResourceConnectionContribution,
    name: String,
    config: serde_json::Map<String, serde_json::Value>,
    updates: HashMap<String, String>,
    declared: &HashSet<String>,
    workspace_id: Option<i64>,
) -> anyhow::Result<StoredConnection> {
    let mut secrets = existing
        .and_then(|connection| connection.to_extension_params().ok())
        .map(|params| params.secrets)
        .unwrap_or_default();
    secrets.retain(|field, _| declared.contains(field));
    secrets.extend(updates);
    let params = ExtensionConnectionParams::new(
        contribution.extension_id.clone(),
        contribution.id.clone(),
        config,
        secrets,
    )?;
    let mut connection = StoredConnection::new_extension(name, params, workspace_id);
    if let Some(existing) = existing {
        copy_metadata(existing, &mut connection);
    }
    Ok(connection)
}

fn copy_metadata(existing: &StoredConnection, connection: &mut StoredConnection) {
    connection.id = existing.id;
    connection.credential_revision = existing.credential_revision;
    connection.remark = existing.remark.clone();
    connection.sync_enabled = existing.sync_enabled;
    connection.cloud_id = existing.cloud_id.clone();
    connection.last_synced_at = existing.last_synced_at;
    connection.sort_order = existing.sort_order;
    connection.created_at = existing.created_at;
    connection.updated_at = existing.updated_at;
    connection.team_id = existing.team_id.clone();
    connection.owner_id = existing.owner_id.clone();
}

pub(super) fn persist_connection(
    repository: &ConnectionRepository,
    connection: &mut StoredConnection,
) -> anyhow::Result<ConnectionDataEvent> {
    if connection.id.is_some() {
        repository.update(connection)?;
        Ok(ConnectionDataEvent::ConnectionUpdated {
            connection: connection.clone(),
        })
    } else {
        repository.insert(connection)?;
        Ok(ConnectionDataEvent::ConnectionCreated {
            connection: connection.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, HashSet};

    use extension_runtime::extension::manifest::ResourceConnectionForm;
    use one_core::storage::ExtensionConnectionParams;

    use super::*;

    #[test]
    fn build_connection_preserves_only_visible_declared_secrets() {
        let params = ExtensionConnectionParams::new(
            "com.example.search",
            "search",
            serde_json::Map::new(),
            BTreeMap::from([
                ("api_key".into(), "old-key".into()),
                ("password".into(), "old-password".into()),
            ]),
        )
        .unwrap();
        let existing = StoredConnection::new_extension("Search".into(), params, None);
        let contribution = contribution();

        let connection = build_connection(
            Some(&existing),
            &contribution,
            "Search".into(),
            serde_json::Map::new(),
            HashMap::from([("password".into(), "new-password".into())]),
            &HashSet::from(["password".into()]),
            Some(7),
        )
        .unwrap();

        let params = connection.to_extension_params().unwrap();
        assert_eq!(
            BTreeMap::from([("password".into(), "new-password".into())]),
            params.secrets
        );
        assert_eq!(Some(7), connection.workspace_id);
    }

    fn contribution() -> extension_runtime::RegisteredResourceConnectionContribution {
        extension_runtime::RegisteredResourceConnectionContribution {
            extension_id: "com.example.search".into(),
            extension_root: "/tmp/com.example.search".into(),
            id: "search".into(),
            label: "Search".into(),
            description: None,
            icon_path: None,
            runtime_id: "com.example.search::main".into(),
            resource_type: "search".into(),
            shell_view_id: None,
            form: ResourceConnectionForm::default(),
        }
    }
}
