use anyhow::Result;
use sqlx::{Pool, Sqlite};

pub(crate) const CLIENT_RUNTIME_SETTINGS_TABLE: &str = "client_runtime_settings";
pub(crate) const DEFAULT_CONFIG_MODE: &str = "unify";

/// Verify client storage after the config migration stream has run.
pub async fn initialize_client_table(pool: &Pool<Sqlite>) -> Result<()> {
    mcpmate_migrations::verify_config_database(pool).await?;
    sqlx::query("SELECT 1 FROM client_runtime_settings LIMIT 1")
        .execute(pool)
        .await
        .map_err(|error| anyhow::anyhow!("client storage was not migrated: {error}"))?;
    Ok(())
}

pub async fn resolve_default_client_config_mode(pool: &Pool<Sqlite>) -> Result<String> {
    crate::system::settings::get_default_config_mode(pool)
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

pub fn effective_client_config_mode<'a>(
    explicit_mode: Option<&'a str>,
    default_mode: &'a str,
) -> &'a str {
    explicit_mode
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
        .unwrap_or(default_mode)
}

pub fn is_managed_client_config_mode(mode: &str) -> bool {
    matches!(mode, "unify" | "hosted")
}

#[cfg(test)]
pub async fn set_default_client_config_mode(
    pool: &Pool<Sqlite>,
    mode: &str,
) -> Result<()> {
    crate::system::settings::set_default_config_mode(pool, mode)
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

/// Ensures the on-disk system settings store exists. It has no durable SQLite schema ownership.
pub async fn initialize_system_settings(pool: &Pool<Sqlite>) -> Result<()> {
    crate::system::settings::initialize_settings_file(pool)
        .await
        .map_err(|error| anyhow::anyhow!(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn pool() -> Pool<Sqlite> {
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn initializes_current_client_schema_through_migrations() {
        let pool = pool().await;
        crate::helpers::prepare_config_database(&pool).await;
        initialize_client_table(&pool).await.unwrap();
        let mode: String =
            sqlx::query_scalar("SELECT value FROM client_runtime_settings WHERE key = 'default_config_mode'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(mode, "unify");
    }

    #[tokio::test]
    async fn keeps_mode_helpers_stable() {
        assert_eq!(effective_client_config_mode(Some(" hosted "), "unify"), "hosted");
        assert_eq!(effective_client_config_mode(Some(" "), "unify"), "unify");
        assert!(is_managed_client_config_mode("unify"));
        assert!(!is_managed_client_config_mode("transparent"));
    }
}
