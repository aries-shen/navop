//! Host-side permission enforcement for universal resource providers.
//!
//! Manifest validation defines the syntax of a permission. This module enforces
//! runtime connection requests after parsing provider-controlled configuration.

use extension_protocol::{
    conn::SecretRef,
    error::{ProtocolError, error_codes},
    host::ResolveSecretParams,
    resource::ResourceOpenParams,
};
use serde_json::Value;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretReference {
    pub namespace: String,
    pub key: String,
}

impl SecretReference {
    pub fn parse(value: &SecretRef) -> Result<Self, ProviderPermissionError> {
        let reference = value.secret_ref.as_str();
        let payload = reference
            .strip_prefix("secret://")
            .ok_or_else(|| ProviderPermissionError::InvalidSecretRef)?;
        let (namespace, key) = payload
            .split_once('/')
            .ok_or_else(|| ProviderPermissionError::InvalidSecretRef)?;
        if namespace.is_empty() || key.is_empty() || key.contains('/') {
            return Err(ProviderPermissionError::InvalidSecretRef);
        }
        Ok(Self {
            namespace: namespace.to_owned(),
            key: key.to_owned(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkEndpoint {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl NetworkEndpoint {
    pub fn parse(value: &str) -> Result<Self, ProviderPermissionError> {
        let url = Url::parse(value).map_err(|_| ProviderPermissionError::InvalidUrl)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ProviderPermissionError::UnsupportedScheme);
        }
        let Some(host) = url.host_str().map(str::to_owned) else {
            return Err(ProviderPermissionError::InvalidUrl);
        };
        let port = url
            .port_or_known_default()
            .ok_or_else(|| ProviderPermissionError::InvalidUrl)?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ProviderPermissionError::InvalidUrl);
        }
        Ok(Self {
            scheme: url.scheme().to_owned(),
            host,
            port,
        })
    }
}

#[derive(Debug, Default)]
pub struct ProviderPermissionSet {
    secret_reads: Vec<(String, String)>,
    tcp_endpoints: Vec<(String, Vec<u16>)>,
}

pub struct ResourceOpenAuthorizer {
    permissions: ProviderPermissionSet,
}

impl ResourceOpenAuthorizer {
    pub fn new<I, P>(permissions: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<str>,
    {
        Self {
            permissions: ProviderPermissionSet::new(permissions),
        }
    }

    pub fn authorize(&self, params: &ResourceOpenParams) -> Result<(), ProviderPermissionError> {
        let url = params
            .config
            .get("url")
            .and_then(Value::as_str)
            .ok_or(ProviderPermissionError::InvalidUrl)?;
        self.permissions
            .authorize_endpoint(&NetworkEndpoint::parse(url)?)
    }
}

impl ProviderPermissionSet {
    pub fn new<I, P>(permissions: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<str>,
    {
        let mut set = Self::default();
        for permission in permissions.into_iter() {
            let permission = permission.as_ref();
            if let Some(scope) = permission.strip_prefix("secrets:read:") {
                if let Some((namespace, key)) = scope.split_once('.') {
                    set.secret_reads
                        .push((namespace.to_owned(), key.to_owned()));
                }
            } else if let Some(rest) = permission.strip_prefix("net:tcp:") {
                if let Some((host, range)) = rest.rsplit_once(':') {
                    if let Some(ports) = parse_port_range(range) {
                        set.tcp_endpoints.push((host.to_owned(), ports));
                    }
                }
            }
        }
        set
    }

    pub fn allows_secret(&self, reference: &SecretReference) -> bool {
        self.secret_reads.iter().any(|(namespace, key)| {
            namespace == &reference.namespace && (key == &reference.key || key == "*")
        })
    }

    pub fn allows_endpoint(&self, endpoint: &NetworkEndpoint) -> bool {
        matches!(endpoint.scheme.as_str(), "http" | "https")
            && self.tcp_endpoints.iter().any(|(host, ports)| {
                host.eq_ignore_ascii_case(&endpoint.host) && ports.contains(&endpoint.port)
            })
    }

    pub fn authorize_secret(
        &self,
        params: &ResolveSecretParams,
    ) -> Result<ResolveSecretParams, ProviderPermissionError> {
        let reference = SecretReference::parse(&params.secret_ref)?;
        if self.allows_secret(&reference) {
            Ok(params.clone())
        } else {
            Err(ProviderPermissionError::SecretDenied)
        }
    }

    pub fn authorize_endpoint(
        &self,
        endpoint: &NetworkEndpoint,
    ) -> Result<(), ProviderPermissionError> {
        if self.allows_endpoint(endpoint) {
            Ok(())
        } else {
            Err(ProviderPermissionError::NetworkDenied)
        }
    }
}

fn parse_port_range(value: &str) -> Option<Vec<u16>> {
    if let Ok(port) = value.parse::<u16>() {
        return Some(vec![port]);
    }
    let (start, end) = value.split_once('-')?;
    let start = start.parse::<u16>().ok()?;
    let end = end.parse::<u16>().ok()?;
    if end < start || usize::from(end - start) > 100 {
        return None;
    }
    Some((start..=end).collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ProviderPermissionError {
    #[error("invalid secret reference; expected secret://namespace/key")]
    InvalidSecretRef,
    #[error(
        "network endpoint must be a host and port without credentials, path, query, or fragment"
    )]
    InvalidUrl,
    #[error("only http and https endpoints are supported")]
    UnsupportedScheme,
    #[error("extension is not permitted to read this secret")]
    SecretDenied,
    #[error("extension is not permitted to connect to this network endpoint")]
    NetworkDenied,
}

impl From<ProviderPermissionError> for ProtocolError {
    fn from(error: ProviderPermissionError) -> Self {
        let code = match error {
            ProviderPermissionError::InvalidSecretRef
            | ProviderPermissionError::InvalidUrl
            | ProviderPermissionError::UnsupportedScheme => error_codes::INVALID_PARAMS,
            ProviderPermissionError::SecretDenied | ProviderPermissionError::NetworkDenied => {
                error_codes::PERMISSION_DENIED
            }
        };
        Self::new(code, error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set() -> ProviderPermissionSet {
        ProviderPermissionSet::new([
            "secrets:read:elasticsearch.*",
            "net:tcp:127.0.0.1:9200",
            "net:tcp:example.com:9200-9210",
        ])
    }

    #[test]
    fn parses_and_enforces_secret_references() {
        let reference =
            SecretReference::parse(&SecretRef::new("secret://elasticsearch/api_key")).unwrap();
        assert_eq!("elasticsearch", reference.namespace);
        assert_eq!("api_key", reference.key);
        assert!(set().allows_secret(&reference));
        assert!(!set().allows_secret(
            &SecretReference::parse(&SecretRef::new("secret://other/api_key")).unwrap()
        ));
        assert!(SecretReference::parse(&SecretRef::new("secret://no/key/suffix")).is_err());
    }

    #[test]
    fn network_permissions_match_host_and_port_only() {
        let set = set();
        assert!(
            set.authorize_endpoint(&NetworkEndpoint::parse("http://127.0.0.1:9200").unwrap())
                .is_ok()
        );
        assert!(
            set.authorize_endpoint(&NetworkEndpoint::parse("https://example.com:9205").unwrap())
                .is_ok()
        );
        assert!(
            set.authorize_endpoint(&NetworkEndpoint::parse("http://127.0.0.1:9201").unwrap())
                .is_err()
        );
        assert!(NetworkEndpoint::parse("ftp://example.com:21").is_err());
        assert!(NetworkEndpoint::parse("http://user@example.com:9200").is_err());
    }

    #[test]
    fn resource_open_authorizer_enforces_provider_configuration() {
        let authorizer =
            ResourceOpenAuthorizer::new(["secrets:read:elasticsearch.*", "net:tcp:127.0.0.1:9200"]);
        let params = ResourceOpenParams {
            resource_type: "elasticsearch".into(),
            config: serde_json::json!({ "url": "http://127.0.0.1:9200" }),
            metadata: None,
        };
        authorizer.authorize(&params).unwrap();

        let denied = ResourceOpenParams {
            resource_type: "elasticsearch".into(),
            config: serde_json::json!({ "url": "http://127.0.0.1:9201" }),
            metadata: None,
        };
        assert_eq!(
            ProviderPermissionError::NetworkDenied,
            authorizer.authorize(&denied).unwrap_err()
        );
    }
}
