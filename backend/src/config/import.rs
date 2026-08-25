// Configuration import for MCPMate
// Contains functions for importing configuration from JSON files to database

use crate::common::types::{ServerSource, ServerSourceType};
use crate::{config::database::Database, core::models::Config};
use anyhow::{Context, Result};
use sqlx::{Pool, Sqlite};
use std::collections::HashMap;
use std::{fs::File, path::Path, sync::Arc};

pub(crate) async fn has_server_configs(pool: &Pool<Sqlite>) -> Result<bool> {
    sqlx::query_scalar::<_, i64>(&format!(
        "SELECT COUNT(*) FROM {}",
        crate::common::constants::database::tables::SERVER_CONFIG
    ))
    .fetch_one(pool)
    .await
    .map(|count| count > 0)
    .map_err(|error| anyhow::anyhow!("Failed to check if server_config table has data: {}", error))
}

async fn import_config(
    pool: &Pool<Sqlite>,
    mcp_config: Config,
) -> Result<()> {
    let items: HashMap<String, crate::api::models::server::ServersImportConfig> = mcp_config
        .mcp_servers
        .into_iter()
        .map(|(name, server_config)| {
            (
                name,
                crate::api::models::server::ServersImportConfig {
                    kind: server_config.kind.as_str().to_string(),
                    command: server_config.command,
                    args: server_config.args,
                    url: server_config.url,
                    env: server_config.env,
                    headers: None,
                    source: Some(ServerSource::new(ServerSourceType::Local, None)),
                    meta: None,
                },
            )
        })
        .collect();

    let database = Arc::new(Database {
        pool: pool.clone(),
        path: std::path::PathBuf::new(),
        capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
    });
    let _ = crate::config::server::import_batch(
        database,
        None,
        items,
        crate::config::server::ImportOptions::dashboard_import(false),
    )
    .await?;

    tracing::info!("Configuration import completed successfully");
    Ok(())
}

/// Import configuration from JSON files to database
pub async fn import_from_mcp_config(
    pool: &Pool<Sqlite>,
    mcp_config_path: &Path,
) -> Result<()> {
    tracing::info!("Importing configuration from JSON files to database");

    // If database already has server configurations, skip import
    if has_server_configs(pool).await? {
        tracing::info!("Database already has server configurations, skipping import");
        return Ok(());
    }

    // Check if configuration files exist
    if !mcp_config_path.exists() {
        tracing::warn!(
            "MCP configuration file not found at {}, skipping import",
            mcp_config_path.display()
        );
        return Ok(());
    }

    // Load MCP server configuration
    let mcp_config = match load_mcp_config_from_file(mcp_config_path) {
        Ok(config) => {
            tracing::info!(
                "Successfully loaded MCP configuration from {}",
                mcp_config_path.display()
            );
            config
        }
        Err(e) => {
            tracing::error!("Failed to load MCP configuration: {}", e);
            return Err(anyhow::anyhow!("Failed to load MCP configuration: {}", e));
        }
    };

    import_config(pool, mcp_config).await
}

pub async fn import_from_mcp_config_content(
    pool: &Pool<Sqlite>,
    content: &str,
) -> Result<()> {
    tracing::info!("Importing configuration from in-memory content to database");

    if has_server_configs(pool).await? {
        tracing::info!("Database already has server configurations, skipping import");
        return Ok(());
    }

    let mcp_config = load_mcp_config_from_str(content).context("Failed to parse in-memory MCP config")?;
    import_config(pool, mcp_config).await
}

/// Load MCP configuration from a file
fn load_mcp_config_from_file<P: AsRef<Path>>(path: P) -> Result<Config> {
    use std::io::Read;

    // Read the file content as a string
    let mut content = String::new();
    let mut file = File::open(path.as_ref())
        .with_context(|| format!("Failed to open config file: {}", path.as_ref().display()))?;
    file.read_to_string(&mut content)
        .with_context(|| format!("Failed to read config file content: {}", path.as_ref().display()))?;

    load_mcp_config_from_str(&content)
}

fn load_mcp_config_from_str(content: &str) -> Result<Config> {
    match serde_json::from_str::<Config>(content) {
        Ok(config) => {
            tracing::debug!("Successfully parsed config file");
            Ok(config)
        }
        Err(e) => {
            tracing::error!("Failed to parse config file: {}", e);

            // Try to parse as Value to get more information
            match serde_json::from_str::<serde_json::Value>(content) {
                Ok(value) => {
                    tracing::debug!("File is valid JSON, but doesn't match Config struct: {:?}", value);
                }
                Err(e) => {
                    tracing::error!("File is not valid JSON: {}", e);
                }
            }

            Err(anyhow::anyhow!("Failed to parse config file: {}", e))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::initialization::run_initialization;
    use sqlx::sqlite::SqlitePoolOptions;
    use tempfile::TempDir;

    const TEST_MCP_CONFIG: &str = r#"{
        "mcpServers": {
            "context7": {
                "command": "npx",
                "args": ["-y", "@upstash/context7-mcp@latest"],
                "type": "stdio"
            },
            "gitmcp": {
                "url": "https://gitmcp.io/modelcontextprotocol/rust-sdk",
                "type": "streamable_http"
            }
        }
    }"#;

    async fn create_test_pool() -> (TempDir, Pool<Sqlite>) {
        let temp_dir = TempDir::new().expect("temp dir");
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");

        sqlx::query("PRAGMA foreign_keys = ON")
            .execute(&pool)
            .await
            .expect("enable foreign keys");
        crate::helpers::prepare_config_database(&pool).await;
        run_initialization(&pool).await.expect("initialize schema");

        (temp_dir, pool)
    }

    #[tokio::test]
    async fn import_from_mcp_config_content_imports_servers_without_file_path() {
        let (_temp_dir, pool) = create_test_pool().await;
        let config = load_mcp_config_from_str(TEST_MCP_CONFIG).expect("parse config");

        import_config(&pool, config).await.expect("import config");

        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {}",
            crate::common::constants::database::tables::SERVER_CONFIG
        ))
        .fetch_one(&pool)
        .await
        .expect("count servers");
        assert_eq!(count, 2);
    }
}
