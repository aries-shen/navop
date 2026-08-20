//! Host-authoritative APIs used by native universal resource providers.

use std::{collections::BTreeMap, sync::Arc};

use extension_host::{HostApiProvider, HostError, HostResult};
use extension_protocol::host::{self, ResolveSecretParams, ResolveSecretResult};

use crate::provider_permissions::{
    ProviderPermissionError, ProviderPermissionSet, SecretReference,
};

/// Application-owned secret lookup. Values never enter logs or provider UI state.
#[async_trait::async_trait]
pub trait SecretResolver: Send + Sync {
    async fn resolve(&self, namespace: &str, key: &str) -> HostResult<Vec<u8>>;
}

/// Simple in-memory resolver suitable for tests and integration harnesses.
#[derive(Clone, Default)]
pub struct MapSecretResolver(BTreeMap<String, Vec<u8>>);

impl MapSecretResolver {
    pub fn insert(mut self, namespace: &str, key: &str, value: impl Into<Vec<u8>>) -> Self {
        self.0.insert(reference_key(namespace, key), value.into());
        self
    }
}

#[async_trait::async_trait]
impl SecretResolver for MapSecretResolver {
    async fn resolve(&self, namespace: &str, key: &str) -> HostResult<Vec<u8>> {
        self.0
            .get(&reference_key(namespace, key))
            .cloned()
            .ok_or_else(|| HostError::protocol(secret_not_found()))
    }
}

pub struct UniversalProviderHost {
    permissions: ProviderPermissionSet,
    secrets: Arc<dyn SecretResolver>,
}

impl UniversalProviderHost {
    pub fn new<I, P>(permissions: I, secrets: Arc<dyn SecretResolver>) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<str>,
    {
        Self {
            permissions: ProviderPermissionSet::new(
                permissions
                    .into_iter()
                    .map(|value| value.as_ref().to_owned()),
            ),
            secrets,
        }
    }

    async fn resolve_secret(&self, params: ResolveSecretParams) -> HostResult<ResolveSecretResult> {
        let reference = SecretReference::parse(&params.secret_ref)
            .map_err(|error| HostError::protocol(error.into()))?;
        if !self.permissions.allows_secret(&reference) {
            return Err(HostError::protocol(
                ProviderPermissionError::SecretDenied.into(),
            ));
        }
        let value = self
            .secrets
            .resolve(&reference.namespace, &reference.key)
            .await?;
        Ok(ResolveSecretResult { value })
    }
}

#[async_trait::async_trait]
impl HostApiProvider for UniversalProviderHost {
    async fn request_credential(
        &self,
        _params: host::RequestCredentialParams,
    ) -> HostResult<host::RequestCredentialResult> {
        // Providers should receive explicit secret references from connection
        // configuration; interactive credential acquisition remains host-owned.
        Err(HostError::NotImplemented(
            "interactive credential requests are not available for this extension".into(),
        ))
    }

    async fn resolve_secret(&self, params: ResolveSecretParams) -> HostResult<ResolveSecretResult> {
        self.resolve_secret(params).await
    }

    async fn notify(&self, _params: host::NotifyParams) -> HostResult<host::NotifyResult> {
        Ok(host::NotifyResult { clicked: None })
    }

    async fn quick_pick(
        &self,
        _params: host::QuickPickParams,
    ) -> HostResult<host::QuickPickResult> {
        Ok(host::QuickPickResult {
            selected: Vec::new(),
            cancelled: true,
        })
    }

    async fn open_view(&self, _params: host::OpenViewParams) -> HostResult<()> {
        Ok(())
    }

    async fn storage_get(
        &self,
        _params: host::StorageGetParams,
    ) -> HostResult<host::StorageGetResult> {
        Ok(host::StorageGetResult { value: None })
    }

    async fn storage_set(&self, _params: host::StorageSetParams) -> HostResult<()> {
        Ok(())
    }

    async fn log(&self, _params: host::LogParams) -> HostResult<()> {
        Ok(())
    }
}

fn reference_key(namespace: &str, key: &str) -> String {
    format!("{namespace}\u{1}{key}")
}

fn secret_not_found() -> extension_protocol::ProtocolError {
    extension_protocol::ProtocolError::new(
        extension_protocol::error_codes::SECRET_NOT_FOUND,
        "requested secret was not found",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use extension_protocol::conn::SecretRef;

    fn provider() -> UniversalProviderHost {
        UniversalProviderHost::new(
            ["secrets:read:elasticsearch.*".to_owned()],
            Arc::new(MapSecretResolver::default().insert(
                "elasticsearch",
                "api_key",
                b"token-value",
            )),
        )
    }

    #[tokio::test]
    async fn secret_resolution_is_permission_checked() {
        let result = provider()
            .resolve_secret(ResolveSecretParams {
                secret_ref: SecretRef::new("secret://elasticsearch/api_key"),
            })
            .await
            .unwrap();
        assert_eq!(b"token-value".to_vec(), result.value);

        let error = provider()
            .resolve_secret(ResolveSecretParams {
                secret_ref: SecretRef::new("secret://other/api_key"),
            })
            .await
            .unwrap_err();
        assert!(
            matches!(error, HostError::Protocol(error) if error.code == extension_protocol::error_codes::PERMISSION_DENIED)
        );
    }
}
