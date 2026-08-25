use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Duration, Utc};
use mcpmate_secrets::{SecretError, SecretResolver, SecretStoreDeleteError, parse_placeholder};
use reqwest::Url;
use reqwest::header::WWW_AUTHENTICATE;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::{collections::HashMap, error::Error, fmt, sync::Arc};
use tokio::sync::Mutex;

use crate::config::{
    models::{ServerOAuthConfig, ServerOAuthToken, ValidatedTransport},
    server,
};
use crate::core::secrets::store::{
    LocalSecretStore, SecretCreateInput, SecretKindInput, SecretOriginInput, SecretUpdateInput,
    SecretUsageLocationInput, SecretUsageUpsertInput,
};

use super::types::{
    OAuthConfigInput, OAuthConnectionState, OAuthCustodyState, OAuthInitiateResult, OAuthPrepareInput, OAuthStatus,
    OAuthStatusIssue,
};

#[derive(Debug, Clone)]
struct PendingOAuthFlow {
    server_id: String,
    code_verifier: String,
    created_at: chrono::DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    authorization_servers: Option<Vec<String>>,
    scopes_supported: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct AuthorizationServerMetadata {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scopes_supported: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct DynamicClientRegistrationResponse {
    client_id: String,
    client_secret: Option<String>,
    scope: Option<String>,
}

/// Existing token fields carried forward during a token refresh.
struct ExistingTokenContext {
    id: Option<String>,
    refresh_token: Option<String>,
    token_type: String,
    scope: Option<String>,
}

/// Server metadata needed when storing OAuth-owned secrets.
struct OAuthServerContext {
    id: String,
    name: String,
    kind: String,
}

impl OAuthServerContext {
    fn from_server(
        server_id: &str,
        server_name: &str,
        transport: &ValidatedTransport,
    ) -> Self {
        Self {
            id: server_id.to_string(),
            name: server_name.to_string(),
            kind: transport.server_type().to_string(),
        }
    }
}

const OAUTH_ISSUE_SECURE_STORE_UNAVAILABLE: &str = "secure_store_unavailable";
const OAUTH_ISSUE_SECRET_UNREADABLE: &str = "oauth_secret_unreadable";
const OAUTH_SECURE_STORE_UNAVAILABLE_MESSAGE: &str =
    "Secure Store is unavailable; unlock or initialize it before connecting OAuth.";
const OAUTH_SECRET_UNREADABLE_MESSAGE: &str =
    "OAuth credentials cannot be read from Secure Store; reconnect OAuth to replace them.";

fn secret_error_code(error: &SecretError) -> &'static str {
    match error {
        SecretError::InvalidReference(_) => "invalid_reference",
        SecretError::NotFound(_) => "not_found",
        SecretError::InvalidMetadata(_) => "invalid_metadata",
        SecretError::DecryptionFailed(_) => "decryption_failed",
        SecretError::ProviderUnavailable => "provider_unavailable",
        SecretError::UnterminatedPlaceholder => "unterminated_placeholder",
    }
}

fn oauth_secret_issue_from_error(error: &anyhow::Error) -> Option<OAuthStatusIssue> {
    let secret_error = error.downcast_ref::<SecretError>()?;
    match secret_error {
        SecretError::ProviderUnavailable => Some(OAuthStatusIssue {
            code: OAUTH_ISSUE_SECURE_STORE_UNAVAILABLE.to_string(),
            message: OAUTH_SECURE_STORE_UNAVAILABLE_MESSAGE.to_string(),
        }),
        SecretError::InvalidReference(_)
        | SecretError::NotFound(_)
        | SecretError::InvalidMetadata(_)
        | SecretError::DecryptionFailed(_)
        | SecretError::UnterminatedPlaceholder => Some(OAuthStatusIssue {
            code: OAUTH_ISSUE_SECRET_UNREADABLE.to_string(),
            message: OAUTH_SECRET_UNREADABLE_MESSAGE.to_string(),
        }),
    }
}

const OAUTH_TOKEN_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Clone, Copy)]
pub(crate) enum OAuthSecretSlot {
    ClientSecret,
    AccessToken,
    RefreshToken,
}

impl OAuthSecretSlot {
    fn key(self) -> &'static str {
        match self {
            Self::ClientSecret => "client-secret",
            Self::AccessToken => "access-token",
            Self::RefreshToken => "refresh-token",
        }
    }

    fn kind(self) -> SecretKindInput {
        match self {
            Self::ClientSecret => SecretKindInput::OAuthClientSecret,
            Self::AccessToken => SecretKindInput::OAuthAccessToken,
            Self::RefreshToken => SecretKindInput::OAuthRefreshToken,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ClientSecret => "OAuth client secret",
            Self::AccessToken => "OAuth access token",
            Self::RefreshToken => "OAuth refresh token",
        }
    }
}

#[derive(Clone)]
pub struct OAuthManager {
    pool: SqlitePool,
    pending_flows: Arc<Mutex<HashMap<String, PendingOAuthFlow>>>,
    http_client: reqwest::Client,
    secret_store: Option<Arc<LocalSecretStore>>,
    secret_resolver: Option<Arc<dyn SecretResolver>>,
}

#[derive(Debug)]
pub struct OAuthSecretCleanupUnavailable {
    operation: &'static str,
}

impl OAuthSecretCleanupUnavailable {
    fn new(operation: &'static str) -> Self {
        Self { operation }
    }
}

impl fmt::Display for OAuthSecretCleanupUnavailable {
    fn fmt(
        &self,
        formatter: &mut fmt::Formatter<'_>,
    ) -> fmt::Result {
        write!(
            formatter,
            "Secure Store is unavailable; unlock or initialize it before {} OAuth credentials",
            self.operation
        )
    }
}

impl Error for OAuthSecretCleanupUnavailable {}

impl OAuthManager {
    pub fn new(pool: SqlitePool) -> Self {
        Self::new_optional_store(pool, None)
    }

    pub fn new_optional_store(
        pool: SqlitePool,
        secret_store: Option<Arc<LocalSecretStore>>,
    ) -> Self {
        let secret_resolver = secret_store
            .as_ref()
            .map(|store| store.clone() as Arc<dyn SecretResolver>);
        Self {
            pool,
            pending_flows: Arc::new(Mutex::new(HashMap::new())),
            http_client: reqwest::Client::new(),
            secret_store,
            secret_resolver,
        }
    }

    fn secret_store(&self) -> Result<&LocalSecretStore> {
        self.secret_store
            .as_deref()
            .ok_or_else(|| anyhow!("Secure Store is unavailable for OAuth credential custody"))
    }

    fn secret_resolver(&self) -> Result<&dyn SecretResolver> {
        self.secret_resolver
            .as_deref()
            .ok_or_else(|| anyhow!("Secure Store is unavailable for OAuth credential resolution"))
    }

    async fn load_oauth_transport(
        &self,
        server_id: &str,
    ) -> Result<ValidatedTransport> {
        let transport = server::load_validated_server_transport(&self.pool, server_id).await?;
        if !matches!(
            &transport,
            ValidatedTransport::Sse { .. } | ValidatedTransport::StreamableHttp { .. }
        ) {
            bail!("OAuth is only supported for HTTP-based MCP servers (sse or streamable_http)");
        }
        Ok(transport)
    }

    async fn oauth_server_context(
        &self,
        server_id: &str,
        transport: &ValidatedTransport,
    ) -> Result<OAuthServerContext> {
        let server_model = server::get_server_by_id(&self.pool, server_id)
            .await?
            .ok_or_else(|| anyhow!("Server '{}' not found", server_id))?;
        Ok(OAuthServerContext::from_server(
            server_id,
            &server_model.name,
            transport,
        ))
    }

    async fn store_or_preserve_client_secret(
        &self,
        server: &OAuthServerContext,
        value: &str,
    ) -> Result<String> {
        let trimmed = value.trim();
        if let Some(reference) = parse_placeholder(trimmed)? {
            let expected_alias = oauth_secret_alias(&server.id, OAuthSecretSlot::ClientSecret);
            if reference.alias() != expected_alias {
                bail!("OAuth client secret placeholder must reference the server OAuth client secret alias");
            }
            return Ok(trimmed.to_string());
        }
        self.store_oauth_secret(server, OAuthSecretSlot::ClientSecret, trimmed.to_string())
            .await
    }

    async fn store_oauth_secret(
        &self,
        server: &OAuthServerContext,
        slot: OAuthSecretSlot,
        value: String,
    ) -> Result<String> {
        let store = self.secret_store()?;
        let alias = oauth_secret_alias(&server.id, slot);
        let origin = Some(SecretOriginInput {
            server_id: Some(server.id.clone()),
            server_name: Some(server.name.clone()),
            server_kind: Some(server.kind.clone()),
            source: Some("oauth".to_string()),
            field_group: Some("oauth".to_string()),
            field_key: Some(slot.key().to_string()),
            field_index: None,
            field_path: Some(format!("oauth.{}", slot.key())),
        });

        let metadata = match store.get_secret_metadata(&alias).await {
            Ok(_) => {
                store
                    .update_secret(SecretUpdateInput {
                        alias: alias.clone(),
                        kind: Some(slot.kind()),
                        value: Some(value),
                        label: Some(format!("{} for {}", slot.label(), server.name)),
                        origin,
                    })
                    .await?
            }
            Err(err) if err.to_string().contains("was not found") => {
                store
                    .create_secret(SecretCreateInput {
                        alias: alias.clone(),
                        kind: slot.kind(),
                        value,
                        label: Some(format!("{} for {}", slot.label(), server.name)),
                        origin,
                    })
                    .await?
            }
            Err(err) => return Err(err),
        };

        store
            .upsert_usage(SecretUsageUpsertInput {
                alias,
                server_id: server.id.clone(),
                location: SecretUsageLocationInput::OAuthToken,
            })
            .await?;
        Ok(metadata.placeholder)
    }

