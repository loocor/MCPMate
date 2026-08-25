// Basic CRUD operations for server configuration
// Contains create, read, update, delete operations for servers

use anyhow::{Context, Result};
use sqlx::{Pool, Sqlite, Transaction};

use crate::common::{
    constants::database::{columns, tables},
    database::fetch_optional,
};
use crate::config::models::Server;
use crate::generate_id;

/// Get all servers from the database
pub async fn get_all_servers(pool: &Pool<Sqlite>) -> Result<Vec<Server>> {
    let servers = sqlx::query_as::<_, Server>(&format!(
        r#"
        SELECT * FROM {}
        WHERE COALESCE({}, 0) = 0
        ORDER BY {} ASC
        "#,
        tables::SERVER_CONFIG,
        columns::PENDING_IMPORT,
        columns::NAME
    ))
    .fetch_all(pool)
    .await
    .context("Failed to fetch visible servers")?;

    Ok(servers)
}

/// Get a specific server from the database by name
pub async fn get_server(
    pool: &Pool<Sqlite>,
    name: &str,
) -> Result<Option<Server>> {
    let server: Option<Server> = fetch_optional(pool, tables::SERVER_CONFIG, columns::NAME, name).await?;

    if let Some(ref s) = server {
        tracing::debug!("Found server '{}', type: {}", name, s.server_type);
    } else {
        tracing::debug!("No server found with name '{}'", name);
    }

    Ok(server)
}

/// Get a specific server from the database by ID
pub async fn get_server_by_id(
    pool: &Pool<Sqlite>,
    id: &str,
) -> Result<Option<Server>> {
    tracing::debug!("Executing SQL query to get server with ID '{}'", id);

    let server = sqlx::query_as::<_, Server>(&format!(
        r#"
        SELECT * FROM {}
        WHERE {} = ?
        "#,
        tables::SERVER_CONFIG,
        columns::ID
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("Failed to fetch server by ID")?;

    if let Some(ref s) = server {
        tracing::debug!("Found server with ID '{}', name: {}", id, s.name);
    } else {
        tracing::debug!("No server found with ID '{}'", id);
    }

    Ok(server)
}

/// Create or update a server in the database
pub async fn upsert_server(
    pool: &Pool<Sqlite>,
    server: &Server,
) -> Result<String> {
    tracing::debug!("Upserting server '{}', type: {}", server.name, server.server_type);

    let mut tx = pool.begin().await.context("Failed to begin transaction")?;
    let server_id = upsert_server_tx(&mut tx, server).await?;
    tx.commit().await.context("Failed to commit transaction")?;

    Ok(server_id)
}

/// Create or update a server in the database (transaction version)
pub async fn upsert_server_tx(
    tx: &mut Transaction<'_, Sqlite>,
    server: &Server,
) -> Result<String> {
    // Generate an ID for the server if it doesn't have one
    let server_id = if let Some(id) = &server.id {
        id.clone()
    } else {
        generate_id!("serv")
    };

    let result = sqlx::query(&format!(
        r#"
        INSERT INTO {} ({}, {}, {}, {}, {}, {}, {}, {})
        VALUES (?, ?, ?, ?, ?, ?, ?, ?)
        ON CONFLICT({}) DO UPDATE SET
            {} = excluded.{},
            {} = excluded.{},
            {} = excluded.{},
            {} = excluded.{},
            {} = excluded.{},
            {} = excluded.{},
            {} = CURRENT_TIMESTAMP
        "#,
        tables::SERVER_CONFIG,
        columns::ID,
        columns::NAME,
        columns::SERVER_TYPE,
        columns::COMMAND,
        columns::URL,
        columns::SOURCE,
        columns::UNIFY_DIRECT_EXPOSURE_ELIGIBLE,
        columns::PENDING_IMPORT,
        columns::NAME,
        columns::SERVER_TYPE,
        columns::SERVER_TYPE,
        columns::COMMAND,
        columns::COMMAND,
        columns::URL,
        columns::URL,
        columns::SOURCE,
        columns::SOURCE,
        columns::UNIFY_DIRECT_EXPOSURE_ELIGIBLE,
        columns::UNIFY_DIRECT_EXPOSURE_ELIGIBLE,
        columns::PENDING_IMPORT,
        columns::PENDING_IMPORT,
        columns::UPDATED_AT
    ))
    .bind(&server_id)
    .bind(&server.name)
    .bind(server.server_type)
    .bind(&server.command)
    .bind(&server.url)
    .bind(&server.source)
    .bind(server.unify_direct_exposure_eligible)
    .bind(server.pending_import)
    .execute(&mut **tx)
    .await
    .context("Failed to upsert server")?;

    if result.rows_affected() == 0 {
        // If no rows were affected, get the existing ID
        let existing_id = sqlx::query_scalar::<_, String>(&format!(
            r#"
            SELECT {} FROM {}
            WHERE {} = ?
            "#,
            columns::ID,
            tables::SERVER_CONFIG,
            columns::NAME
        ))
        .bind(&server.name)
        .fetch_one(&mut **tx)
        .await
        .context("Failed to get server ID")?;

        return Ok(existing_id);
    }

    Ok(server_id)
}

/// Delete a server from the database
pub async fn delete_server(
    pool: &Pool<Sqlite>,
    name: &str,
) -> Result<bool> {
    tracing::debug!("Deleting server '{}'", name);

    let result = sqlx::query(
        r#"
        DELETE FROM server_config
        WHERE name = ?
        "#,
    )
    .bind(name)
    .execute(pool)
    .await
    .context("Failed to delete server")?;

    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        common::{server::ServerType, status::EnabledStatus},
        config::{models::Server, server::init::initialize_server_tables},
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
        crate::helpers::prepare_config_database(&pool).await;
        initialize_server_tables(&pool).await.expect("init tables");
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
    async fn get_all_servers_hides_pending_import_records() {
        let pool = setup_pool().await;

        upsert_server(&pool, &build_server("serv_visible", "visible-server", false))
            .await
            .expect("insert visible server");
        upsert_server(&pool, &build_server("serv_hidden", "hidden-server", true))
            .await
            .expect("insert hidden server");

        let servers = get_all_servers(&pool).await.expect("list visible servers");
        let names = servers.into_iter().map(|server| server.name).collect::<Vec<_>>();

        assert_eq!(names, vec!["visible-server".to_string()]);
    }
}
