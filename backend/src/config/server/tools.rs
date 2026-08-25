// Server tools management module
// Manages global tool name mappings for servers

use anyhow::{Context, Result};
use rmcp::model::Tool;
use sqlx::{Pool, Sqlite, Transaction};

use crate::{
    config::models::ServerTool,
    core::capability::index::CachedToolInfo,
    core::capability::naming::{
        NamingKind, begin_naming_transaction, reconcile_external_identifier_additions, reconcile_external_identifiers,
    },
    generate_id,
};

/// Add or update a server tool mapping
#[derive(Debug, Clone)]
pub struct ServerToolUpsertResult {
    pub tool_id: String,
    pub unique_name: String,
}

async fn upsert_server_tool_row(
    tx: &mut Transaction<'_, Sqlite>,
    server_id: &str,
    server_name: &str,
    tool_name: &str,
    unique_name: &str,
    description: Option<&str>,
) -> Result<ServerToolUpsertResult> {
    let existing_id =
        sqlx::query_scalar::<_, String>("SELECT id FROM server_tools WHERE server_id = ? AND tool_name = ?")
            .bind(server_id)
            .bind(tool_name)
            .fetch_optional(&mut **tx)
            .await
            .context("Failed to check existing server tool")?;

    let tool_id = if let Some(id) = existing_id {
        sqlx::query(
            r#"
            UPDATE server_tools
            SET server_name = ?, unique_name = ?, description = ?, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
            "#,
        )
        .bind(server_name)
        .bind(unique_name)
        .bind(description)
        .bind(&id)
        .execute(&mut **tx)
        .await
        .context("Failed to update server tool")?;
        id
    } else {
        let id = generate_id!("stool");
        sqlx::query(
            r#"
            INSERT INTO server_tools (id, server_id, server_name, tool_name, unique_name, description)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&id)
        .bind(server_id)
        .bind(server_name)
        .bind(tool_name)
        .bind(unique_name)
        .bind(description)
        .execute(&mut **tx)
        .await
        .context("Failed to insert server tool")?;
        id
    };

    Ok(ServerToolUpsertResult {
        tool_id,
        unique_name: unique_name.to_string(),
    })
}

async fn upsert_server_tools_batch(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    tools: &[(String, Option<String>)],
) -> Result<(Vec<ServerToolUpsertResult>, bool)> {
    let mut tx = begin_naming_transaction(pool)
        .await
        .context("Failed to begin server tool update")?;
    let result = upsert_server_tools_batch_in_transaction(&mut tx, server_id, server_name, tools).await?;
    tx.commit().await.context("Failed to commit server tool update")?;
    Ok(result)
}

pub(crate) async fn upsert_server_tools_batch_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    server_id: &str,
    server_name: &str,
    tools: &[(String, Option<String>)],
) -> Result<(Vec<ServerToolUpsertResult>, bool)> {
    let inventory = tools.iter().map(|(name, _)| name.clone()).collect::<Vec<_>>();
    let reconciliation = reconcile_external_identifiers(tx, NamingKind::Tool, server_id, server_name, &inventory)
        .await
        .context("Failed to reconcile external tool names")?;

    let mut results = Vec::with_capacity(tools.len());
    for (tool_name, description) in tools {
        let unique_name = reconciliation.identifier_for(tool_name)?;
        results.push(
            upsert_server_tool_row(
                tx,
                server_id,
                server_name,
                tool_name,
                unique_name,
                description.as_deref(),
            )
            .await?,
        );
    }
    let catalog_changed = reconciliation.catalog_changed();
    Ok((results, catalog_changed))
}

pub async fn upsert_server_tool(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    tool_name: &str,
    description: Option<&str>,
) -> Result<ServerToolUpsertResult> {
    tracing::debug!(
        "Upserting server tool mapping: server_id={}, tool_name={}",
        server_id,
        tool_name
    );

    let mut tx = begin_naming_transaction(pool)
        .await
        .context("Failed to begin server tool update")?;
    let reconciliation = reconcile_external_identifier_additions(
        &mut tx,
        NamingKind::Tool,
        server_id,
        server_name,
        &[tool_name.to_string()],
    )
    .await
    .context("Failed to extend external tool inventory")?;
    let result = upsert_server_tool_row(
        &mut tx,
        server_id,
        server_name,
        tool_name,
        reconciliation.identifier_for(tool_name)?,
        description,
    )
    .await?;
    let catalog_changed = reconciliation.catalog_changed();
    tx.commit().await.context("Failed to commit server tool update")?;
    if catalog_changed {
        crate::core::events::EventBus::global().publish(crate::core::events::Event::CapabilityCatalogChanged {
            server_id: server_id.to_string(),
            server_name: server_name.to_string(),
        });
    }
    Ok(result)
}

