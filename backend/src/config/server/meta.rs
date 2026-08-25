// Server metadata management
// Contains operations for managing server metadata (description, author, etc.)

use anyhow::{Context, Result};
use sqlx::{Pool, Sqlite};

use crate::config::{models::ServerMeta, operations::utils::get_server_name};
use crate::generate_id;

/// Get server metadata from the database
pub async fn get_server_meta(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<Option<ServerMeta>> {
    tracing::debug!("Executing SQL query to get metadata for server ID {}", server_id);

    let meta = sqlx::query_as::<_, ServerMeta>(
        r#"
        SELECT * FROM server_meta
        WHERE server_id = ?
        "#,
    )
    .bind(server_id)
    .fetch_optional(pool)
    .await
    .context("Failed to fetch server metadata")?;

    if meta.is_some() {
        tracing::debug!("Found metadata for server ID {}", server_id);
    } else {
        tracing::debug!("No metadata found for server ID {}", server_id);
    }

    Ok(meta)
}

/// Create or update server metadata in the database
pub async fn upsert_server_meta(
    pool: &Pool<Sqlite>,
    meta: &ServerMeta,
) -> Result<String> {
    tracing::debug!("Upserting metadata for server ID {}", meta.server_id);

    // Generate an ID for the metadata if it doesn't have one
    let meta_id = if let Some(id) = &meta.id {
        id.clone()
    } else {
        generate_id!("smet")
    };

    // Get the server name
    let server_name = get_server_name(pool, &meta.server_id).await?;

    let result = sqlx::query(
        r#"
        INSERT INTO server_meta (
            id,
            server_id,
            server_name,
            description,
            website,
            repository,
            registry_version,
            registry_meta_json,
            extras_json,
            icons_json,
            upstream_name,
            upstream_title,
            server_version,
            protocol_version,
            author,
            category,
            recommended_scenario,
            rating
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(server_id) DO UPDATE SET
            server_name = excluded.server_name,
            description = excluded.description,
            website = excluded.website,
            repository = excluded.repository,
            registry_version = excluded.registry_version,
            registry_meta_json = excluded.registry_meta_json,
            extras_json = excluded.extras_json,
            icons_json = COALESCE(excluded.icons_json, server_meta.icons_json),
            upstream_name = COALESCE(excluded.upstream_name, server_meta.upstream_name),
            upstream_title = COALESCE(excluded.upstream_title, server_meta.upstream_title),
            server_version = COALESCE(excluded.server_version, server_meta.server_version),
            protocol_version = COALESCE(excluded.protocol_version, server_meta.protocol_version),
            author = COALESCE(excluded.author, server_meta.author),
            category = COALESCE(excluded.category, server_meta.category),
            recommended_scenario = COALESCE(excluded.recommended_scenario, server_meta.recommended_scenario),
            rating = COALESCE(excluded.rating, server_meta.rating),
            updated_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(&meta_id)
    .bind(&meta.server_id)
    .bind(&server_name)
    .bind(&meta.description)
    .bind(&meta.website)
    .bind(&meta.repository)
    .bind(&meta.registry_version)
    .bind(&meta.registry_meta_json)
    .bind(&meta.extras_json)
    .bind(&meta.icons_json)
    .bind(&meta.upstream_name)
    .bind(&meta.upstream_title)
    .bind(&meta.server_version)
    .bind(&meta.protocol_version)
    .bind(&meta.author)
    .bind(&meta.category)
    .bind(&meta.recommended_scenario)
    .bind(meta.rating)
    .execute(pool)
    .await
    .context("Failed to upsert server metadata")?;

    if result.rows_affected() == 0 {
        // If no rows were affected, get the existing ID
        let existing_id = sqlx::query_scalar::<_, String>(
            r#"
            SELECT id FROM server_meta
            WHERE server_id = ?
            "#,
        )
        .bind(&meta.server_id)
        .fetch_one(pool)
        .await
        .context("Failed to get server metadata ID")?;

        return Ok(existing_id);
    }

    Ok(meta_id)
}

/// Update server icons based on the rmcp::Implementation metadata.
pub async fn update_server_icons(
    pool: &Pool<Sqlite>,
    server_id: &str,
    icons: Option<Vec<rmcp::model::Icon>>,
) -> Result<()> {
    let icons_json = if let Some(icons) = icons {
        Some(serde_json::to_string(&icons).context("Failed to serialize server icons")?)
    } else {
        None
    };

    let server_name = get_server_name(pool, server_id).await?;
    let icon_id = generate_id!("smet");

    sqlx::query(
        r#"
        INSERT INTO server_meta (id, server_id, server_name, icons_json)
        VALUES (?, ?, ?, ?)
        ON CONFLICT(server_id) DO UPDATE SET
            server_name = excluded.server_name,
            icons_json = excluded.icons_json,
            updated_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(icon_id)
    .bind(server_id)
    .bind(server_name)
    .bind(icons_json)
    .execute(pool)
    .await
    .context("Failed to upsert server icons")?;

    Ok(())
}

/// Update standard server information and protocol version captured from peer info.
pub async fn update_server_info(
    pool: &Pool<Sqlite>,
    server_id: &str,
    upstream_name: String,
    upstream_title: Option<String>,
    server_version: Option<String>,
    protocol_version: String,
) -> Result<()> {
    let server_name = get_server_name(pool, server_id).await?;
    let meta_id = generate_id!("smet");

    sqlx::query(
        r#"
        INSERT INTO server_meta (
            id,
            server_id,
            server_name,
            upstream_name,
            upstream_title,
            server_version,
            protocol_version
        )
        VALUES (?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT(server_id) DO UPDATE SET
            server_name = excluded.server_name,
            upstream_name = excluded.upstream_name,
            upstream_title = excluded.upstream_title,
            server_version = excluded.server_version,
            protocol_version = excluded.protocol_version,
            updated_at = CURRENT_TIMESTAMP
        "#,
    )
    .bind(meta_id)
    .bind(server_id)
    .bind(server_name)
    .bind(upstream_name)
    .bind(upstream_title)
    .bind(server_version)
    .bind(protocol_version)
    .execute(pool)
    .await
    .context("Failed to upsert standard server information")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::*;
    use crate::common::{server::ServerType, status::EnabledStatus};
    use crate::config::models::Server;

    #[tokio::test]
    async fn observed_server_info_is_persisted_without_overwriting_namespace() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect in-memory database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        let server = Server {
            id: Some("server-a".to_string()),
            name: "managed_namespace".to_string(),
            server_type: ServerType::Stdio,
            command: Some("server-command".to_string()),
            url: None,
            source: None,
            enabled: EnabledStatus::Enabled,
            unify_direct_exposure_eligible: false,
            pending_import: false,
            created_at: None,
            updated_at: None,
        };
        crate::config::server::upsert_server(&pool, &server)
            .await
            .expect("insert server");

        update_server_info(
            &pool,
            "server-a",
            "upstream-server".to_string(),
            Some("Upstream Title".to_string()),
            Some("1.2.3".to_string()),
            "2025-11-25".to_string(),
        )
        .await
        .expect("persist server info");

        update_server_info(
            &pool,
            "server-a",
            "renamed-upstream-server".to_string(),
            Some("Renamed Upstream Title".to_string()),
            Some("2.0.0".to_string()),
            "2025-11-25".to_string(),
        )
        .await
        .expect("refresh server info");

        let namespace = sqlx::query_scalar::<_, String>("SELECT name FROM server_config WHERE id = 'server-a'")
            .fetch_one(&pool)
            .await
            .expect("load namespace");
        let meta = get_server_meta(&pool, "server-a")
            .await
            .expect("load server metadata")
            .expect("server metadata exists");
        assert_eq!(namespace, "managed_namespace");
        assert_eq!(meta.upstream_name.as_deref(), Some("renamed-upstream-server"));
        assert_eq!(meta.upstream_title.as_deref(), Some("Renamed Upstream Title"));
        assert_eq!(meta.server_version.as_deref(), Some("2.0.0"));
        assert_eq!(meta.protocol_version.as_deref(), Some("2025-11-25"));
    }
}