    async fn delete_oauth_secret_if_available(
        &self,
        server_id: &str,
        slot: OAuthSecretSlot,
    ) -> Result<()> {
        let store = self.secret_store()?;
        let alias = oauth_secret_alias(server_id, slot);
        match store.delete_secret(&alias, true).await {
            Ok(()) => Ok(()),
            Err(SecretStoreDeleteError::NotFound { .. }) => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    fn value_has_secret_reference(value: &str) -> Result<bool> {
        Ok(parse_placeholder(value.trim())?.is_some())
    }

    fn oauth_config_has_secret_references(config: &Option<ServerOAuthConfig>) -> Result<bool> {
        let Some(client_secret) = config
            .as_ref()
            .and_then(|config| config.client_secret.as_deref())
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(false);
        };

        Self::value_has_secret_reference(client_secret)
    }

    fn oauth_token_has_secret_references(token: &Option<ServerOAuthToken>) -> Result<bool> {
        let Some(token) = token.as_ref() else {
            return Ok(false);
        };
        for value in [Some(token.access_token.as_str()), token.refresh_token.as_deref()]
            .into_iter()
            .flatten()
            .filter(|value| !value.trim().is_empty())
        {
            if Self::value_has_secret_reference(value)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn oauth_has_secret_references(
        config: &Option<ServerOAuthConfig>,
        token: &Option<ServerOAuthToken>,
    ) -> Result<bool> {
        Ok(Self::oauth_config_has_secret_references(config)? || Self::oauth_token_has_secret_references(token)?)
    }

    fn ensure_oauth_secret_cleanup_available(
        &self,
        has_secret_references: bool,
        operation: &str,
    ) -> Result<()> {
        if has_secret_references && self.secret_store.is_none() {
            return Err(OAuthSecretCleanupUnavailable::new(match operation {
                "removing" => "removing",
                "revoking" => "revoking",
                _ => "managing",
            })
            .into());
        }
        Ok(())
    }

    async fn delete_oauth_token_secrets(
        &self,
        server_id: &str,
    ) -> Result<()> {
        self.delete_oauth_secret_if_available(server_id, OAuthSecretSlot::AccessToken)
            .await?;
        self.delete_oauth_secret_if_available(server_id, OAuthSecretSlot::RefreshToken)
            .await
    }

    pub async fn delete_all_oauth_secrets(
        &self,
        server_id: &str,
    ) -> Result<()> {
        let config = server::get_server_oauth_config(&self.pool, server_id).await?;
        let token = server::get_server_oauth_token(&self.pool, server_id).await?;
        let has_secret_references = Self::oauth_has_secret_references(&config, &token)?;
        self.ensure_oauth_secret_cleanup_available(has_secret_references, "removing")?;

        if !has_secret_references {
            return Ok(());
        }

        self.delete_oauth_secret_if_available(server_id, OAuthSecretSlot::ClientSecret)
            .await?;
        self.delete_oauth_token_secrets(server_id).await?;
        Ok(())
    }

    fn resolve_optional_client_secret(
        &self,
        stored: Option<&str>,
    ) -> Result<Option<String>> {
        stored
            .filter(|value| !value.trim().is_empty())
            .map(|value| self.resolve_oauth_secret_value(value, "client secret"))
            .transpose()
    }

    fn resolve_oauth_secret_value(
        &self,
        stored: &str,
        label: &str,
    ) -> Result<String> {
        let reference = parse_placeholder(stored)?.ok_or_else(|| {
            anyhow!(
                "OAuth {label} is not stored in Secure Store custody; re-save the OAuth configuration and reconnect the server"
            )
        })?;
        let alias = reference.alias().to_string();
        let value = match self.secret_resolver()?.resolve_secret(&reference) {
            Ok(value) => value,
            Err(error) => {
                let provider = self.secret_store.as_ref().map(|store| store.provider_metadata());
                let provider_id = provider
                    .as_ref()
                    .map(|metadata| metadata.provider_id())
                    .unwrap_or("none");
                let provider_kind = provider
                    .as_ref()
                    .map(|metadata| metadata.provider_kind())
                    .unwrap_or("none");
                let provider_mode = provider
                    .as_ref()
                    .map(|metadata| metadata.mode().as_str())
                    .unwrap_or("none");
                let security_level = provider
                    .as_ref()
                    .map(|metadata| metadata.security_level().as_str())
                    .unwrap_or("unknown");
                tracing::warn!(
                    oauth_secret_label = %label,
                    secret_alias = %alias,
                    error_code = secret_error_code(&error),
                    error = %error,
                    provider_id = provider_id,
                    provider_kind = provider_kind,
                    provider_mode = provider_mode,
                    security_level = security_level,
                    "Failed to resolve OAuth secret reference"
                );
                return Err(error.into());
            }
        };
        Ok(value.expose().to_string())
    }

    fn oauth_custody_status(
        &self,
        config: &Option<ServerOAuthConfig>,
        token: &Option<ServerOAuthToken>,
    ) -> Result<(OAuthCustodyState, bool, Option<OAuthStatusIssue>)> {
        if config.is_none() {
            return Ok((OAuthCustodyState::Missing, false, None));
        }

        let mut values = Vec::new();
        if let Some(client_secret) = config.as_ref().and_then(|item| item.client_secret.as_deref()) {
            values.push(client_secret);
        }
        if let Some(token) = token.as_ref() {
            values.push(token.access_token.as_str());
            if let Some(refresh_token) = token.refresh_token.as_deref() {
                values.push(refresh_token);
            }
        }

        super::types::classify_custody(self.secret_store.is_some(), &values)
    }

    pub async fn upsert_config(
        &self,
        server_id: &str,
        input: OAuthConfigInput,
    ) -> Result<OAuthStatus> {
        let transport = self.load_oauth_transport(server_id).await?;
        let server_context = self.oauth_server_context(server_id, &transport).await?;
        let existing = server::get_server_oauth_config(&self.pool, server_id).await?;
        let client_secret = match input.client_secret {
            Some(secret) if !secret.trim().is_empty() => {
                Some(self.store_or_preserve_client_secret(&server_context, &secret).await?)
            }
            _ => existing.as_ref().and_then(|item| item.client_secret.clone()),
        };
        let config = ServerOAuthConfig {
            id: existing.as_ref().and_then(|item| item.id.clone()),
            server_id: server_id.to_string(),
            authorization_endpoint: input.authorization_endpoint.trim().to_string(),
            token_endpoint: input.token_endpoint.trim().to_string(),
            client_id: input.client_id.trim().to_string(),
            client_secret,
            scopes: input.scopes.and_then(|scopes| {
                let trimmed = scopes.trim().to_string();
                (!trimmed.is_empty()).then_some(trimmed)
            }),
            redirect_uri: input.redirect_uri.trim().to_string(),
            created_at: None,
            updated_at: None,
        };
        server::upsert_server_oauth_config(&self.pool, &config).await?;
        self.get_status(server_id).await
    }

    pub async fn prepare(
        &self,
        server_id: &str,
        input: OAuthPrepareInput,
    ) -> Result<OAuthStatus> {
        let transport = self.load_oauth_transport(server_id).await?;
        let resource_url = oauth_resource_url_from_transport(&transport)?;

        let existing = server::get_server_oauth_config(&self.pool, server_id).await?;
        if let Some(existing_config) = existing.as_ref() {
            if !existing_config.authorization_endpoint.trim().is_empty()
                && !existing_config.token_endpoint.trim().is_empty()
                && !existing_config.client_id.trim().is_empty()
            {
                return self
                    .upsert_config(
                        server_id,
                        OAuthConfigInput {
                            authorization_endpoint: existing_config.authorization_endpoint.clone(),
                            token_endpoint: existing_config.token_endpoint.clone(),
                            client_id: existing_config.client_id.clone(),
                            client_secret: existing_config.client_secret.clone(),
                            scopes: input.scopes.or_else(|| existing_config.scopes.clone()),
                            redirect_uri: input.redirect_uri,
                        },
                    )
                    .await;
            }
        }

        let protected_metadata = self.discover_protected_resource_metadata(&resource_url).await?;
        let authorization_server = protected_metadata
            .authorization_servers
            .as_ref()
            .and_then(|servers| servers.first())
            .cloned()
            .unwrap_or_else(|| resource_origin(&resource_url));
        let authorization_metadata = self
            .discover_authorization_server_metadata(&authorization_server)
            .await?;

        let registration = match authorization_metadata.registration_endpoint.as_deref() {
            Some(endpoint) => Some(self.register_dynamic_client(endpoint, &input.redirect_uri).await?),
            None => None,
        };

        let client_id = registration
            .as_ref()
            .map(|item| item.client_id.clone())
            .ok_or_else(|| anyhow!("OAuth discovery succeeded, but the authorization server does not support automatic client registration yet"))?;
        let scopes = input
            .scopes
            .or_else(|| registration.as_ref().and_then(|item| item.scope.clone()))
            .or_else(|| {
                protected_metadata
                    .scopes_supported
                    .as_ref()
                    .map(|scopes| scopes.join(" "))
            })
            .or_else(|| {
                authorization_metadata
                    .scopes_supported
                    .as_ref()
                    .map(|scopes| scopes.join(" "))
            });

        self.upsert_config(
            server_id,
            OAuthConfigInput {
                authorization_endpoint: authorization_metadata.authorization_endpoint,
                token_endpoint: authorization_metadata.token_endpoint,
                client_id,
                client_secret: registration.and_then(|item| item.client_secret),
                scopes,
                redirect_uri: input.redirect_uri,
            },
        )
        .await
    }

    pub async fn initiate(
        &self,
        server_id: &str,
    ) -> Result<OAuthInitiateResult> {
        let transport = self.load_oauth_transport(server_id).await?;
        let config = server::get_server_oauth_config(&self.pool, server_id)
            .await?
            .ok_or_else(|| anyhow!("OAuth is not configured for server '{}'", server_id))?;
        let resource = oauth_resource_from_transport(&transport)?;

        let state = generate_oauth_random(32);
        let code_verifier = generate_oauth_random(64);
        let code_challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(code_verifier.as_bytes()));

        let mut url = Url::parse(&config.authorization_endpoint)
            .with_context(|| format!("Invalid authorization endpoint '{}'", config.authorization_endpoint))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("response_type", "code");
            query.append_pair("client_id", &config.client_id);
            query.append_pair("redirect_uri", &config.redirect_uri);
            query.append_pair("resource", &resource);
            query.append_pair("state", &state);
            query.append_pair("code_challenge", &code_challenge);
            query.append_pair("code_challenge_method", "S256");
            if let Some(scopes) = config.scopes.as_ref().filter(|value| !value.trim().is_empty()) {
                query.append_pair("scope", scopes);
            }
        }

        let mut flows = self.pending_flows.lock().await;
        flows.retain(|_, flow| Utc::now() - flow.created_at < Duration::minutes(10));
        flows.insert(
            state.clone(),
            PendingOAuthFlow {
                server_id: server_id.to_string(),
                code_verifier,
                created_at: Utc::now(),
            },
        );

        Ok(OAuthInitiateResult {
            server_id: server_id.to_string(),
            authorization_url: url.to_string(),
            state,
        })
    }

    pub async fn exchange_code(
        &self,
        state: &str,
        code: &str,
    ) -> Result<OAuthStatus> {
        let pending = {
            let mut flows = self.pending_flows.lock().await;
            flows.remove(state)
        }
        .ok_or_else(|| anyhow!("Invalid or expired OAuth state"))?;

        let config = server::get_server_oauth_config(&self.pool, &pending.server_id)
            .await?
            .ok_or_else(|| anyhow!("OAuth config missing for server '{}'", pending.server_id))?;
        let transport = self.load_oauth_transport(&pending.server_id).await?;
        let resource = oauth_resource_from_transport(&transport)?;

        let mut form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("redirect_uri", config.redirect_uri.clone()),
            ("client_id", config.client_id.clone()),
            ("code_verifier", pending.code_verifier.clone()),
            ("resource", resource),
        ];
        if let Some(secret) = self.resolve_optional_client_secret(config.client_secret.as_deref())? {
            form.push(("client_secret", secret));
        }

        let token_response = self.request_token_response(&config.token_endpoint, &form).await?;
        server::ensure_persisted_http_authorization_headers_clearable(&self.pool, &pending.server_id).await?;
        let server_context = self.oauth_server_context(&pending.server_id, &transport).await?;

        self.store_oauth_token_with_context(&server_context, token_response, None)
            .await?;
        server::clear_persisted_http_authorization_headers(&self.pool, &pending.server_id).await?;

        self.get_status(&pending.server_id).await
    }

