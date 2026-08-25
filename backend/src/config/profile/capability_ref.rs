use std::str::FromStr;

use anyhow::{Context, Result};
use mcpmate_capability_store::{
    CapabilityId, CapabilityKind, CapabilityRefId, CapabilityRefState, EffectiveCapabilityDefinition,
    EffectiveCapabilityRecordV1,
};
use serde::{Deserialize, Serialize};
use sqlx::{Executor, FromRow, Pool, Sqlite, Transaction};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NewRefPolicy {
    Follow,
    Review,
}

impl NewRefPolicy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Follow => "follow",
            Self::Review => "review",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProfileCapabilityRef {
    pub profile_id: String,
    pub ref_id: CapabilityRefId,
    pub enabled: bool,
    pub server_id: String,
    pub server_name: String,
    pub kind: CapabilityKind,
    pub origin_key: String,
    pub state: CapabilityRefState,
    pub state_generation: i64,
    pub last_known_capability_id: CapabilityId,
    pub external_key: String,
    pub definition: EffectiveCapabilityDefinition,
}

#[derive(FromRow)]
struct ProfileCapabilityRefRow {
    profile_id: String,
    ref_id: String,
    enabled: bool,
    server_id: String,
    server_name: String,
    kind: String,
    origin_key: String,
    state: String,
    state_generation: i64,
    capability_id: String,
    canonical_record: Vec<u8>,
}

#[cfg(test)]
pub(crate) async fn upsert_profile_capability_ref(
    pool: &Pool<Sqlite>,
    profile_id: &str,
    ref_id: &CapabilityRefId,
    enabled: bool,
) -> Result<()> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM capability_refs WHERE ref_id = ?)")
        .bind(ref_id.as_str())
        .fetch_one(pool)
        .await
        .context("Failed to validate capability ref")?;
    if !exists {
        anyhow::bail!("Capability ref '{}' is not registered", ref_id);
    }
    sqlx::query(
        r#"
        INSERT INTO profile_capability_refs (profile_id, ref_id, enabled)
        VALUES (?, ?, ?)
        ON CONFLICT(profile_id, ref_id) DO UPDATE SET enabled = excluded.enabled
        "#,
    )
    .bind(profile_id)
    .bind(ref_id.as_str())
    .bind(enabled)
    .execute(pool)
    .await
    .context("Failed to persist profile capability ref")?;
    Ok(())
}

pub async fn load_profile_capability_refs(
    pool: &Pool<Sqlite>,
    profile_id: &str,
    kind: Option<CapabilityKind>,
) -> Result<Vec<ProfileCapabilityRef>> {
    load_profile_capability_refs_from(pool, profile_id, kind).await
}

pub async fn load_profile_capability_refs_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
    kind: Option<CapabilityKind>,
) -> Result<Vec<ProfileCapabilityRef>> {
    load_profile_capability_refs_from(&mut **transaction, profile_id, kind).await
}