/// Get all server tools for a specific server
pub async fn get_server_tools(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<Vec<ServerTool>> {
    let tools = sqlx::query_as::<_, ServerTool>("SELECT * FROM server_tools WHERE server_id = ? ORDER BY tool_name")
        .bind(server_id)
        .fetch_all(pool)
        .await
        .context("Failed to fetch server tools")?;

    Ok(tools)
}

/// Get a server tool by server_id and tool_name
pub async fn get_server_tool(
    pool: &Pool<Sqlite>,
    server_id: &str,
    tool_name: &str,
) -> Result<Option<ServerTool>> {
    let tool = sqlx::query_as::<_, ServerTool>("SELECT * FROM server_tools WHERE server_id = ? AND tool_name = ?")
        .bind(server_id)
        .bind(tool_name)
        .fetch_optional(pool)
        .await
        .context("Failed to fetch server tool")?;

    Ok(tool)
}

/// Remove all server tools for a specific server
pub async fn remove_server_tools(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<u64> {
    let result = sqlx::query("DELETE FROM server_tools WHERE server_id = ?")
        .bind(server_id)
        .execute(pool)
        .await
        .context("Failed to remove server tools")?;

    tracing::debug!(
        "Removed {} server tools for server_id={}",
        result.rows_affected(),
        server_id
    );
    Ok(result.rows_affected())
}

/// Get all unique tool names (for API responses)
pub async fn get_all_unique_tool_names(pool: &Pool<Sqlite>) -> Result<Vec<String>> {
    let names = sqlx::query_scalar::<_, String>("SELECT unique_name FROM server_tools ORDER BY unique_name")
        .fetch_all(pool)
        .await
        .context("Failed to fetch unique tool names")?;

    Ok(names)
}

/// Batch upsert server tools (for server synchronization)
pub async fn batch_upsert_server_tools(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    tools: &[(String, Option<String>)], // (tool_name, description)
) -> Result<Vec<String>> {
    let (results, catalog_changed) = upsert_server_tools_batch(pool, server_id, server_name, tools).await?;
    let tool_ids = results.into_iter().map(|result| result.tool_id).collect::<Vec<_>>();

    tracing::debug!(
        "Batch upserted {} server tools for server_id={}",
        tool_ids.len(),
        server_id
    );
    if catalog_changed {
        crate::core::events::EventBus::global().publish(crate::core::events::Event::CapabilityCatalogChanged {
            server_id: server_id.to_string(),
            server_name: server_name.to_string(),
        });
    }

    Ok(tool_ids)
}

/// Assign collision-free unique names to the provided tools, updating the server_tools mapping as needed.
pub async fn assign_unique_names_to_tools(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    tools: &mut [Tool],
) -> Result<()> {
    let inventory = tools
        .iter()
        .map(|tool| {
            (
                tool.name.to_string(),
                tool.description.as_ref().map(|description| description.to_string()),
            )
        })
        .collect::<Vec<_>>();
    let (results, catalog_changed) = upsert_server_tools_batch(pool, server_id, server_name, &inventory).await?;
    for (tool, result) in tools.iter_mut().zip(results) {
        tool.name = std::borrow::Cow::Owned(result.unique_name);
    }
    if catalog_changed {
        crate::core::events::EventBus::global().publish(crate::core::events::Event::CapabilityCatalogChanged {
            server_id: server_id.to_string(),
            server_name: server_name.to_string(),
        });
    }

    Ok(())
}

/// Assign collision-free unique names to cached tool snapshots and update their stored metadata.
pub async fn assign_unique_names_to_cached_tools(
    pool: &Pool<Sqlite>,
    server_id: &str,
    server_name: &str,
    tools: &mut [CachedToolInfo],
) -> Result<bool> {
    let mut tx = begin_naming_transaction(pool)
        .await
        .context("Failed to begin cached tool naming update")?;
    let catalog_changed =
        assign_unique_names_to_cached_tools_in_transaction(&mut tx, server_id, server_name, tools).await?;
    tx.commit()
        .await
        .context("Failed to commit cached tool naming update")?;
    Ok(catalog_changed)
}

pub(crate) async fn assign_unique_names_to_cached_tools_in_transaction(
    tx: &mut Transaction<'_, Sqlite>,
    server_id: &str,
    server_name: &str,
    tools: &mut [CachedToolInfo],
) -> Result<bool> {
    let inventory = tools
        .iter()
        .map(|tool| (tool.name.clone(), tool.description.clone()))
        .collect::<Vec<_>>();
    let (results, catalog_changed) =
        upsert_server_tools_batch_in_transaction(tx, server_id, server_name, &inventory).await?;
    for (tool, ((upstream_name, _), result)) in tools.iter_mut().zip(inventory.into_iter().zip(results)) {
        tool.name = upstream_name;
        tool.unique_name = Some(result.unique_name);
    }

    Ok(catalog_changed)
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;

    async fn test_pool() -> Pool<Sqlite> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .expect("initialize profile tables");
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("initialize client table");
        pool
    }

    #[tokio::test]
    async fn cached_tool_keeps_exact_upstream_name_and_stores_external_identifier() {
        let pool = test_pool().await;
        sqlx::query("INSERT INTO server_config (id, name, server_type) VALUES ('server-a', 'searxng', 'stdio')")
            .execute(&pool)
            .await
            .expect("insert server");
        let mut tools = vec![CachedToolInfo {
            name: "searxng_web_search".to_string(),
            description: Some("Search the web".to_string()),
            input_schema_json: r#"{"type":"object"}"#.to_string(),
            output_schema_json: None,
            unique_name: None,
            icons: None,
            enabled: true,
            cached_at: Utc::now(),
        }];

        assign_unique_names_to_cached_tools(&pool, "server-a", "searxng", &mut tools)
            .await
            .expect("assign cached tool identity");

        assert_eq!(tools[0].name, "searxng_web_search");
        assert_eq!(tools[0].unique_name.as_deref(), Some("searxng_web_search"));
        let mapping = get_server_tool(&pool, "server-a", "searxng_web_search")
            .await
            .expect("load mapping")
            .expect("mapping exists");
        assert_eq!(mapping.tool_name, "searxng_web_search");
        assert_eq!(mapping.unique_name, "searxng_web_search");
    }
}