    pub async fn get_effective_server_headers(
        &self,
        server_id: &str,
        manual_headers: Option<HashMap<String, String>>,
    ) -> Result<Option<HashMap<String, String>>> {
        if server::has_manual_authorization_header(&manual_headers) {
            return Ok(manual_headers.filter(|headers| !headers.is_empty()));
        }

        let mut effective = manual_headers.unwrap_or_default();
        if let Some(token) = server::get_server_oauth_token(&self.pool, server_id).await? {
            // Skip OAuth token injection when the secret resolver is unavailable
            // (e.g. locked passphrase store at startup). The server will start
            // without the Bearer header; the user can unlock the store later and
            // re-authorize to restore authenticated access.
            if self.secret_resolver.is_none() {
                tracing::warn!(
                    server_id = %server_id,
                    "Secure Store not available; skipping OAuth token injection for startup"
                );
                return if effective.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(effective))
                };
            }

            let token = if is_token_refreshable(&token) {
                self.refresh_access_token(server_id).await?
            } else {
                token
            };

            if !is_token_expired(&token) && !token.access_token.trim().is_empty() {
                let access_token = self.resolve_oauth_secret_value(&token.access_token, "access token")?;
                effective.insert("authorization".to_string(), format!("Bearer {}", access_token.trim()));
            }
        }

        if effective.is_empty() {
            Ok(None)
        } else {
            Ok(Some(effective))
        }
    }

    pub async fn refresh_access_token(
        &self,
        server_id: &str,
    ) -> Result<ServerOAuthToken> {
        let config = server::get_server_oauth_config(&self.pool, server_id)
            .await?
            .ok_or_else(|| anyhow!("OAuth config missing for server '{}'", server_id))?;
        let token = server::get_server_oauth_token(&self.pool, server_id)
            .await?
            .ok_or_else(|| anyhow!("OAuth token missing for server '{}'", server_id))?;
        let refresh_token_ref = token
            .refresh_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow!("OAuth token for server '{}' has no refresh token", server_id))?;
        let refresh_token = self.resolve_oauth_secret_value(refresh_token_ref, "refresh token")?;
        let transport = self.load_oauth_transport(server_id).await?;
        let resource = oauth_resource_from_transport(&transport)?;
        let server_context = self.oauth_server_context(server_id, &transport).await?;

        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token.trim().to_string()),
            ("client_id", config.client_id.clone()),
            ("resource", resource),
        ];
        if let Some(secret) = self.resolve_optional_client_secret(config.client_secret.as_deref())? {
            form.push(("client_secret", secret));
        }

        let token_response = self.request_token_response(&config.token_endpoint, &form).await?;
        if token_response.access_token.trim().is_empty() {
            bail!("OAuth refresh response did not include a usable access_token");
        }

        self.store_oauth_token_with_context(
            &server_context,
            token_response,
            Some(ExistingTokenContext {
                id: token.id,
                refresh_token: token.refresh_token,
                token_type: token.token_type,
                scope: token.scope,
            }),
        )
        .await
    }

    #[cfg(test)]
    async fn store_oauth_token_for_server(
        &self,
        server_id: &str,
        token_response: OAuthTokenResponse,
        existing: Option<ExistingTokenContext>,
    ) -> Result<ServerOAuthToken> {
        let transport = self.load_oauth_transport(server_id).await?;
        let server_context = self.oauth_server_context(server_id, &transport).await?;
        self.store_oauth_token_with_context(&server_context, token_response, existing)
            .await
    }

    async fn store_oauth_token_with_context(
        &self,
        server: &OAuthServerContext,
        token_response: OAuthTokenResponse,
        existing: Option<ExistingTokenContext>,
    ) -> Result<ServerOAuthToken> {
        let access_token = self
            .store_oauth_secret(server, OAuthSecretSlot::AccessToken, token_response.access_token)
            .await?;
        let existing_id = existing.as_ref().and_then(|ctx| ctx.id.clone());
        let existing_refresh_token = existing.as_ref().and_then(|ctx| ctx.refresh_token.clone());
        let existing_token_type = existing
            .as_ref()
            .map(|ctx| ctx.token_type.clone())
            .unwrap_or_else(|| "bearer".to_string());
        let existing_scope = existing.as_ref().and_then(|ctx| ctx.scope.clone());
        let incoming_refresh_token = token_response.refresh_token.filter(|value| !value.trim().is_empty());
        let should_delete_stale_refresh_token = existing_id.is_none() && incoming_refresh_token.is_none();
        let refresh_token = match incoming_refresh_token {
            Some(refresh_token) => Some(
                self.store_oauth_secret(server, OAuthSecretSlot::RefreshToken, refresh_token)
                    .await?,
            ),
            None => existing_refresh_token,
        };
        let expires_at = token_response
            .expires_in
            .map(|seconds| (Utc::now() + Duration::seconds(seconds)).to_rfc3339());
        let stored = ServerOAuthToken {
            id: existing_id,
            server_id: server.id.clone(),
            access_token,
            refresh_token,
            token_type: token_response.token_type.unwrap_or(existing_token_type),
            expires_at,
            scope: token_response.scope.or(existing_scope),
            created_at: None,
            updated_at: None,
        };

        server::upsert_server_oauth_token(&self.pool, &stored).await?;
        if should_delete_stale_refresh_token {
            self.delete_oauth_secret_if_available(&server.id, OAuthSecretSlot::RefreshToken)
                .await?;
        }
        Ok(stored)
    }

    pub async fn get_status(
        &self,
        server_id: &str,
    ) -> Result<OAuthStatus> {
        let transport = self.load_oauth_transport(server_id).await?;
        let config = server::get_server_oauth_config(&self.pool, server_id).await?;
        let mut token = server::get_server_oauth_token(&self.pool, server_id).await?;
        let manual_authorization_override = server::has_manual_authorization_header(&Some(transport.runtime_headers()));
        let mut refresh_issue = None;
        if !manual_authorization_override && token.as_ref().is_some_and(is_token_refreshable) {
            match self.refresh_access_token(server_id).await {
                Ok(refreshed) => token = Some(refreshed),
                Err(error) => {
                    refresh_issue = oauth_secret_issue_from_error(&error);
                    tracing::warn!(
                        server_id = %server_id,
                        error = %error,
                        "Failed to refresh expired OAuth token while reading OAuth status"
                    );
                }
            }
        }
        let (mut custody_state, mut requires_reconnect, mut issue) = self.oauth_custody_status(&config, &token)?;
        if let Some(refresh_issue) = refresh_issue {
            if refresh_issue.code == OAUTH_ISSUE_SECURE_STORE_UNAVAILABLE {
                custody_state = OAuthCustodyState::Unavailable;
            }
            requires_reconnect = true;
            issue = Some(refresh_issue);
        }

        let state = match (&config, &token) {
            (None, _) => OAuthConnectionState::NotConfigured,
            (Some(_), None) => OAuthConnectionState::Disconnected,
            (Some(_), Some(token_row)) => {
                if is_token_expired(token_row) {
                    OAuthConnectionState::Expired
                } else {
                    OAuthConnectionState::Connected
                }
            }
        };

        Ok(OAuthStatus {
            server_id: server_id.to_string(),
            configured: config.is_some(),
            state,
            authorization_endpoint: config.as_ref().map(|item| item.authorization_endpoint.clone()),
            token_endpoint: config.as_ref().map(|item| item.token_endpoint.clone()),
            client_id: config.as_ref().map(|item| item.client_id.clone()),
            scopes: config.as_ref().and_then(|item| item.scopes.clone()),
            redirect_uri: config.as_ref().map(|item| item.redirect_uri.clone()),
            has_client_secret: config
                .as_ref()
                .and_then(|item| item.client_secret.as_ref())
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false),
            manual_authorization_override,
            expires_at: token.and_then(|item| item.expires_at),
            custody_state,
            requires_reconnect,
            issue,
        })
    }

    pub async fn revoke(
        &self,
        server_id: &str,
    ) -> Result<OAuthStatus> {
        let token = server::get_server_oauth_token(&self.pool, server_id).await?;
        let has_secret_references = Self::oauth_token_has_secret_references(&token)?;
        self.ensure_oauth_secret_cleanup_available(has_secret_references, "revoking")?;

        if self.secret_store.is_some() {
            self.delete_oauth_token_secrets(server_id).await?;
        }
        server::delete_server_oauth_token(&self.pool, server_id).await?;
        self.get_status(server_id).await
    }
}

