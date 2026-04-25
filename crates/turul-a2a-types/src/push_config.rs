//! Ergonomic wrapper for push notification configuration.

use turul_a2a_proto as pb;

/// HTTP authentication info for the push receiver, mirroring the `Authorization`
/// header semantics in [RFC 9110 §11](https://www.rfc-editor.org/rfc/rfc9110#section-11).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PushAuth {
    pub(crate) inner: pb::AuthenticationInfo,
}

impl PushAuth {
    /// Build a new `PushAuth`. `scheme` is case-insensitive (e.g. `Bearer`,
    /// `Basic`); `credentials` carries the scheme-specific token.
    pub fn new(scheme: impl Into<String>, credentials: impl Into<String>) -> Self {
        Self {
            inner: pb::AuthenticationInfo {
                scheme: scheme.into(),
                credentials: credentials.into(),
            },
        }
    }

    pub fn scheme(&self) -> &str {
        &self.inner.scheme
    }

    pub fn credentials(&self) -> &str {
        &self.inner.credentials
    }

    pub fn as_proto(&self) -> &pb::AuthenticationInfo {
        &self.inner
    }

    pub fn into_proto(self) -> pb::AuthenticationInfo {
        self.inner
    }
}

impl From<pb::AuthenticationInfo> for PushAuth {
    fn from(inner: pb::AuthenticationInfo) -> Self {
        Self { inner }
    }
}

/// Ergonomic wrapper over proto `TaskPushNotificationConfig`.
///
/// Construct via [`PushConfigBuilder`]; the server populates `id` during
/// registration and the builder leaves it empty by default.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct PushConfig {
    pub(crate) inner: pb::TaskPushNotificationConfig,
}

impl PushConfig {
    /// Server-assigned config id (empty before registration).
    pub fn id(&self) -> &str {
        &self.inner.id
    }

    /// Task this config is bound to.
    pub fn task_id(&self) -> &str {
        &self.inner.task_id
    }

    /// Webhook URL.
    pub fn url(&self) -> &str {
        &self.inner.url
    }

    /// Shared secret echoed back in the `X-Turul-Push-Token` header.
    pub fn token(&self) -> &str {
        &self.inner.token
    }

    /// Optional HTTP auth header for the webhook delivery.
    pub fn authentication(&self) -> Option<PushAuth> {
        self.inner.authentication.clone().map(PushAuth::from)
    }

    pub fn tenant(&self) -> &str {
        &self.inner.tenant
    }

    pub fn as_proto(&self) -> &pb::TaskPushNotificationConfig {
        &self.inner
    }

    pub fn into_proto(self) -> pb::TaskPushNotificationConfig {
        self.inner
    }
}

impl From<pb::TaskPushNotificationConfig> for PushConfig {
    fn from(inner: pb::TaskPushNotificationConfig) -> Self {
        Self { inner }
    }
}

/// A page of push notification configs returned by `list_push_configs`.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct PushConfigPage {
    pub configs: Vec<PushConfig>,
    /// `None` once the listing is exhausted.
    pub next_page_token: Option<String>,
}

impl PushConfigPage {
    pub fn new(configs: Vec<PushConfig>, next_page_token: Option<String>) -> Self {
        Self {
            configs,
            next_page_token,
        }
    }
}

/// Builder for [`PushConfig`].
///
/// The 80% case is two inputs: `url` + `token`. `authentication` and
/// `tenant` are optional and rarely used.
#[derive(Debug, Clone)]
pub struct PushConfigBuilder {
    url: String,
    token: String,
    task_id: String,
    tenant: String,
    authentication: Option<PushAuth>,
}

impl PushConfigBuilder {
    /// Begin a new builder. `url` is the webhook receiver, `token` is the
    /// shared secret echoed in the `X-Turul-Push-Token` header.
    pub fn new(url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: token.into(),
            task_id: String::new(),
            tenant: String::new(),
            authentication: None,
        }
    }

    /// Bind to an existing task. The client also passes the task id as a
    /// path parameter, so this is only required when constructing a config
    /// without going through `create_push_config`.
    pub fn task_id(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = task_id.into();
        self
    }

    /// Multi-tenant routing.
    pub fn tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = tenant.into();
        self
    }

    /// HTTP `Authorization` header to attach to webhook deliveries.
    pub fn authentication(mut self, auth: PushAuth) -> Self {
        self.authentication = Some(auth);
        self
    }

    pub fn build(self) -> PushConfig {
        PushConfig {
            inner: pb::TaskPushNotificationConfig {
                tenant: self.tenant,
                id: String::new(),
                task_id: self.task_id,
                url: self.url,
                token: self.token,
                authentication: self.authentication.map(PushAuth::into_proto),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_minimal() {
        let cfg = PushConfigBuilder::new("https://example.test/webhook", "secret").build();
        assert_eq!(cfg.url(), "https://example.test/webhook");
        assert_eq!(cfg.token(), "secret");
        assert_eq!(cfg.task_id(), "");
        assert_eq!(cfg.id(), "");
        assert!(cfg.authentication().is_none());
    }

    #[test]
    fn builder_full() {
        let cfg = PushConfigBuilder::new("https://example.test/webhook", "secret")
            .task_id("task-42")
            .tenant("acme")
            .authentication(PushAuth::new("Bearer", "eyJ..."))
            .build();
        assert_eq!(cfg.task_id(), "task-42");
        assert_eq!(cfg.tenant(), "acme");
        let auth = cfg.authentication().unwrap();
        assert_eq!(auth.scheme(), "Bearer");
        assert_eq!(auth.credentials(), "eyJ...");
    }

    #[test]
    fn round_trips_proto() {
        let cfg = PushConfigBuilder::new("https://example.test/webhook", "secret").build();
        let proto = cfg.clone().into_proto();
        let back: PushConfig = proto.into();
        assert_eq!(cfg, back);
    }
}
