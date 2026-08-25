use anyhow::Result;
use sqlx::{Pool, Sqlite};
use std::collections::HashMap;

use crate::config::models::{ServerOAuthConfig, ServerOAuthToken};
use crate::generate_id;

use super::headers::has_non_empty_authorization_header;

pub async fn upsert_server_oauth_config(
    pool: &Pool<Sqlite>,
    config: &ServerOAuthConfig,
) -> Result<String> {
    let id = config.id.clone().unwrap_or_else(|| generate_id!("socf"));
    sqlx::query(
        r#"
        INSERT INTO server_oauth_config (
            id, server_id, authorization_endpoint, token_endpoint, client_id, client_secret, scopes, redirect_uri
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(server_id) DO UPDATE SET
            authorization_endpoint = excluded.authorization_endpoint,
            token_endpoint = excluded.token_endpoint,
            client_id = excluded.client_id,
            client_secret = excluded.client_secret,
            scopes = excluded.scopes,
            redirect_uri = excluded.redirect_uri,
            updated_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(&id)
    .bind(&config.server_id)
    .bind(&config.authorization_endpoint)
    .bind(&config.token_endpoint)
    .bind(&config.client_id)
    .bind(&config.client_secret)
    .bind(&config.scopes)
    .bind(&config.redirect_uri)
    .execute(pool)
    .await?;

    Ok(id)
}

pub async fn get_server_oauth_config(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<Option<ServerOAuthConfig>> {
    sqlx::query_as::<_, ServerOAuthConfig>(
        r#"
        SELECT id, server_id, authorization_endpoint, token_endpoint, client_id, client_secret, scopes, redirect_uri, created_at, updated_at
        FROM server_oauth_config
        WHERE server_id = ?
        "#,
    )
    .bind(server_id)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

pub async fn delete_server_oauth_config(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM server_oauth_config WHERE server_id = ?")
        .bind(server_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn upsert_server_oauth_token(
    pool: &Pool<Sqlite>,
    token: &ServerOAuthToken,
) -> Result<String> {
    let id = token.id.clone().unwrap_or_else(|| generate_id!("sotk"));
    sqlx::query(
        r#"
        INSERT INTO server_oauth_tokens (
            id, server_id, access_token, refresh_token, token_type, expires_at, scope
        )
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(server_id) DO UPDATE SET
            access_token = excluded.access_token,
            refresh_token = excluded.refresh_token,
            token_type = excluded.token_type,
            expires_at = excluded.expires_at,
            scope = excluded.scope,
            updated_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(&id)
    .bind(&token.server_id)
    .bind(&token.access_token)
    .bind(&token.refresh_token)
    .bind(&token.token_type)
    .bind(&token.expires_at)
    .bind(&token.scope)
    .execute(pool)
    .await?;

    Ok(id)
}

pub async fn get_server_oauth_token(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<Option<ServerOAuthToken>> {
    sqlx::query_as::<_, ServerOAuthToken>(
        r#"
        SELECT id, server_id, access_token, refresh_token, token_type, expires_at, scope, created_at, updated_at
        FROM server_oauth_tokens
        WHERE server_id = ?
        "#,
    )
    .bind(server_id)
    .fetch_optional(pool)
    .await
    .map_err(Into::into)
}

pub async fn delete_server_oauth_token(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<()> {
    sqlx::query("DELETE FROM server_oauth_tokens WHERE server_id = ?")
        .bind(server_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_all_oauth_configs(pool: &Pool<Sqlite>) -> Result<Vec<ServerOAuthConfig>> {
    sqlx::query_as::<_, ServerOAuthConfig>(
        "SELECT id, server_id, authorization_endpoint, token_endpoint, client_id, client_secret, scopes, redirect_uri, created_at, updated_at FROM server_oauth_config",
    )
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

pub async fn get_all_oauth_tokens(pool: &Pool<Sqlite>) -> Result<Vec<ServerOAuthToken>> {
    sqlx::query_as::<_, ServerOAuthToken>(
        "SELECT id, server_id, access_token, refresh_token, token_type, expires_at, scope, created_at, updated_at FROM server_oauth_tokens",
    )
    .fetch_all(pool)
    .await
    .map_err(Into::into)
}

pub fn has_manual_authorization_header(headers: &Option<HashMap<String, String>>) -> bool {
    headers
        .as_ref()
        .map(has_non_empty_authorization_header)
        .unwrap_or(false)
}

pub async fn get_effective_server_headers(
    pool: &Pool<Sqlite>,
    server_id: &str,
    manual_headers: Option<HashMap<String, String>>,
) -> Result<Option<HashMap<String, String>>> {
    if has_manual_authorization_header(&manual_headers) {
        return Ok(manual_headers.filter(|headers| !headers.is_empty()));
    }

    let effective = manual_headers.unwrap_or_default();
    if get_server_oauth_token(pool, server_id).await?.is_some() {
        anyhow::bail!(
            "OAuth secure token resolution requires OAuthManager with a Secure Store resolver for server '{}'",
            server_id
        );
    }

    if effective.is_empty() {
        Ok(None)
    } else {
        Ok(Some(effective))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        common::{server::ServerType, status::EnabledStatus},
        config::{
            models::Server,
            server::{crud::upsert_server, init::initialize_server_tables},
        },
    };
    use chrono::Utc;

    async fn setup_pool() -> sqlx::SqlitePool {
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
        pool
    }

    async fn insert_server(
        pool: &sqlx::SqlitePool,
        id: &str,
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
        upsert_server(pool, &server).await.expect("insert server");
    }

    #[tokio::test]
    async fn oauth_token_store_roundtrip() {
        let pool = setup_pool().await;
        insert_server(&pool, "serv_oauth_roundtrip").await;

        let token = ServerOAuthToken {
            id: None,
            server_id: "serv_oauth_roundtrip".to_string(),
            access_token: "access-1".to_string(),
            refresh_token: Some("refresh-1".to_string()),
            token_type: "bearer".to_string(),
            expires_at: Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
            scope: Some("read write".to_string()),
            created_at: None,
            updated_at: None,
        };
        upsert_server_oauth_token(&pool, &token)
            .await
            .expect("insert oauth token");

        let loaded = get_server_oauth_token(&pool, "serv_oauth_roundtrip")
            .await
            .expect("load oauth token")
            .expect("oauth token exists");
        assert_eq!(loaded.access_token, "access-1");

        let updated = ServerOAuthToken {
            access_token: "access-2".to_string(),
            ..token.clone()
        };
        upsert_server_oauth_token(&pool, &updated)
            .await
            .expect("update oauth token");

        let loaded_updated = get_server_oauth_token(&pool, "serv_oauth_roundtrip")
            .await
            .expect("reload oauth token")
            .expect("oauth token exists after update");
        assert_eq!(loaded_updated.access_token, "access-2");

        delete_server_oauth_token(&pool, "serv_oauth_roundtrip")
            .await
            .expect("delete oauth token");
        assert!(
            get_server_oauth_token(&pool, "serv_oauth_roundtrip")
                .await
                .expect("load deleted token")
                .is_none()
        );
    }

    #[tokio::test]
    async fn oauth_config_store_roundtrip() {
        let pool = setup_pool().await;
        insert_server(&pool, "serv_oauth_config").await;

        let config = ServerOAuthConfig {
            id: None,
            server_id: "serv_oauth_config".to_string(),
            authorization_endpoint: "https://issuer.example.com/authorize".to_string(),
            token_endpoint: "https://issuer.example.com/token".to_string(),
            client_id: "client-1".to_string(),
            client_secret: Some("secret-1".to_string()),
            scopes: Some("read write".to_string()),
            redirect_uri: "http://localhost:5173/oauth/callback".to_string(),
            created_at: None,
            updated_at: None,
        };
        upsert_server_oauth_config(&pool, &config)
            .await
            .expect("insert oauth config");

        let loaded = get_server_oauth_config(&pool, "serv_oauth_config")
            .await
            .expect("load oauth config")
            .expect("oauth config exists");
        assert_eq!(loaded.client_id, "client-1");

        delete_server_oauth_config(&pool, "serv_oauth_config")
            .await
            .expect("delete oauth config");
        assert!(
            get_server_oauth_config(&pool, "serv_oauth_config")
                .await
                .expect("load deleted oauth config")
                .is_none()
        );
    }

    #[tokio::test]
    async fn manual_authorization_header_takes_precedence() {
        let pool = setup_pool().await;
        insert_server(&pool, "serv_manual_auth_wins").await;

        upsert_server_oauth_token(
            &pool,
            &ServerOAuthToken {
                id: None,
                server_id: "serv_manual_auth_wins".to_string(),
                access_token: "oauth-token".to_string(),
                refresh_token: None,
                token_type: "bearer".to_string(),
                expires_at: Some((Utc::now() + chrono::Duration::hours(1)).to_rfc3339()),
                scope: None,
                created_at: None,
                updated_at: None,
            },
        )
        .await
        .expect("insert oauth token");

        let manual = HashMap::from([("Authorization".to_string(), "Bearer manual-token".to_string())]);
        let effective = get_effective_server_headers(&pool, "serv_manual_auth_wins", Some(manual))
            .await
            .expect("effective headers")
            .expect("headers exist");
        assert_eq!(
            effective.get("Authorization").map(String::as_str),
            Some("Bearer manual-token")
        );
    }
}