async fn load_profile_capability_refs_from<'e, E>(
    executor: E,
    profile_id: &str,
    kind: Option<CapabilityKind>,
) -> Result<Vec<ProfileCapabilityRef>>
where
    E: Executor<'e, Database = Sqlite>,
{
    let rows = sqlx::query_as::<_, ProfileCapabilityRefRow>(
        r#"
        WITH profile_refs AS (
            SELECT pcr.ref_id,
                   CASE
                       WHEN psr.server_id IS NULL THEN pcr.enabled
                       ELSE pcr.enabled AND psr.enabled
                   END AS enabled
            FROM profile_capability_refs pcr
            JOIN capability_refs explicit_ref ON explicit_ref.ref_id = pcr.ref_id
            LEFT JOIN profile_server_relationships psr
              ON psr.profile_id = pcr.profile_id
             AND psr.server_id = explicit_ref.server_id
            WHERE pcr.profile_id = ?

            UNION ALL

            SELECT cr.ref_id, psr.enabled
            FROM profile_server_relationships psr
            JOIN capability_refs cr ON cr.server_id = psr.server_id
            WHERE psr.profile_id = ?
              AND NOT EXISTS (
                  SELECT 1
                  FROM profile_capability_refs explicit
                  WHERE explicit.profile_id = psr.profile_id
                    AND explicit.ref_id = cr.ref_id
              )
        ),
        effective_refs AS (
            SELECT ref_id, MAX(enabled) AS enabled
            FROM profile_refs
            GROUP BY ref_id
        )
        SELECT ? AS profile_id, effective.ref_id, effective.enabled,
               cr.server_id, COALESCE(server.name, snapshot.server_name, cr.server_id) AS server_name,
               cr.kind, cr.origin_key, cr.state, cr.state_generation,
               versions.capability_id, versions.canonical_record
        FROM effective_refs effective
        JOIN capability_refs cr ON cr.ref_id = effective.ref_id
        LEFT JOIN server_config server ON server.id = cr.server_id
        LEFT JOIN capability_server_snapshots snapshot ON snapshot.server_id = cr.server_id
        LEFT JOIN capability_ref_current current ON current.ref_id = cr.ref_id
        JOIN capability_versions versions ON versions.capability_id = COALESCE(
            current.capability_id,
            (
                SELECT historical.capability_id
                FROM capability_versions historical
                WHERE historical.ref_id = cr.ref_id
                ORDER BY historical.first_observed_revision DESC, historical.capability_id DESC
                LIMIT 1
            )
        )
        WHERE (? IS NULL OR cr.kind = ?)
        ORDER BY cr.server_id, cr.kind, cr.origin_key, cr.ref_id
        "#,
    )
    .bind(profile_id)
    .bind(profile_id)
    .bind(profile_id)
    .bind(kind.map(CapabilityKind::as_str))
    .bind(kind.map(CapabilityKind::as_str))
    .fetch_all(executor)
    .await
    .context("Failed to load profile capability refs")?;
    rows.into_iter().map(ProfileCapabilityRef::try_from).collect()
}

impl TryFrom<ProfileCapabilityRefRow> for ProfileCapabilityRef {
    type Error = anyhow::Error;

    fn try_from(row: ProfileCapabilityRefRow) -> Result<Self> {
        let kind = CapabilityKind::parse(&row.kind)
            .ok_or_else(|| anyhow::anyhow!("Invalid capability kind '{}'", row.kind))?;
        let state = CapabilityRefState::parse(&row.state)
            .ok_or_else(|| anyhow::anyhow!("Invalid capability ref state '{}'", row.state))?;
        let ref_id = CapabilityRefId::from_str(&row.ref_id)?;
        ref_id.verify_source(&mcpmate_capability_store::CapabilitySourceIdentity::new(
            &row.server_id,
            kind,
            &row.origin_key,
        ))?;
        let last_known_capability_id = CapabilityId::from_str(&row.capability_id)?;
        let effective_record: EffectiveCapabilityRecordV1 = serde_json::from_slice(&row.canonical_record)?;
        effective_record.validate()?;
        last_known_capability_id.verify_canonical_content(&row.canonical_record, &row.canonical_record)?;
        if effective_record.ref_id != ref_id {
            anyhow::bail!("Capability ref '{}' current version has mismatched source", ref_id);
        }
        let external_key = effective_record.definition.external_key();
        Ok(Self {
            profile_id: row.profile_id,
            ref_id,
            enabled: row.enabled,
            server_id: row.server_id,
            server_name: row.server_name,
            kind,
            origin_key: row.origin_key,
            state,
            state_generation: row.state_generation,
            last_known_capability_id,
            external_key,
            definition: effective_record.definition,
        })
    }
}

pub async fn load_capability_server_name(
    pool: &Pool<Sqlite>,
    server_id: &str,
) -> Result<String> {
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
    .context("Failed to load current or retained capability server name")?
    .ok_or_else(|| anyhow::anyhow!("Capability server '{}' is not available", server_id))
}

#[cfg(test)]
mod tests {
    use mcpmate_capability_store::{
        CapabilityCatalog, CapabilityKind, CapabilityObservation, CapabilityPayload, CapabilityRefState, CatalogRecord,
        DeclarationState, InventoryState, KindObservation, SqliteCapabilityCatalog,
    };
    use rmcp::model::{InitializeResult, Tool};
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;

    use super::{NewRefPolicy, load_profile_capability_refs, upsert_profile_capability_ref};

