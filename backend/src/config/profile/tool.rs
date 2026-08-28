#[cfg(test)]
use std::str::FromStr;

use anyhow::{Context, Result};
#[cfg(test)]
use mcpmate_capability_store::CapabilityRefId;
use mcpmate_capability_store::{CapabilityKind, EffectiveCapabilityDefinition};
use sqlx::{Pool, Sqlite, Transaction};

#[cfg(test)]
use crate::config::profile::capability_ref::upsert_profile_capability_ref;
use crate::config::{
    models::ProfileToolWithDetails,
    profile::capability_ref::{load_profile_capability_refs, load_profile_capability_refs_in_transaction},
};

#[derive(Debug, Clone)]
pub struct ToolStatus {
    pub ref_id: String,
    pub unique_name: String,
    pub enabled: bool,
}

pub struct ToolStatusService;

impl ToolStatusService {
    pub async fn get_tool_status(
        pool: &Pool<Sqlite>,
        server_name: &str,
        tool_name: &str,
    ) -> Result<ToolStatus> {
        sqlx::query_as::<_, (String, String, bool)>(
            r#"
            SELECT pcr.ref_id, st.unique_name, pcr.enabled
            FROM profile_capability_refs pcr
            JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
            JOIN profile p ON p.id = pcr.profile_id
            JOIN server_config sc ON sc.id = cr.server_id
            JOIN server_tools st
              ON st.server_id = cr.server_id
             AND st.tool_name = cr.origin_key
            WHERE p.is_active = 1 AND p.profile_mode != 'workflow'
              AND cr.kind = 'tools'
              AND sc.name = ?
              AND cr.origin_key = ?
            ORDER BY p.priority DESC, p.id
            LIMIT 1
            "#,
        )
        .bind(server_name)
        .bind(tool_name)
        .fetch_optional(pool)
        .await
        .context("Failed to load Tool authoring status")?
        .map(|(ref_id, unique_name, enabled)| ToolStatus {
            ref_id,
            unique_name,
            enabled,
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Tool relationship not found for server '{}' and origin '{}'",
                server_name,
                tool_name
            )
        })
    }
}

pub fn build_enabled_tools_query(additional_where: Option<&str>) -> String {
    let base_query = r#"
        SELECT DISTINCT st.unique_name, st.server_name, st.tool_name, st.server_id
        FROM profile_capability_refs pcr
        JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
        JOIN profile p ON pcr.profile_id = p.id
        JOIN server_tools st
          ON st.server_id = cr.server_id
         AND st.tool_name = cr.origin_key
        JOIN server_config sc ON sc.id = cr.server_id
        WHERE p.is_active = 1 AND p.profile_mode != 'workflow'
          AND pcr.enabled = 1
          AND cr.state = 'active'
          AND cr.kind = 'tools'
          AND sc.enabled = 1"#;
    match additional_where {
        Some(condition) => format!("{base_query} AND {condition}"),
        None => base_query.to_string(),
    }
}

pub fn build_tool_details_query(additional_where: Option<&str>) -> String {
    let base_query = r#"
        SELECT
            pcr.profile_id,
            pcr.ref_id,
            pcr.enabled,
            cr.server_id,
            sc.name AS server_name,
            cr.origin_key AS tool_name,
            st.unique_name,
            st.description,
            cr.state,
            cr.state_generation
        FROM profile_capability_refs pcr
        JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
        JOIN server_config sc ON sc.id = cr.server_id
        LEFT JOIN server_tools st
          ON st.server_id = cr.server_id
         AND st.tool_name = cr.origin_key
        WHERE cr.kind = 'tools'"#;
    match additional_where {
        Some(condition) => format!("{base_query} AND {condition}"),
        None => base_query.to_string(),
    }
}

pub async fn get_profile_tools(
    pool: &Pool<Sqlite>,
    profile_id: &str,
) -> Result<Vec<ProfileToolWithDetails>> {
    let relationships = load_profile_capability_refs(pool, profile_id, Some(CapabilityKind::Tools)).await?;
    profile_tools_from_relationships(relationships)
}

pub async fn get_profile_tools_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<Vec<ProfileToolWithDetails>> {
    let relationships =
        load_profile_capability_refs_in_transaction(transaction, profile_id, Some(CapabilityKind::Tools)).await?;
    profile_tools_from_relationships(relationships)
}

fn profile_tools_from_relationships(
    relationships: Vec<crate::config::profile::capability_ref::ProfileCapabilityRef>
) -> Result<Vec<ProfileToolWithDetails>> {
    let mut tools = Vec::with_capacity(relationships.len());
    for relationship in relationships {
        let EffectiveCapabilityDefinition::Tool(tool) = relationship.definition else {
            anyhow::bail!(
                "Capability ref '{}' does not contain a Tool definition",
                relationship.ref_id
            );
        };
        tools.push(ProfileToolWithDetails {
            profile_id: relationship.profile_id,
            ref_id: relationship.ref_id.to_string(),
            enabled: relationship.enabled,
            server_id: relationship.server_id,
            server_name: relationship.server_name,
            tool_name: relationship.origin_key,
            unique_name: relationship.external_key,
            description: tool.description.map(|value| value.to_string()),
            state: relationship.state.as_str().to_string(),
            state_generation: relationship.state_generation,
        });
    }
    Ok(tools)
}

#[cfg(test)]
pub(crate) async fn add_tool_to_profile(
    pool: &Pool<Sqlite>,
    profile_id: &str,
    server_id: &str,
    ref_id: &str,
    enabled: bool,
) -> Result<String> {
    let ref_id =
        CapabilityRefId::from_str(ref_id).with_context(|| format!("Invalid Tool capability ref '{ref_id}'"))?;
    let relationship = load_ref_for_profile_write(pool, &ref_id).await?;
    if relationship.0 != server_id || relationship.1 != CapabilityKind::Tools {
        anyhow::bail!(
            "Capability ref '{}' is not a Tool owned by server '{}'",
            ref_id,
            server_id
        );
    }
    upsert_profile_capability_ref(pool, profile_id, &ref_id, enabled).await?;
    crate::core::events::EventBus::global().publish(crate::core::events::Event::ToolEnabledInProfileChanged {
        tool_id: ref_id.to_string(),
        tool_name: relationship.2,
        profile_id: profile_id.to_string(),
        enabled,
    });
    Ok(ref_id.to_string())
}

#[cfg(test)]
async fn load_ref_for_profile_write(
    pool: &Pool<Sqlite>,
    ref_id: &CapabilityRefId,
) -> Result<(String, CapabilityKind, String)> {
    let row: (String, String, String) =
        sqlx::query_as("SELECT server_id, kind, origin_key FROM capability_refs WHERE ref_id = ?")
            .bind(ref_id.as_str())
            .fetch_optional(pool)
            .await
            .context("Failed to load capability ref")?
            .ok_or_else(|| anyhow::anyhow!("Capability ref '{}' is not registered", ref_id))?;
    let kind = CapabilityKind::parse(&row.1).ok_or_else(|| anyhow::anyhow!("Invalid capability kind '{}'", row.1))?;
    Ok((row.0, kind, row.2))
}
