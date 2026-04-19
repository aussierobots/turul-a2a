//! Generic JWT validator with JWKS caching and kid-miss refresh.
//!
//! Design sourced from `turul-mcp-framework/crates/turul-mcp-oauth/src/jwt.rs`.
//! Local to turul-a2a; future extraction to a shared crate is a separate ADR.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Errors from JWT validation.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum JwtValidationError {
    #[error("Invalid token: {0}")]
    InvalidToken(String),
    #[error("Token expired")]
    TokenExpired,
    #[error("Invalid audience")]
    InvalidAudience,
    #[error("Invalid issuer")]
    InvalidIssuer,
    #[error("Unsupported algorithm: {0}")]
    UnsupportedAlgorithm(String),
    #[error("JWKS fetch error: {0}")]
    JwksFetchError(String),
    #[error("Key not found: {0}")]
    KeyNotFound(String),
    #[error("Decoding error: {0}")]
    DecodingError(String),
}

/// Validated token claims extracted from a JWT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenClaims {
    #[serde(default)]
    pub sub: String,
    #[serde(default)]
    pub iss: String,
    /// Audience — can be string or array in JWT.
    #[serde(default)]
    pub aud: serde_json::Value,
    #[serde(default)]
    pub exp: u64,
    #[serde(default)]
    pub iat: u64,
    /// Scopes (space-separated string).
    #[serde(default)]
    pub scope: Option<String>,
    /// All other claims.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct JwksResponse {
    keys: Vec<JwkKey>,
}

#[derive(Debug, Clone, Deserialize)]
struct JwkKey {
    kty: String,
    kid: Option<String>,
    alg: Option<String>,
    n: Option<String>,
    e: Option<String>,
    crv: Option<String>,
    x: Option<String>,
    y: Option<String>,
}

struct CachedJwks {
    keys: HashMap<String, (DecodingKey, Algorithm)>,
    last_refresh_at: Instant,
}

/// JWT validator with JWKS caching and kid-miss refresh.
///
/// Supports RS256 and ES256 by default. Rate-limits JWKS fetches.
pub struct JwtValidator {
    jwks_uri: String,
    cached_jwks: RwLock<Option<CachedJwks>>,
    allowed_algorithms: Vec<Algorithm>,
    issuer: Option<String>,
    audience: Option<String>,
    refresh_interval: Duration,
    http_client: reqwest::Client,
}

impl JwtValidator {
    pub fn new(jwks_uri: impl Into<String>, audience: impl Into<String>) -> Self {
        Self {
            jwks_uri: jwks_uri.into(),
            cached_jwks: RwLock::new(None),
            allowed_algorithms: vec![Algorithm::RS256, Algorithm::ES256],
            issuer: None,
            audience: Some(audience.into()),
            refresh_interval: Duration::from_secs(60),
            http_client: reqwest::Client::new(),
        }
    }

    pub fn with_issuer(mut self, issuer: impl Into<String>) -> Self {
        self.issuer = Some(issuer.into());
        self
    }

    pub fn with_algorithms(mut self, algorithms: Vec<Algorithm>) -> Self {
        self.allowed_algorithms = algorithms;
        self
    }

    pub fn with_refresh_interval(mut self, interval: Duration) -> Self {
        self.refresh_interval = interval;
        self
    }

    /// Validate a JWT token and return the claims.
    pub async fn validate(&self, token: &str) -> Result<TokenClaims, JwtValidationError> {
        let header = decode_header(token)
            .map_err(|e| JwtValidationError::DecodingError(format!("Invalid JWT header: {e}")))?;

        if !self.allowed_algorithms.contains(&header.alg) {
            return Err(JwtValidationError::UnsupportedAlgorithm(format!(
                "{:?}",
                header.alg
            )));
        }

        let kid = header.kid.as_deref().unwrap_or("default").to_string();
        let (key, jwks_alg) = self.get_decoding_key(&kid).await?;

        // Cross-check: token algorithm must match JWKS-advertised algorithm
        if header.alg != jwks_alg {
            return Err(JwtValidationError::UnsupportedAlgorithm(format!(
                "Token uses {:?} but JWKS key '{kid}' advertises {:?}",
                header.alg, jwks_alg
            )));
        }

        let mut validation = Validation::new(header.alg);
        validation.validate_exp = true;

        if let Some(ref iss) = self.issuer {
            validation.set_issuer(&[iss]);
        }

        if let Some(ref aud) = self.audience {
            validation.set_audience(&[aud]);
        } else {
            validation.validate_aud = false;
        }

        let token_data =
            decode::<TokenClaims>(token, &key, &validation).map_err(|e| match e.kind() {
                jsonwebtoken::errors::ErrorKind::ExpiredSignature => {
                    JwtValidationError::TokenExpired
                }
                jsonwebtoken::errors::ErrorKind::InvalidAudience => {
                    JwtValidationError::InvalidAudience
                }
                jsonwebtoken::errors::ErrorKind::InvalidIssuer => JwtValidationError::InvalidIssuer,
                _ => JwtValidationError::InvalidToken(e.to_string()),
            })?;

        Ok(token_data.claims)
    }

