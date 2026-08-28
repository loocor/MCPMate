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
    models::ProfileResource,
    profile::capability_ref::{load_profile_capability_refs, load_profile_capability_refs_in_transaction},
};

#[cfg(test)]
pub(crate) async fn add_resource_template_to_profile(
    pool: &Pool<Sqlite>,
    profile_id: &str,
    server_id: &str,
    ref_id: &str,
    enabled: bool,
) -> Result<String> {
    let ref_id = parse_owned_ref(pool, server_id, ref_id).await?;
    upsert_profile_capability_ref(pool, profile_id, &ref_id, enabled).await?;
    Ok(ref_id.to_string())
}

pub async fn get_resource_templates_for_profile(
    pool: &Pool<Sqlite>,
    profile_id: &str,
) -> Result<Vec<ProfileResource>> {
    let relationships = load_profile_capability_refs(pool, profile_id, Some(CapabilityKind::ResourceTemplates)).await?;
    resource_templates_from_relationships(relationships)
}

pub async fn get_resource_templates_for_profile_in_transaction(
    transaction: &mut Transaction<'_, Sqlite>,
    profile_id: &str,
) -> Result<Vec<ProfileResource>> {
    let relationships =
        load_profile_capability_refs_in_transaction(transaction, profile_id, Some(CapabilityKind::ResourceTemplates))
            .await?;
    resource_templates_from_relationships(relationships)
}

fn resource_templates_from_relationships(
    relationships: Vec<crate::config::profile::capability_ref::ProfileCapabilityRef>
) -> Result<Vec<ProfileResource>> {
    let mut templates = Vec::with_capacity(relationships.len());
    for relationship in relationships {
        let EffectiveCapabilityDefinition::ResourceTemplate(template) = relationship.definition else {
            anyhow::bail!(
                "Capability ref '{}' does not contain a Resource Template definition",
                relationship.ref_id
            );
        };
        templates.push(ProfileResource {
            id: Some(relationship.ref_id.to_string()),
            profile_id: relationship.profile_id,
            server_id: relationship.server_id,
            server_name: relationship.server_name,
            resource_uri: relationship.origin_key,
            unique_uri: relationship.external_key,
            description: template.description,
            enabled: relationship.enabled,
            state: relationship.state.as_str().to_string(),
            state_generation: relationship.state_generation,
        });
    }
    Ok(templates)
}

pub async fn get_enabled_resource_templates_for_profile(
    pool: &Pool<Sqlite>,
    profile_id: &str,
) -> Result<Vec<ProfileResource>> {
    Ok(get_resource_templates_for_profile(pool, profile_id)
        .await?
        .into_iter()
        .filter(|template| template.enabled && template.state == "active")
        .collect())
}

pub fn build_enabled_resource_templates_query(additional_where: Option<&str>) -> String {
    let base_query = r#"
        SELECT DISTINCT cr.server_id, sc.name AS server_name,
                        cr.origin_key AS uri_template
        FROM profile_capability_refs pcr
        JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
        JOIN profile p ON p.id = pcr.profile_id
        JOIN server_config sc ON sc.id = cr.server_id
        WHERE p.is_active = 1 AND p.profile_mode != 'workflow'
          AND pcr.enabled = 1
          AND cr.state = 'active'
          AND cr.kind = 'resource_templates'
          AND sc.enabled = 1"#;
    match additional_where {
        Some(condition) => format!("{base_query} AND {condition}"),
        None => base_query.to_string(),
    }
}

pub fn template_prefix(uri_template: &str) -> &str {
    for (index, byte) in uri_template.as_bytes().iter().enumerate() {
        if *byte == b'{' || *byte == b'*' {
            return &uri_template[..index];
        }
    }
    uri_template
}

pub async fn resource_matches_enabled_templates(
    pool: &Pool<Sqlite>,
    profile_id: &str,
    server_id: &str,
    resource_uri: &str,
) -> Result<bool> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT cr.origin_key
        FROM profile_capability_refs pcr
        JOIN capability_refs cr ON cr.ref_id = pcr.ref_id
        WHERE pcr.profile_id = ?
          AND cr.server_id = ?
          AND cr.kind = 'resource_templates'
          AND cr.state = 'active'
          AND pcr.enabled = 1
        "#,
    )
    .bind(profile_id)
    .bind(server_id)
    .fetch_all(pool)
    .await
    .context("Failed to fetch enabled Resource Template refs for Profile and server")?;

    Ok(rows.into_iter().any(|(template,)| {
        let prefix = template_prefix(&template);
        !prefix.is_empty() && resource_uri.starts_with(prefix)
    }))
}

#[cfg(test)]
async fn parse_owned_ref(
    pool: &Pool<Sqlite>,
    server_id: &str,
    ref_id: &str,
) -> Result<CapabilityRefId> {
    let ref_id = CapabilityRefId::from_str(ref_id)
        .with_context(|| format!("Invalid Resource Template capability ref '{ref_id}'"))?;
    let owner: (String, String) = sqlx::query_as("SELECT server_id, kind FROM capability_refs WHERE ref_id = ?")
        .bind(ref_id.as_str())
        .fetch_optional(pool)
        .await
        .context("Failed to load Resource Template capability ref")?
        .ok_or_else(|| anyhow::anyhow!("Capability ref '{}' is not registered", ref_id))?;
    if owner.0 != server_id || CapabilityKind::parse(&owner.1) != Some(CapabilityKind::ResourceTemplates) {
        anyhow::bail!(
            "Capability ref '{}' is not a Resource Template owned by server '{}'",
            ref_id,
            server_id
        );
    }
    Ok(ref_id)
}
