use anyhow::Result;
use sqlx::{Pool, Sqlite};

use crate::common::constants::database::tables;

/// Verify server storage and perform recurring startup cleanup.
/// Durable schema is owned by `mcpmate-migrations`.
pub async fn initialize_server_tables(pool: &Pool<Sqlite>) -> Result<()> {
    mcpmate_migrations::verify_config_database(pool).await?;
    verify_server_tables(pool).await?;
    cleanup_pending_import_servers(pool).await
}

async fn cleanup_pending_import_servers(pool: &Pool<Sqlite>) -> Result<()> {
    let result = sqlx::query("DELETE FROM server_config WHERE pending_import = 1")
        .execute(pool)
        .await?;
    let removed = result.rows_affected();
    if removed > 0 {
        tracing::info!(removed, "Removed stale pending_import server records during startup");
    }
    Ok(())
}

async fn verify_server_tables(pool: &Pool<Sqlite>) -> Result<()> {
    for table in [
        tables::SERVER_CONFIG,
        tables::SERVER_ARGS,
        tables::SERVER_ENV,
        tables::SERVER_HEADERS,
        tables::SERVER_META,
        tables::SERVER_OAUTH_CONFIG,
        tables::SERVER_OAUTH_TOKENS,
    ] {
        sqlx::query(&format!(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='{table}'"
        ))
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| anyhow::anyhow!("{table} table not found after migration"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        common::{server::ServerType, status::EnabledStatus},
        config::{models::Server, server::crud::upsert_server},
    };

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
        pool
    }

    fn build_server(
        id: &str,
        name: &str,
        pending_import: bool,
    ) -> Server {
        Server {
            id: Some(id.to_string()),
            name: name.to_string(),
            server_type: ServerType::StreamableHttp,
            command: None,
            url: Some(format!("https://example.com/{name}")),
            source: None,
            enabled: EnabledStatus::Enabled,
            unify_direct_exposure_eligible: false,
            pending_import,
            created_at: None,
            updated_at: None,
        }
    }

    #[tokio::test]
    async fn initialize_server_tables_removes_pending_import_records() {
        let pool = setup_pool().await;
        crate::helpers::prepare_config_database(&pool).await;
        initialize_server_tables(&pool).await.expect("initialize tables");
        upsert_server(&pool, &build_server("serv_visible", "visible-server", false))
            .await
            .unwrap();
        upsert_server(&pool, &build_server("serv_pending", "pending-server", true))
            .await
            .unwrap();
        initialize_server_tables(&pool).await.expect("reinitialize tables");
        let remaining_names = sqlx::query_scalar::<_, String>("SELECT name FROM server_config ORDER BY name ASC")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(remaining_names, vec!["visible-server".to_string()]);
    }
}
