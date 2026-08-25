use anyhow::{Context, Result};
use sqlx::{Pool, Sqlite};

use crate::config::models::ProfileServer;
#[cfg(test)]
use crate::config::profile::capability_ref::NewRefPolicy;

pub async fn get_profile_servers(
    pool: &Pool<Sqlite>,
    profile_id: &str,
) -> Result<Vec<ProfileServer>> {
    sqlx::query_as(
        r#"
        SELECT NULL AS id, profile_id, server_id, enabled,
               new_ref_policy, NULL AS created_at, NULL AS updated_at
        FROM profile_server_relationships
        WHERE profile_id = ?
        ORDER BY server_id
        "#,
    )
    .bind(profile_id)
    .fetch_all(pool)
    .await
    .context("Failed to fetch Profile server relationships")
}

#[cfg(test)]
pub(crate) async fn add_server_to_profile(
    pool: &Pool<Sqlite>,
    profile_id: &str,
    server_id: &str,
    enabled: bool,
) -> Result<String> {
    let server_name = server_name(pool, server_id).await?;
    sqlx::query(
        r#"
        INSERT INTO profile_server_relationships
            (profile_id, server_id, enabled, new_ref_policy)
        VALUES (?, ?, ?, 'follow')
        ON CONFLICT(profile_id, server_id) DO UPDATE SET
            enabled = excluded.enabled
        "#,
    )
    .bind(profile_id)
    .bind(server_id)
    .bind(enabled)
    .execute(pool)
    .await
    .context("Failed to update Profile server enabled state")?;
    crate::core::events::EventBus::global().publish(crate::core::events::Event::ServerEnabledInProfileChanged {
        server_id: server_id.to_string(),
        server_name,
        profile_id: profile_id.to_string(),
        enabled,
    });
    Ok(format!("{profile_id}/{server_id}"))
}

#[cfg(test)]
pub(crate) async fn set_server_relationship(
    pool: &Pool<Sqlite>,
    profile_id: &str,
    server_id: &str,
    new_ref_policy: NewRefPolicy,
) -> Result<()> {
    server_name(pool, server_id).await?;
    sqlx::query(
        r#"
        INSERT INTO profile_server_relationships
            (profile_id, server_id, new_ref_policy)
        VALUES (?, ?, ?)
        ON CONFLICT(profile_id, server_id) DO UPDATE SET
            new_ref_policy = excluded.new_ref_policy
        "#,
    )
    .bind(profile_id)
    .bind(server_id)
    .bind(new_ref_policy.as_str())
    .execute(pool)
    .await
    .context("Failed to persist Profile server relationship")?;
    Ok(())
}

#[cfg(test)]
pub(crate) async fn remove_server_from_profile(
    pool: &Pool<Sqlite>,
    profile_id: &str,
    server_id: &str,
) -> Result<bool> {
    let server_name = known_server_name(pool, server_id)
        .await?
        .unwrap_or_else(|| server_id.to_string());
    let result = sqlx::query("DELETE FROM profile_server_relationships WHERE profile_id = ? AND server_id = ?")
        .bind(profile_id)
        .bind(server_id)
        .execute(pool)
        .await
        .context("Failed to remove Profile server relationship")?;
    if result.rows_affected() == 1 {
        crate::core::events::EventBus::global().publish(crate::core::events::Event::ServerEnabledInProfileChanged {
            server_id: server_id.to_string(),
            server_name,
            profile_id: profile_id.to_string(),
            enabled: false,
        });
    }
    Ok(result.rows_affected() == 1)
}

#[cfg(test)]
async fn server_name(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<String> {
    sqlx::query_scalar("SELECT name FROM server_config WHERE id = ?")
        .bind(server_id)
        .fetch_optional(pool)
        .await
        .context("Failed to load server for Profile relationship")?
        .ok_or_else(|| anyhow::anyhow!("Server '{}' does not exist", server_id))
}

#[cfg(test)]
async fn known_server_name(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<Option<String>> {
    sqlx::query_scalar(
        r#"
        SELECT name
        FROM server_config
        WHERE id = ?
        UNION ALL
        SELECT server_name
        FROM capability_server_snapshots
        WHERE server_id = ?
          AND NOT EXISTS (SELECT 1 FROM server_config WHERE id = ?)
        LIMIT 1
        "#,
    )
    .bind(server_id)
    .bind(server_id)
    .bind(server_id)
    .fetch_optional(pool)
    .await
    .context("Failed to load retained Profile server relationship name")
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::SqlitePoolOptions;

    use super::{get_profile_servers, remove_server_from_profile, set_server_relationship};
    use crate::config::profile::capability_ref::NewRefPolicy;

    #[tokio::test]
    async fn server_relationship_persists_stable_server_and_new_ref_policy() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .unwrap();
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .unwrap();
        crate::config::database::initialize_capability_catalog(&pool)
            .await
            .unwrap();
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO server_config (id, name, server_type, command, enabled) VALUES ('server-a', 'Server A', 'stdio', '', 1)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO profile (id, name, description, type, role) VALUES ('profile-a', 'Profile A', '', 'shared', 'user')",
        )
        .execute(&pool)
        .await
        .unwrap();

        set_server_relationship(&pool, "profile-a", "server-a", NewRefPolicy::Review)
            .await
            .unwrap();

        let relationships = get_profile_servers(&pool, "profile-a").await.unwrap();
        assert_eq!(relationships.len(), 1);
        assert_eq!(relationships[0].server_id, "server-a");
        assert_eq!(relationships[0].new_ref_policy, "review");
        assert!(relationships[0].enabled);

        sqlx::query("DELETE FROM server_config WHERE id = 'server-a'")
            .execute(&pool)
            .await
            .expect("remove live server row");
        assert!(
            remove_server_from_profile(&pool, "profile-a", "server-a")
                .await
                .unwrap()
        );
        assert!(get_profile_servers(&pool, "profile-a").await.unwrap().is_empty());
    }
}