    async fn get_decoding_key(
        &self,
        kid: &str,
    ) -> Result<(DecodingKey, Algorithm), JwtValidationError> {
        // Try cache first
        {
            let cache = self.cached_jwks.read().await;
            if let Some(ref cached) = *cache {
                if let Some((key, alg)) = cached.keys.get(kid) {
                    return Ok((key.clone(), *alg));
                }
            }
        }

        // Cache miss — refresh JWKS
        self.refresh_jwks().await?;

        // Try again
        let cache = self.cached_jwks.read().await;
        if let Some(ref cached) = *cache {
            if let Some((key, alg)) = cached.keys.get(kid) {
                return Ok((key.clone(), *alg));
            }
        }

        Err(JwtValidationError::KeyNotFound(kid.to_string()))
    }

    async fn refresh_jwks(&self) -> Result<(), JwtValidationError> {
        // Rate limit
        {
            let cache = self.cached_jwks.read().await;
            if let Some(ref cached) = *cache {
                if cached.last_refresh_at.elapsed() < self.refresh_interval {
                    debug!("JWKS refresh rate-limited, skipping");
                    return Ok(());
                }
            }
        }

        debug!("Fetching JWKS from {}", self.jwks_uri);

        let response = self
            .http_client
            .get(&self.jwks_uri)
            .send()
            .await
            .map_err(|e| JwtValidationError::JwksFetchError(e.to_string()))?;

        let jwks: JwksResponse = response
            .json()
            .await
            .map_err(|e| JwtValidationError::JwksFetchError(format!("Invalid JWKS JSON: {e}")))?;

        let mut keys = HashMap::new();

        for key in &jwks.keys {
            let kid = key.kid.clone().unwrap_or_else(|| "default".to_string());

            match key.kty.as_str() {
                "RSA" => {
                    if let (Some(n), Some(e)) = (&key.n, &key.e) {
                        match DecodingKey::from_rsa_components(n, e) {
                            Ok(decoding_key) => {
                                let alg = key
                                    .alg
                                    .as_deref()
                                    .and_then(|a| match a {
                                        "RS256" => Some(Algorithm::RS256),
                                        "RS384" => Some(Algorithm::RS384),
                                        "RS512" => Some(Algorithm::RS512),
                                        _ => None,
                                    })
                                    .unwrap_or(Algorithm::RS256);
                                keys.insert(kid, (decoding_key, alg));
                            }
                            Err(e) => warn!("Failed to parse RSA key: {e}"),
                        }
                    }
                }
                "EC" => {
                    if let (Some(x), Some(y), Some(crv)) = (&key.x, &key.y, &key.crv) {
                        match DecodingKey::from_ec_components(x, y) {
                            Ok(decoding_key) => {
                                let alg = match crv.as_str() {
                                    "P-256" => Algorithm::ES256,
                                    "P-384" => Algorithm::ES384,
                                    _ => {
                                        warn!("Unsupported EC curve: {crv}");
                                        continue;
                                    }
                                };
                                keys.insert(kid, (decoding_key, alg));
                            }
                            Err(e) => warn!("Failed to parse EC key: {e}"),
                        }
                    }
                }
                other => debug!("Skipping unsupported key type: {other}"),
            }
        }

        debug!("JWKS loaded: {} keys", keys.len());

        let now = Instant::now();
        *self.cached_jwks.write().await = Some(CachedJwks {
            keys,
            last_refresh_at: now,
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_claims_deserializes_with_defaults() {
        let json = r#"{"sub":"user-1","iss":"https://auth.example.com","exp":999999999}"#;
        let claims: TokenClaims = serde_json::from_str(json).unwrap();
        assert_eq!(claims.sub, "user-1");
        assert_eq!(claims.iss, "https://auth.example.com");
        assert_eq!(claims.exp, 999999999);
        assert!(claims.scope.is_none());
    }

    #[test]
    fn token_claims_handles_array_audience() {
        let json = r#"{"sub":"u","aud":["a","b"],"exp":1}"#;
        let claims: TokenClaims = serde_json::from_str(json).unwrap();
        assert!(claims.aud.is_array());
    }

    #[test]
    fn token_claims_handles_string_audience() {
        let json = r#"{"sub":"u","aud":"single","exp":1}"#;
        let claims: TokenClaims = serde_json::from_str(json).unwrap();
        assert_eq!(claims.aud, "single");
    }

    #[test]
    fn token_claims_captures_extra_fields() {
        let json = r#"{"sub":"u","exp":1,"custom_field":"custom_value"}"#;
        let claims: TokenClaims = serde_json::from_str(json).unwrap();
        assert_eq!(claims.extra.get("custom_field").unwrap(), "custom_value");
    }

    #[test]
    fn error_types_are_distinct() {
        let errors: Vec<JwtValidationError> = vec![
            JwtValidationError::InvalidToken("bad".into()),
            JwtValidationError::TokenExpired,
            JwtValidationError::InvalidAudience,
            JwtValidationError::InvalidIssuer,
            JwtValidationError::UnsupportedAlgorithm("HS256".into()),
            JwtValidationError::JwksFetchError("network".into()),
            JwtValidationError::KeyNotFound("kid-1".into()),
            JwtValidationError::DecodingError("corrupt".into()),
        ];
        // All should have non-empty display
        for err in &errors {
            assert!(!err.to_string().is_empty());
        }
    }

    #[test]
    fn validator_builder_api() {
        let _validator =
            JwtValidator::new("https://example.com/.well-known/jwks.json", "my-audience")
                .with_issuer("https://example.com")
                .with_algorithms(vec![Algorithm::RS256])
                .with_refresh_interval(Duration::from_secs(120));
    }
}