    fn initialize_result() -> InitializeResult {
        serde_json::from_value(json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "fixture", "version": "1.0.0"}
        }))
        .unwrap()
    }

    fn tool_record(description: &str) -> CatalogRecord {
        let tool: Tool = serde_json::from_value(json!({
            "name": "analyze",
            "description": description,
            "inputSchema": {"type": "object"}
        }))
        .unwrap();
        CatalogRecord::materialize(
            "server-a",
            "analyze",
            "server_a__analyze",
            CapabilityPayload::Tool(tool),
        )
        .unwrap()
    }

    fn observation(records: Vec<CatalogRecord>) -> CapabilityObservation {
        CapabilityObservation::new(
            "server-a",
            "Server A",
            "config-v1",
            initialize_result(),
            vec![KindObservation::new(
                CapabilityKind::Tools,
                DeclarationState::Supported,
                InventoryState::Complete,
            )],
            records,
        )
    }

    #[tokio::test]
    async fn server_level_profile_relationship_projects_current_refs_without_seeding_capability_intent() {
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

        let record = tool_record("Visible through server-level intent");
        SqliteCapabilityCatalog::new(pool.clone())
            .commit_observation(observation(vec![record.clone()]))
            .await
            .unwrap();
        crate::config::profile::server::set_server_relationship(&pool, "profile-a", "server-a", NewRefPolicy::Follow)
            .await
            .unwrap();

        let relationships = load_profile_capability_refs(&pool, "profile-a", Some(CapabilityKind::Tools))
            .await
            .unwrap();

        assert_eq!(relationships.len(), 1);
        assert_eq!(relationships[0].ref_id, record.ref_id);
        assert!(relationships[0].enabled);
        let authored_ref_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM profile_capability_refs WHERE profile_id = 'profile-a'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(authored_ref_count, 0);
    }

    #[tokio::test]
    async fn standard_profile_intent_survives_version_change_and_complete_removal() {
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
            "INSERT INTO profile (id, name, description, type, role) VALUES ('profile-a', 'Profile A', '', 'shared', 'user')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let first = tool_record("Version one");
        let second = tool_record("Version two");
        let catalog = SqliteCapabilityCatalog::new(pool.clone());
        catalog
            .commit_observation(observation(vec![first.clone()]))
            .await
            .unwrap();
        upsert_profile_capability_ref(&pool, "profile-a", &first.ref_id, true)
            .await
            .unwrap();

        catalog.commit_observation(observation(vec![second])).await.unwrap();
        catalog.commit_observation(observation(Vec::new())).await.unwrap();

        let relationships = load_profile_capability_refs(&pool, "profile-a", Some(CapabilityKind::Tools))
            .await
            .unwrap();
        assert_eq!(relationships.len(), 1);
        assert_eq!(relationships[0].ref_id, first.ref_id);
        assert_eq!(relationships[0].state, CapabilityRefState::Unresolved);
        assert!(relationships[0].enabled);

        let mut transaction = pool.begin().await.unwrap();
        catalog
            .retire_server_in_transaction(&mut transaction, "server-a")
            .await
            .unwrap()
            .expect("retire existing capability server");
        transaction.commit().await.unwrap();

        sqlx::query("DELETE FROM server_config WHERE id = 'server-a'")
            .execute(&pool)
            .await
            .expect("remove live server row");
        let retired_relationships = load_profile_capability_refs(&pool, "profile-a", Some(CapabilityKind::Tools))
            .await
            .unwrap();
        assert_eq!(retired_relationships.len(), 1);
        assert_eq!(retired_relationships[0].state, CapabilityRefState::Retired);
        let retired_tools = crate::config::profile::tool::get_profile_tools(&pool, "profile-a")
            .await
            .unwrap();
        assert_eq!(retired_tools.len(), 1);
        assert_eq!(retired_tools[0].server_name, "Server A");
        assert_eq!(retired_tools[0].state, "retired");

        sqlx::query("DELETE FROM capability_server_snapshots WHERE server_id = 'server-a'")
            .execute(&pool)
            .await
            .expect("delete source catalog");
        let remaining_intent: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM profile_capability_refs WHERE profile_id = 'profile-a'")
                .fetch_one(&pool)
                .await
                .expect("count profile intent after source deletion");
        assert_eq!(remaining_intent, 0);
    }
}