impl OAuthManager {
    async fn discover_protected_resource_metadata(
        &self,
        resource_url: &Url,
    ) -> Result<ProtectedResourceMetadata> {
        if let Some(metadata) = self.discover_protected_metadata_from_challenge(resource_url).await? {
            return Ok(metadata);
        }

        let candidates = protected_resource_candidates(resource_url)?;
        for candidate in candidates {
            if let Ok(metadata) = self.fetch_json::<ProtectedResourceMetadata>(&candidate).await {
                return Ok(metadata);
            }
        }

        Ok(ProtectedResourceMetadata {
            authorization_servers: Some(vec![resource_origin(resource_url)]),
            scopes_supported: None,
        })
    }

    async fn discover_protected_metadata_from_challenge(
        &self,
        resource_url: &Url,
    ) -> Result<Option<ProtectedResourceMetadata>> {
        let response = self.http_client.get(resource_url.clone()).send().await;
        let response = match response {
            Ok(response) => response,
            Err(_) => return Ok(None),
        };
        if response.status() != reqwest::StatusCode::UNAUTHORIZED {
            return Ok(None);
        }

        let mut resource_metadata_url: Option<String> = None;
        let mut authorization_endpoint: Option<String> = None;
        let mut token_endpoint: Option<String> = None;
        let mut scopes: Option<String> = None;

        for value in response.headers().get_all(WWW_AUTHENTICATE) {
            if let Ok(raw) = value.to_str() {
                let params = parse_www_authenticate(raw);
                resource_metadata_url = resource_metadata_url.or_else(|| params.get("resource_metadata").cloned());
                authorization_endpoint = authorization_endpoint.or_else(|| params.get("authorization_uri").cloned());
                token_endpoint = token_endpoint.or_else(|| params.get("token_uri").cloned());
                scopes = scopes.or_else(|| params.get("scope").cloned());
            }
        }

        if let Some(resource_metadata_url) = resource_metadata_url {
            if let Ok(metadata) = self
                .fetch_json::<ProtectedResourceMetadata>(&resource_metadata_url)
                .await
            {
                return Ok(Some(metadata));
            }
        }

        if let (Some(authorization_endpoint), Some(_token_endpoint)) = (authorization_endpoint, token_endpoint) {
            return Ok(Some(ProtectedResourceMetadata {
                authorization_servers: Some(vec![issuer_from_endpoint(&authorization_endpoint)]),
                scopes_supported: scopes.map(|value| value.split_whitespace().map(ToOwned::to_owned).collect()),
            }));
        }

        Ok(None)
    }

    async fn discover_authorization_server_metadata(
        &self,
        issuer_or_base: &str,
    ) -> Result<AuthorizationServerMetadata> {
        let mut candidates = Vec::new();
        let base = issuer_or_base.trim_end_matches('/');
        candidates.push(format!("{base}/.well-known/oauth-authorization-server"));
        candidates.push(format!("{base}/.well-known/openid-configuration"));

        for candidate in candidates {
            if let Ok(metadata) = self.fetch_json::<AuthorizationServerMetadata>(&candidate).await {
                return Ok(metadata);
            }
        }

        bail!("Failed to discover OAuth authorization server metadata")
    }

    async fn register_dynamic_client(
        &self,
        registration_endpoint: &str,
        redirect_uri: &str,
    ) -> Result<DynamicClientRegistrationResponse> {
        let payload = serde_json::json!({
            "client_name": "MCPMate",
            "application_type": "native",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "redirect_uris": [redirect_uri],
            "token_endpoint_auth_method": "none"
        });

        self.http_client
            .post(registration_endpoint)
            .json(&payload)
            .send()
            .await
            .context("Failed to call OAuth dynamic client registration endpoint")?
            .error_for_status()
            .context("OAuth dynamic client registration returned error status")?
            .json::<DynamicClientRegistrationResponse>()
            .await
            .context("Failed to parse OAuth dynamic client registration response")
    }

    async fn request_token_response(
        &self,
        token_endpoint: &str,
        form: &[(&str, String)],
    ) -> Result<OAuthTokenResponse> {
        let endpoint_url = Url::parse(token_endpoint).context("Invalid OAuth token_endpoint URL")?;
        let is_loopback = matches!(endpoint_url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
        if endpoint_url.scheme() != "https" && !is_loopback {
            bail!("OAuth token endpoint must use HTTPS: {}", token_endpoint);
        }

        let encoded_body = {
            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            for (key, value) in form {
                serializer.append_pair(key, value);
            }
            serializer.finish()
        };

        let client = if is_loopback {
            reqwest::Client::builder()
                .no_proxy()
                .build()
                .context("Failed to build loopback OAuth token client")?
        } else {
            self.http_client.clone()
        };

        let endpoint = token_endpoint_error_target(&endpoint_url);
        let response = client
            .post(token_endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .timeout(OAUTH_TOKEN_REQUEST_TIMEOUT)
            .body(encoded_body)
            .send()
            .await
            .map_err(|error| {
                tracing::warn!(
                    endpoint = %endpoint,
                    error = %error,
                    "Failed to call OAuth token endpoint"
                );
                error
            })
            .context("Failed to call OAuth token endpoint")?;

        if !response.status().is_success() {
            let status = response.status();
            let response_body = response.text().await.unwrap_or_default();
            if let Some(oauth_error) = oauth_error_code_from_body(&response_body) {
                tracing::warn!(
                    endpoint = %endpoint,
                    status = %status,
                    oauth_error = %oauth_error,
                    "OAuth token endpoint returned error status"
                );
                bail!(
                    "OAuth token endpoint returned error status: {status} for {endpoint}. OAuth error: {oauth_error}"
                );
            }

            tracing::warn!(
                endpoint = %endpoint,
                status = %status,
                "OAuth token endpoint returned error status"
            );
            bail!("OAuth token endpoint returned error status: {status} for {endpoint}");
        }

        response
            .json::<OAuthTokenResponse>()
            .await
            .context("Failed to parse OAuth token response")
    }

    async fn fetch_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: &str,
    ) -> Result<T> {
        self.http_client
            .get(url)
            .send()
            .await
            .with_context(|| format!("Failed to fetch OAuth metadata from '{url}'"))?
            .error_for_status()
            .with_context(|| format!("OAuth metadata endpoint '{url}' returned error status"))?
            .json::<T>()
            .await
            .with_context(|| format!("Failed to parse OAuth metadata from '{url}'"))
    }
}

fn token_endpoint_error_target(endpoint_url: &Url) -> String {
    let mut target = endpoint_url.clone();
    target.set_query(None);
    target.set_fragment(None);
    target.to_string()
}

fn oauth_error_code_from_body(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    value
        .get("error")
        .and_then(|error| error.as_str())
        .map(|error| {
            error
                .chars()
                .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.'))
                .take(80)
                .collect::<String>()
        })
        .filter(|error| !error.is_empty())
}

fn resource_origin(resource_url: &Url) -> String {
    let mut origin = resource_url.clone();
    origin.set_path("");
    origin.set_query(None);
    origin.set_fragment(None);
    origin.to_string().trim_end_matches('/').to_string()
}

fn issuer_from_endpoint(endpoint: &str) -> String {
    Url::parse(endpoint)
        .ok()
        .map(|url| resource_origin(&url))
        .unwrap_or_else(|| endpoint.trim_end_matches('/').to_string())
}

fn oauth_resource_url_from_transport(transport: &ValidatedTransport) -> Result<Url> {
    let mut resource = match transport {
        ValidatedTransport::Sse { endpoint, .. } | ValidatedTransport::StreamableHttp { endpoint, .. } => {
            endpoint.clone()
        }
        ValidatedTransport::Stdio { .. } => {
            bail!("OAuth is only supported for HTTP-based MCP servers (sse or streamable_http)")
        }
    };
    resource
        .set_username("")
        .map_err(|_| anyhow!("Invalid HTTP transport endpoint"))?;
    resource
        .set_password(None)
        .map_err(|_| anyhow!("Invalid HTTP transport endpoint"))?;
    resource.set_query(None);
    resource.set_fragment(None);
    Ok(resource)
}

fn oauth_resource_from_transport(transport: &ValidatedTransport) -> Result<String> {
    let resource = oauth_resource_url_from_transport(transport)?;
    Ok(resource.to_string().trim_end_matches('/').to_string())
}

fn protected_resource_candidates(resource_url: &Url) -> Result<Vec<String>> {
    let origin = resource_origin(resource_url);
    let mut candidates = vec![format!("{origin}/.well-known/oauth-protected-resource")];
    let path = resource_url.path().trim_start_matches('/');
    if !path.is_empty() {
        candidates.push(format!("{origin}/.well-known/oauth-protected-resource/{path}"));
    }
    Ok(candidates)
}

fn parse_www_authenticate(header_value: &str) -> HashMap<String, String> {
    let trimmed = header_value.trim();
    let payload = trimmed
        .strip_prefix("Bearer ")
        .or_else(|| trimmed.strip_prefix("bearer "))
        .unwrap_or(trimmed);

    payload
        .split(',')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            let parsed = value.trim().trim_matches('"').to_string();
            Some((key.trim().to_string(), parsed))
        })
        .collect()
}

fn generate_oauth_random(size: usize) -> String {
    nanoid::nanoid!(size, &nanoid::alphabet::SAFE)
}

pub(crate) fn oauth_secret_alias(
    server_id: &str,
    slot: OAuthSecretSlot,
) -> String {
    format!("oauth/{}/{}", server_id, slot.key())
}

fn is_token_expired(token: &ServerOAuthToken) -> bool {
    token
        .expires_at
        .as_ref()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|expires_at| expires_at.with_timezone(&Utc) <= Utc::now())
        .unwrap_or(false)
}

fn is_token_refreshable(token: &ServerOAuthToken) -> bool {
    is_token_expired(token)
        && token
            .refresh_token
            .as_ref()
            .is_some_and(|value| !value.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        common::{server::ServerType, status::EnabledStatus},
        config::{
            llm::init::initialize_llm_tables,
            models::{ConfigValue, HttpTransportKind, Server, ServerTransportDraft},
            server::{self, init::initialize_server_tables},
        },
        core::secrets::store::LocalSecretStore,
    };
    use std::{collections::BTreeMap, sync::Arc};
    use tempfile::TempDir;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, method, path},
    };

    async fn setup_manager() -> OAuthManager {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("enable foreign keys");
        crate::helpers::prepare_config_database(&pool).await;
        initialize_server_tables(&pool).await.expect("init tables");
        initialize_llm_tables(&pool).await.expect("init llm tables");
        OAuthManager::new(pool)
    }

    async fn setup_secure_manager() -> (OAuthManager, Arc<LocalSecretStore>, TempDir) {
        let temp_dir = TempDir::new().expect("temp dir");
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory sqlite");
        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("enable foreign keys");
        crate::helpers::prepare_config_database(&pool).await;
        initialize_server_tables(&pool).await.expect("init tables");
        initialize_llm_tables(&pool).await.expect("init llm tables");
        let store = Arc::new(
            LocalSecretStore::initialize_with_development_root_key(
                pool.clone(),
                temp_dir.path().join("secrets").join("local-root.key"),
            )
            .await
            .expect("initialize secret store"),
        );
        let manager = OAuthManager::new_optional_store(pool, Some(store.clone()));
        (manager, store, temp_dir)
    }

    async fn insert_server(
        pool: &SqlitePool,
        id: &str,
    ) {
        insert_typed_http_server(pool, id, BTreeMap::new()).await;
    }

    async fn insert_typed_http_server(
        pool: &SqlitePool,
        id: &str,
        headers: BTreeMap<String, ConfigValue>,
    ) {
        let server = Server {
            id: Some(id.to_string()),
            name: format!("server-{id}"),
            server_type: ServerType::StreamableHttp,
            command: None,
            url: Some("https://example.com/mcp".to_string()),
            source: None,
            enabled: EnabledStatus::Enabled,
            unify_direct_exposure_eligible: false,
            pending_import: false,
            created_at: None,
            updated_at: None,
        };
        server::upsert_server_definition(
            pool,
            &server,
            &ServerTransportDraft::Http {
                protocol: HttpTransportKind::StreamableHttp,
                endpoint: Some("https://example.com/mcp".to_string()),
                headers,
            },
        )
        .await
        .expect("insert typed HTTP server");
    }

    #[tokio::test]
    async fn initiate_returns_valid_authorization_url() {
        let manager = setup_manager().await;
        insert_server(&manager.pool, "serv_initiate").await;
        manager
            .upsert_config(
                "serv_initiate",
                OAuthConfigInput {
                    authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                    token_endpoint: "https://issuer.example.com/token".to_string(),
                    client_id: "client-1".to_string(),
                    client_secret: None,
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save oauth config");

        let initiate = manager.initiate("serv_initiate").await.expect("initiate oauth");
        assert!(initiate.authorization_url.contains("response_type=code"));
        assert!(initiate.authorization_url.contains("code_challenge="));
        assert!(initiate.authorization_url.contains("code_challenge_method=S256"));
        assert!(initiate.authorization_url.contains("client_id=client-1"));
        assert!(
            initiate
                .authorization_url
                .contains("resource=https%3A%2F%2Fexample.com%2Fmcp")
        );
    }

    #[tokio::test]
    async fn exchange_code_stores_tokens() {
        let (manager, store, _temp_dir) = setup_secure_manager().await;
        insert_typed_http_server(
            &manager.pool,
            "serv_exchange",
            BTreeMap::from([
                (
                    "Authorization".to_string(),
                    ConfigValue::Literal {
                        value: "Bearer manual-token".to_string(),
                    },
                ),
                (
                    "X-Trace".to_string(),
                    ConfigValue::SecretRef {
                        alias: "trace-token".to_string(),
                    },
                ),
            ]),
        )
        .await;
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("resource=https%3A%2F%2Fexample.com%2Fmcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-123",
                "refresh_token": "refresh-123",
                "token_type": "bearer",
                "expires_in": 3600,
                "scope": "read write"
            })))
            .mount(&mock_server)
            .await;

        manager
            .upsert_config(
                "serv_exchange",
                OAuthConfigInput {
                    authorization_endpoint: format!("{}/authorize", mock_server.uri()),
                    token_endpoint: format!("{}/token", mock_server.uri()),
                    client_id: "client-1".to_string(),
                    client_secret: None,
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save oauth config");
        let initiate = manager.initiate("serv_exchange").await.expect("initiate oauth");
        let status = manager
            .exchange_code(&initiate.state, "code-123")
            .await
            .expect("exchange code");

        assert!(matches!(status.state, OAuthConnectionState::Connected));
        let stored = server::get_server_oauth_token(&manager.pool, "serv_exchange")
            .await
            .expect("load stored token")
            .expect("stored token exists");
        assert_eq!(
            mcpmate_secrets::resolve_placeholders(&stored.access_token, store.as_ref()).expect("resolve access"),
            "access-123"
        );
        let headers = server::get_server_headers(&manager.pool, "serv_exchange")
            .await
            .expect("load headers");
        assert!(!headers.contains_key("authorization"));
        assert_eq!(
            headers.get("x-trace").map(String::as_str),
            Some("[[secret:trace-token]]")
        );

        let Some(ServerTransportDraft::Http { headers, .. }) =
            server::get_server_transport_draft(&manager.pool, "serv_exchange")
                .await
                .expect("load typed transport draft")
        else {
            panic!("typed HTTP draft must be retained after OAuth exchange");
        };
        assert!(!headers.keys().any(|key| key.eq_ignore_ascii_case("authorization")));
        assert_eq!(
            headers.get("X-Trace"),
            Some(&ConfigValue::SecretRef {
                alias: "trace-token".to_string(),
            })
        );

        let effective = manager
            .get_effective_server_headers(
                "serv_exchange",
                Some(
                    ServerTransportDraft::Http {
                        protocol: HttpTransportKind::StreamableHttp,
                        endpoint: Some("https://example.com/mcp".to_string()),
                        headers,
                    }
                    .validate()
                    .expect("validate persisted typed transport")
                    .runtime_headers(),
                ),
            )
            .await
            .expect("resolve effective headers")
            .expect("effective headers");
        assert_eq!(
            effective.get("authorization").map(String::as_str),
            Some("Bearer access-123")
        );
        assert_eq!(
            effective.get("X-Trace").map(String::as_str),
            Some("[[secret:trace-token]]")
        );
    }

    #[tokio::test]
    async fn upsert_config_stores_client_secret_in_secure_store() {
        let (manager, store, _temp_dir) = setup_secure_manager().await;
        insert_server(&manager.pool, "serv_secure_config").await;

        manager
            .upsert_config(
                "serv_secure_config",
                OAuthConfigInput {
                    authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                    token_endpoint: "https://issuer.example.com/token".to_string(),
                    client_id: "client-1".to_string(),
                    client_secret: Some("client-secret-1".to_string()),
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save secure oauth config");

        let stored = server::get_server_oauth_config(&manager.pool, "serv_secure_config")
            .await
            .expect("load oauth config")
            .expect("oauth config exists");
        let stored_secret = stored.client_secret.expect("client secret ref");
        assert_ne!(stored_secret, "client-secret-1");
        assert!(stored_secret.starts_with("[[secret:"));
        assert_eq!(
            mcpmate_secrets::resolve_placeholders(&stored_secret, store.as_ref()).expect("resolve secret"),
            "client-secret-1"
        );

        let metadata = store
            .list_secret_metadata()
            .await
            .expect("list secrets")
            .into_iter()
            .find(|secret| secret.kind == "oauth_client_secret")
            .expect("client secret metadata");
        assert_eq!(
            metadata.origin.as_ref().and_then(|origin| origin.source.as_deref()),
            Some("oauth")
        );
    }

    #[tokio::test]
    async fn upsert_config_rejects_unrelated_client_secret_placeholder() {
        let (manager, _store, _temp_dir) = setup_secure_manager().await;
        insert_server(&manager.pool, "serv_reject_placeholder").await;

        let error = manager
            .upsert_config(
                "serv_reject_placeholder",
                OAuthConfigInput {
                    authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                    token_endpoint: "https://issuer.example.com/token".to_string(),
                    client_id: "client-1".to_string(),
                    client_secret: Some("[[secret:server/other/header-token]]".to_string()),
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect_err("unrelated placeholder should be rejected");

        assert!(error.to_string().contains("OAuth client secret placeholder"));
    }

    #[tokio::test]
    async fn status_marks_legacy_plaintext_oauth_values_as_reconnect_required() {
        let (manager, _store, _temp_dir) = setup_secure_manager().await;
        insert_server(&manager.pool, "serv_legacy_plaintext").await;

        server::upsert_server_oauth_config(
            &manager.pool,
            &ServerOAuthConfig {
                id: None,
                server_id: "serv_legacy_plaintext".to_string(),
                authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                token_endpoint: "https://issuer.example.com/token".to_string(),
                client_id: "client-1".to_string(),
                client_secret: Some("legacy-client-secret".to_string()),
                scopes: Some("read write".to_string()),
                redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("save legacy oauth config");
        server::upsert_server_oauth_token(
            &manager.pool,
            &ServerOAuthToken {
                id: None,
                server_id: "serv_legacy_plaintext".to_string(),
                access_token: "legacy-access-token".to_string(),
                refresh_token: Some("legacy-refresh-token".to_string()),
                token_type: "bearer".to_string(),
                expires_at: Some((Utc::now() + Duration::minutes(5)).to_rfc3339()),
                scope: Some("read write".to_string()),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("save legacy oauth token");

        let status = manager
            .get_status("serv_legacy_plaintext")
            .await
            .expect("load oauth status");

        assert!(matches!(status.state, OAuthConnectionState::Connected));
        assert!(matches!(status.custody_state, OAuthCustodyState::LegacyPlaintext));
        assert!(status.requires_reconnect);
        assert_eq!(
            status.issue.as_ref().map(|issue| issue.code.as_str()),
            Some("legacy_plaintext_oauth_credentials")
        );
    }

    #[tokio::test]
    async fn status_reports_secure_store_unavailable_for_configured_oauth() {
        let manager = setup_manager().await;
        insert_server(&manager.pool, "serv_store_unavailable").await;

        server::upsert_server_oauth_config(
            &manager.pool,
            &ServerOAuthConfig {
                id: None,
                server_id: "serv_store_unavailable".to_string(),
                authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                token_endpoint: "https://issuer.example.com/token".to_string(),
                client_id: "client-1".to_string(),
                client_secret: None,
                scopes: Some("read write".to_string()),
                redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("save oauth config");

        let status = manager
            .get_status("serv_store_unavailable")
            .await
            .expect("load oauth status");

        assert!(matches!(status.state, OAuthConnectionState::Disconnected));
        assert!(matches!(status.custody_state, OAuthCustodyState::Unavailable));
        assert!(status.requires_reconnect);
        assert_eq!(
            status.issue.as_ref().map(|issue| issue.code.as_str()),
            Some(OAUTH_ISSUE_SECURE_STORE_UNAVAILABLE)
        );
    }

    #[tokio::test]
    async fn exchange_code_stores_tokens_in_secure_store() {
        let (manager, store, _temp_dir) = setup_secure_manager().await;
        insert_typed_http_server(&manager.pool, "serv_secure_exchange", BTreeMap::new()).await;
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-123",
                "refresh_token": "refresh-123",
                "token_type": "bearer",
                "expires_in": 3600,
                "scope": "read write"
            })))
            .mount(&mock_server)
            .await;

        manager
            .upsert_config(
                "serv_secure_exchange",
                OAuthConfigInput {
                    authorization_endpoint: format!("{}/authorize", mock_server.uri()),
                    token_endpoint: format!("{}/token", mock_server.uri()),
                    client_id: "client-1".to_string(),
                    client_secret: None,
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save oauth config");

        let initiate = manager.initiate("serv_secure_exchange").await.expect("initiate oauth");
        manager
            .exchange_code(&initiate.state, "code-123")
            .await
            .expect("exchange code");

        let stored = server::get_server_oauth_token(&manager.pool, "serv_secure_exchange")
            .await
            .expect("load stored token")
            .expect("stored token exists");
        assert_ne!(stored.access_token, "access-123");
        assert_ne!(stored.refresh_token.as_deref(), Some("refresh-123"));
        assert_eq!(
            mcpmate_secrets::resolve_placeholders(&stored.access_token, store.as_ref()).expect("resolve access"),
            "access-123"
        );
        assert_eq!(
            mcpmate_secrets::resolve_placeholders(
                stored.refresh_token.as_deref().expect("refresh ref"),
                store.as_ref()
            )
            .expect("resolve refresh"),
            "refresh-123"
        );

        let headers = manager
            .get_effective_server_headers("serv_secure_exchange", None)
            .await
            .expect("effective headers")
            .expect("headers");
        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer access-123")
        );
    }

    #[tokio::test]
    async fn delete_all_oauth_secrets_removes_client_and_token_secrets() {
        let (manager, store, _temp_dir) = setup_secure_manager().await;
        insert_server(&manager.pool, "serv_delete_oauth_secrets").await;

        manager
            .upsert_config(
                "serv_delete_oauth_secrets",
                OAuthConfigInput {
                    authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                    token_endpoint: "https://issuer.example.com/token".to_string(),
                    client_id: "client-1".to_string(),
                    client_secret: Some("client-secret-1".to_string()),
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save secure oauth config");
        manager
            .store_oauth_token_for_server(
                "serv_delete_oauth_secrets",
                OAuthTokenResponse {
                    access_token: "access-123".to_string(),
                    refresh_token: Some("refresh-123".to_string()),
                    token_type: Some("bearer".to_string()),
                    expires_in: Some(3600),
                    scope: Some("read write".to_string()),
                },
                None,
            )
            .await
            .expect("store secure oauth token");

        assert_eq!(store.list_secret_metadata().await.expect("list secrets").len(), 3);

        manager
            .delete_all_oauth_secrets("serv_delete_oauth_secrets")
            .await
            .expect("delete oauth secrets");

        assert!(store.list_secret_metadata().await.expect("list secrets").is_empty());
    }

    #[tokio::test]
    async fn fresh_token_storage_deletes_stale_refresh_token_secret_when_response_omits_refresh() {
        let (manager, store, _temp_dir) = setup_secure_manager().await;
        insert_server(&manager.pool, "serv_drop_refresh").await;

        manager
            .store_oauth_token_for_server(
                "serv_drop_refresh",
                OAuthTokenResponse {
                    access_token: "access-old".to_string(),
                    refresh_token: Some("refresh-old".to_string()),
                    token_type: Some("bearer".to_string()),
                    expires_in: Some(3600),
                    scope: Some("read write".to_string()),
                },
                None,
            )
            .await
            .expect("store initial token");
        store
            .get_secret_metadata("oauth/serv_drop_refresh/refresh-token")
            .await
            .expect("refresh secret should exist");

        manager
            .store_oauth_token_for_server(
                "serv_drop_refresh",
                OAuthTokenResponse {
                    access_token: "access-new".to_string(),
                    refresh_token: None,
                    token_type: Some("bearer".to_string()),
                    expires_in: Some(3600),
                    scope: Some("read write".to_string()),
                },
                None,
            )
            .await
            .expect("store replacement token without refresh");

        let stored = server::get_server_oauth_token(&manager.pool, "serv_drop_refresh")
            .await
            .expect("load stored token")
            .expect("stored token exists");
        assert!(stored.refresh_token.is_none());
        assert!(
            store
                .get_secret_metadata("oauth/serv_drop_refresh/refresh-token")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn active_secret_discovery_counts_oauth_token_secrets() {
        let (manager, _store, _temp_dir) = setup_secure_manager().await;
        insert_server(&manager.pool, "serv_oauth_usage").await;

        manager
            .store_oauth_token_for_server(
                "serv_oauth_usage",
                OAuthTokenResponse {
                    access_token: "access-123".to_string(),
                    refresh_token: Some("refresh-123".to_string()),
                    token_type: Some("bearer".to_string()),
                    expires_in: Some(3600),
                    scope: Some("read write".to_string()),
                },
                None,
            )
            .await
            .expect("store secure oauth token");

        let access_alias = oauth_secret_alias("serv_oauth_usage", OAuthSecretSlot::AccessToken);
        let refresh_alias = oauth_secret_alias("serv_oauth_usage", OAuthSecretSlot::RefreshToken);

        for alias in [access_alias, refresh_alias] {
            let usages = crate::core::secrets::discover_active_secret_usages_for_alias(&manager.pool, &alias)
                .await
                .expect("discover oauth usage");
            assert_eq!(usages.len(), 1);
            assert_eq!(usages[0].server_id, "serv_oauth_usage");
            assert!(matches!(usages[0].location, SecretUsageLocationInput::OAuthToken));
        }
    }

    #[tokio::test]
    async fn revoke_fails_closed_when_secure_token_secret_cleanup_is_unavailable() {
        let (manager, _store, _temp_dir) = setup_secure_manager().await;
        insert_server(&manager.pool, "serv_revoke_no_store").await;
        manager
            .store_oauth_token_for_server(
                "serv_revoke_no_store",
                OAuthTokenResponse {
                    access_token: "access-123".to_string(),
                    refresh_token: Some("refresh-123".to_string()),
                    token_type: Some("bearer".to_string()),
                    expires_in: Some(3600),
                    scope: Some("read write".to_string()),
                },
                None,
            )
            .await
            .expect("store secure oauth token");

        let manager_without_store = OAuthManager::new(manager.pool.clone());
        let error = manager_without_store
            .revoke("serv_revoke_no_store")
            .await
            .expect_err("revoke should fail without Secure Store cleanup");

        assert!(error.to_string().contains("Secure Store is unavailable"));
        assert!(
            server::get_server_oauth_token(&manager.pool, "serv_revoke_no_store")
                .await
                .expect("load oauth token")
                .is_some()
        );
    }

    #[tokio::test]
    async fn refresh_rotates_secure_access_token_without_plaintext_storage() {
        let (manager, store, _temp_dir) = setup_secure_manager().await;
        insert_server(&manager.pool, "serv_secure_refresh").await;
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=refresh-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-new",
                "token_type": "bearer",
                "expires_in": 3600
            })))
            .mount(&mock_server)
            .await;

        manager
            .upsert_config(
                "serv_secure_refresh",
                OAuthConfigInput {
                    authorization_endpoint: format!("{}/authorize", mock_server.uri()),
                    token_endpoint: format!("{}/token", mock_server.uri()),
                    client_id: "client-1".to_string(),
                    client_secret: None,
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save oauth config");
        let stored = manager
            .store_oauth_token_for_server(
                "serv_secure_refresh",
                OAuthTokenResponse {
                    access_token: "access-old".to_string(),
                    refresh_token: Some("refresh-123".to_string()),
                    token_type: Some("bearer".to_string()),
                    expires_in: Some(-60),
                    scope: Some("read write".to_string()),
                },
                None,
            )
            .await
            .expect("store secure token");

        assert!(stored.access_token.starts_with("[[secret:"));
        let refreshed = manager
            .refresh_access_token("serv_secure_refresh")
            .await
            .expect("refresh access token");

        assert_ne!(refreshed.access_token, "access-new");
        assert_eq!(
            mcpmate_secrets::resolve_placeholders(&refreshed.access_token, store.as_ref()).expect("resolve access"),
            "access-new"
        );
        assert_eq!(
            mcpmate_secrets::resolve_placeholders(
                refreshed.refresh_token.as_deref().expect("refresh ref"),
                store.as_ref()
            )
            .expect("resolve refresh"),
            "refresh-123"
        );
    }

    #[tokio::test]
    async fn refresh_error_redacts_oauth_token_endpoint_body() {
        let (manager, _store, _temp_dir) = setup_secure_manager().await;
        insert_server(&manager.pool, "serv_refresh_error_redaction").await;
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("refresh_token=refresh-123"))
            .and(body_string_contains("client_secret=client-secret-1"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_grant",
                "error_description": "refresh_token=refresh-123 client_secret=client-secret-1 access-new",
                "refresh_token": "refresh-123",
                "client_secret": "client-secret-1"
            })))
            .mount(&mock_server)
            .await;

        manager
            .upsert_config(
                "serv_refresh_error_redaction",
                OAuthConfigInput {
                    authorization_endpoint: format!("{}/authorize", mock_server.uri()),
                    token_endpoint: format!("{}/token?client_secret=client-secret-1", mock_server.uri()),
                    client_id: "client-1".to_string(),
                    client_secret: Some("client-secret-1".to_string()),
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save oauth config");
        manager
            .store_oauth_token_for_server(
                "serv_refresh_error_redaction",
                OAuthTokenResponse {
                    access_token: "access-old".to_string(),
                    refresh_token: Some("refresh-123".to_string()),
                    token_type: Some("bearer".to_string()),
                    expires_in: Some(-60),
                    scope: Some("read write".to_string()),
                },
                None,
            )
            .await
            .expect("store expired token");

        let error = manager
            .refresh_access_token("serv_refresh_error_redaction")
            .await
            .expect_err("refresh should fail");
        let message = error.to_string();

        assert!(message.contains("OAuth token endpoint returned error status"));
        assert!(message.contains("OAuth error: invalid_grant"));
        assert!(!message.contains("refresh-123"));
        assert!(!message.contains("client-secret-1"));
        assert!(!message.contains("access-new"));
        assert!(!message.contains("Response body"));
    }

    #[tokio::test]
    async fn status_refreshes_expired_secure_token_when_refresh_available() {
        let (manager, store, _temp_dir) = setup_secure_manager().await;
        insert_server(&manager.pool, "serv_status_refresh").await;
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=refresh-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-new",
                "token_type": "bearer",
                "expires_in": 3600
            })))
            .mount(&mock_server)
            .await;

        manager
            .upsert_config(
                "serv_status_refresh",
                OAuthConfigInput {
                    authorization_endpoint: format!("{}/authorize", mock_server.uri()),
                    token_endpoint: format!("{}/token", mock_server.uri()),
                    client_id: "client-1".to_string(),
                    client_secret: None,
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save oauth config");
        manager
            .store_oauth_token_for_server(
                "serv_status_refresh",
                OAuthTokenResponse {
                    access_token: "access-old".to_string(),
                    refresh_token: Some("refresh-123".to_string()),
                    token_type: Some("bearer".to_string()),
                    expires_in: Some(-60),
                    scope: Some("read write".to_string()),
                },
                None,
            )
            .await
            .expect("store expired token");

        let status = manager
            .get_status("serv_status_refresh")
            .await
            .expect("status refresh should not fail");

        assert!(matches!(status.state, OAuthConnectionState::Connected));
        let stored = server::get_server_oauth_token(&manager.pool, "serv_status_refresh")
            .await
            .expect("load refreshed token")
            .expect("refreshed token exists");
        assert_eq!(
            mcpmate_secrets::resolve_placeholders(&stored.access_token, store.as_ref()).expect("resolve access"),
            "access-new"
        );
    }

    #[tokio::test]
    async fn status_marks_unreadable_oauth_secret_as_reconnect_required() {
        let (manager, _store, temp_dir) = setup_secure_manager().await;
        insert_server(&manager.pool, "serv_unreadable_oauth_secret").await;

        manager
            .upsert_config(
                "serv_unreadable_oauth_secret",
                OAuthConfigInput {
                    authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                    token_endpoint: "https://issuer.example.com/token".to_string(),
                    client_id: "client-1".to_string(),
                    client_secret: None,
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save oauth config");
        manager
            .store_oauth_token_for_server(
                "serv_unreadable_oauth_secret",
                OAuthTokenResponse {
                    access_token: "access-old".to_string(),
                    refresh_token: Some("refresh-123".to_string()),
                    token_type: Some("bearer".to_string()),
                    expires_in: Some(-60),
                    scope: Some("read write".to_string()),
                },
                None,
            )
            .await
            .expect("store expired token");

        let corrupted_key = base64::engine::general_purpose::STANDARD.encode([0_u8; 48]);
        sqlx::query("UPDATE secure_store_secrets SET encrypted_key = ?2 WHERE alias = ?1")
            .bind(oauth_secret_alias(
                "serv_unreadable_oauth_secret",
                OAuthSecretSlot::RefreshToken,
            ))
            .bind(corrupted_key)
            .execute(&manager.pool)
            .await
            .expect("corrupt refresh token record");

        let reloaded_store = Arc::new(
            LocalSecretStore::initialize_with_development_root_key(
                manager.pool.clone(),
                temp_dir.path().join("secrets").join("local-root.key"),
            )
            .await
            .expect("reload secret store"),
        );
        let reloaded_manager = OAuthManager::new_optional_store(manager.pool.clone(), Some(reloaded_store));

        let status = reloaded_manager
            .get_status("serv_unreadable_oauth_secret")
            .await
            .expect("status should report unreadable secret");

        assert!(matches!(status.state, OAuthConnectionState::Expired));
        assert!(matches!(status.custody_state, OAuthCustodyState::Secure));
        assert!(status.requires_reconnect);
        assert_eq!(
            status.issue.as_ref().map(|issue| issue.code.as_str()),
            Some(OAUTH_ISSUE_SECRET_UNREADABLE)
        );
    }

    #[tokio::test]
    async fn effective_headers_refresh_expired_oauth_token() {
        let (manager, _store, _temp_dir) = setup_secure_manager().await;
        insert_server(&manager.pool, "serv_refresh").await;
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=refresh-123"))
            .and(body_string_contains("resource=https%3A%2F%2Fexample.com%2Fmcp"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-new",
                "token_type": "bearer",
                "expires_in": 3600
            })))
            .mount(&mock_server)
            .await;

        manager
            .upsert_config(
                "serv_refresh",
                OAuthConfigInput {
                    authorization_endpoint: format!("{}/authorize", mock_server.uri()),
                    token_endpoint: format!("{}/token", mock_server.uri()),
                    client_id: "client-1".to_string(),
                    client_secret: None,
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save oauth config");
        manager
            .store_oauth_token_for_server(
                "serv_refresh",
                OAuthTokenResponse {
                    access_token: "access-old".to_string(),
                    refresh_token: Some("refresh-123".to_string()),
                    token_type: Some("bearer".to_string()),
                    expires_in: Some(-60),
                    scope: Some("read write".to_string()),
                },
                None,
            )
            .await
            .expect("store expired token");

        let headers = manager
            .get_effective_server_headers("serv_refresh", None)
            .await
            .expect("refresh expired token")
            .expect("headers");

        assert_eq!(
            headers.get("authorization").map(String::as_str),
            Some("Bearer access-new")
        );
        let stored = server::get_server_oauth_token(&manager.pool, "serv_refresh")
            .await
            .expect("load refreshed token")
            .expect("refreshed token exists");
        assert!(stored.access_token.starts_with("[[secret:"));
        assert!(
            stored
                .refresh_token
                .as_deref()
                .is_some_and(|value| value.starts_with("[[secret:"))
        );
        assert!(matches!(
            manager
                .get_status("serv_refresh")
                .await
                .expect("status after refresh")
                .state,
            OAuthConnectionState::Connected
        ));
    }

    #[tokio::test]
    async fn refresh_keeps_existing_refresh_token_when_response_is_blank() {
        let (manager, store, _temp_dir) = setup_secure_manager().await;
        insert_server(&manager.pool, "serv_refresh_blank").await;
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=refresh-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-new",
                "refresh_token": "   ",
                "token_type": "bearer",
                "expires_in": 3600
            })))
            .mount(&mock_server)
            .await;

        manager
            .upsert_config(
                "serv_refresh_blank",
                OAuthConfigInput {
                    authorization_endpoint: format!("{}/authorize", mock_server.uri()),
                    token_endpoint: format!("{}/token", mock_server.uri()),
                    client_id: "client-1".to_string(),
                    client_secret: None,
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save oauth config");
        manager
            .store_oauth_token_for_server(
                "serv_refresh_blank",
                OAuthTokenResponse {
                    access_token: "access-old".to_string(),
                    refresh_token: Some("refresh-123".to_string()),
                    token_type: Some("bearer".to_string()),
                    expires_in: Some(-60),
                    scope: Some("read write".to_string()),
                },
                None,
            )
            .await
            .expect("store expired token");

        manager
            .refresh_access_token("serv_refresh_blank")
            .await
            .expect("refresh token");

        let stored = server::get_server_oauth_token(&manager.pool, "serv_refresh_blank")
            .await
            .expect("load stored token")
            .expect("stored token exists");
        assert_eq!(
            mcpmate_secrets::resolve_placeholders(&stored.access_token, store.as_ref()).expect("resolve access"),
            "access-new"
        );
        assert_eq!(
            mcpmate_secrets::resolve_placeholders(
                stored.refresh_token.as_deref().expect("refresh ref"),
                store.as_ref()
            )
            .expect("resolve refresh"),
            "refresh-123"
        );
    }

    #[tokio::test]
    async fn effective_headers_keep_manual_authorization_override() {
        let manager = setup_manager().await;
        insert_server(&manager.pool, "serv_manual_refresh").await;

        manager
            .upsert_config(
                "serv_manual_refresh",
                OAuthConfigInput {
                    authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                    token_endpoint: "http://not-loopback.example.com/token".to_string(),
                    client_id: "client-1".to_string(),
                    client_secret: None,
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save oauth config");
        server::upsert_server_oauth_token(
            &manager.pool,
            &ServerOAuthToken {
                id: None,
                server_id: "serv_manual_refresh".to_string(),
                access_token: "access-old".to_string(),
                refresh_token: Some("refresh-123".to_string()),
                token_type: "bearer".to_string(),
                expires_at: Some((Utc::now() - Duration::minutes(1)).to_rfc3339()),
                scope: Some("read write".to_string()),
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("store expired token");

        let manual = HashMap::from([("Authorization".to_string(), "Bearer manual-token".to_string())]);
        let headers = manager
            .get_effective_server_headers("serv_manual_refresh", Some(manual))
            .await
            .expect("manual header should not trigger refresh")
            .expect("headers");

        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer manual-token")
        );
    }

    #[tokio::test]
    async fn upsert_config_preserves_manual_authorization_header_until_oauth_connects() {
        let manager = setup_manager().await;
        insert_typed_http_server(
            &manager.pool,
            "serv_oauth_auth_mode",
            BTreeMap::from([
                (
                    "Authorization".to_string(),
                    ConfigValue::Literal {
                        value: "Bearer manual-token".to_string(),
                    },
                ),
                (
                    "X-Trace".to_string(),
                    ConfigValue::Literal {
                        value: "trace-1".to_string(),
                    },
                ),
            ]),
        )
        .await;

        let status = manager
            .upsert_config(
                "serv_oauth_auth_mode",
                OAuthConfigInput {
                    authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
                    token_endpoint: "https://issuer.example.com/token".to_string(),
                    client_id: "client-1".to_string(),
                    client_secret: None,
                    scopes: Some("read write".to_string()),
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save oauth config");

        let headers = server::load_validated_server_transport(&manager.pool, "serv_oauth_auth_mode")
            .await
            .expect("load typed transport")
            .runtime_headers();

        assert_eq!(
            headers.get("Authorization").map(String::as_str),
            Some("Bearer manual-token")
        );
        assert_eq!(headers.get("X-Trace").map(String::as_str), Some("trace-1"));
        assert!(server::has_manual_authorization_header(&Some(headers)));
        assert!(status.manual_authorization_override);
    }

    #[tokio::test]
    async fn status_ignores_legacy_authorization_when_typed_transport_has_none() {
        let manager = setup_manager().await;
        insert_server(&manager.pool, "serv_typed_auth_status").await;
        server::upsert_server_headers(
            &manager.pool,
            "serv_typed_auth_status",
            &HashMap::from([("Authorization".to_string(), "Bearer stale-token".to_string())]),
        )
        .await
        .expect("write divergent legacy header projection");

        let status = manager
            .get_status("serv_typed_auth_status")
            .await
            .expect("read OAuth status");

        assert!(!status.manual_authorization_override);
    }

    #[tokio::test]
    async fn exchange_code_rejects_invalid_state() {
        let manager = setup_manager().await;
        let error = manager
            .exchange_code("missing-state", "code")
            .await
            .expect_err("invalid state should fail");
        assert!(error.to_string().contains("Invalid or expired OAuth state"));
    }

    #[test]
    fn oauth_resource_strips_url_userinfo() {
        let transport = ValidatedTransport::StreamableHttp {
            endpoint: Url::parse("https://user:pass@example.com/mcp?secret=1#fragment")
                .expect("valid transport endpoint"),
            headers: BTreeMap::new(),
        };

        let resource = oauth_resource_from_transport(&transport).expect("sanitize oauth resource");

        assert_eq!(resource, "https://example.com/mcp");
    }

    #[test]
    fn oauth_resource_trims_trailing_slash() {
        let transport = ValidatedTransport::Sse {
            endpoint: Url::parse("https://example.com").expect("valid transport endpoint"),
            headers: BTreeMap::new(),
        };

        let resource = oauth_resource_from_transport(&transport).expect("normalize oauth resource");

        assert_eq!(resource, "https://example.com");
    }

    #[tokio::test]
    async fn exchange_code_rejects_non_loopback_http_endpoint() {
        let manager = setup_manager().await;
        insert_server(&manager.pool, "serv_http").await;

        manager
            .upsert_config(
                "serv_http",
                OAuthConfigInput {
                    authorization_endpoint: "https://auth.example.com/authorize".to_string(),
                    token_endpoint: "http://evil.example.com/token".to_string(),
                    client_id: "client-1".to_string(),
                    client_secret: None,
                    scopes: None,
                    redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
                },
            )
            .await
            .expect("save oauth config");

        let initiate = manager.initiate("serv_http").await.expect("initiate oauth");
        let error = manager
            .exchange_code(&initiate.state, "code-123")
            .await
            .expect_err("non-loopback HTTP endpoint should be rejected");
        assert!(
            error.to_string().contains("must use HTTPS"),
            "unexpected error: {error}"
        );
    }
}
