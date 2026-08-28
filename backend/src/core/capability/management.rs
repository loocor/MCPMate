use std::collections::{BTreeSet, HashMap, HashSet};

use mcpmate_capability_store::{CapabilityKind, CatalogError, Result};
use sqlx::{Pool, Row, Sqlite};
use uuid::Uuid;

use super::materializer::{
    MaterializationCommit, MaterializationCoordinator, MaterializationTrigger, SurfaceAuthoringLoader,
    load_default_config_mode,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileRelationshipAction {
    Enable,
    Disable,
    Remove,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileActivationAction {
    Activate,
    Deactivate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileCapabilityMutation {
    pub ref_id: String,
    pub kind: CapabilityKind,
}

#[derive(Debug)]
pub struct ConsumerMaterialization {
    pub consumer_id: String,
    pub commit: MaterializationCommit,
}

pub struct ProfileCapabilityManagementResult {
    pub mutations: Vec<ProfileCapabilityMutation>,
    pub materializations: Vec<ConsumerMaterialization>,
}

pub struct ProfileActivationMutation {
    pub profile_id: String,
    pub name: String,
    pub is_active: bool,
}

pub struct ProfileActivationManagementResult {
    pub mutations: Vec<ProfileActivationMutation>,
    pub materializations: Vec<ConsumerMaterialization>,
}

pub struct ProfileDeletionManagementResult {
    pub profile_name: String,
    pub workflow_skill_name: Option<String>,
    pub materializations: Vec<ConsumerMaterialization>,
}

pub struct ProfileSurfaceManagement;

pub struct ServerStatusManagementResult {
    pub server_id: String,
    pub server_name: String,
    pub enabled: bool,
    pub materializations: Vec<ConsumerMaterialization>,
}

pub struct ServerDirectExposureManagementResult {
    pub server_id: String,
    pub server_name: String,
    pub unify_direct_exposure_eligible: bool,
    pub materializations: Vec<ConsumerMaterialization>,
}

pub struct ServerSurfaceManagement;

impl ServerSurfaceManagement {
    pub async fn set_server_enabled(
        pool: &Pool<Sqlite>,
        server_id: &str,
        enabled: bool,
        actor: &str,
    ) -> Result<ServerStatusManagementResult> {
        let coordinator = MaterializationCoordinator::new(pool.clone());
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let default_config_mode = load_default_config_mode(pool).await?;
        let server_name: String = sqlx::query_scalar(
            "UPDATE server_config SET enabled = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ? RETURNING name",
        )
        .bind(enabled)
        .bind(server_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or_else(|| CatalogError::InvalidSurfaceValue {
            field: "server status",
            value: server_id.to_string(),
        })?;
        let consumer_ids = load_server_consumer_ids(&mut transaction, server_id, &default_config_mode).await?;
        let trigger = MaterializationTrigger::for_consumer(
            "server_status_save",
            format!("{server_id}:{}", Uuid::new_v4()),
            actor,
        );
        let mut materializations = Vec::with_capacity(consumer_ids.len());
        for consumer_id in consumer_ids {
            let commit = coordinator
                .compile_consumer_in_transaction_with_default(
                    &mut transaction,
                    &consumer_id,
                    &default_config_mode,
                    &trigger,
                )
                .await?;
            materializations.push(ConsumerMaterialization { consumer_id, commit });
        }
        transaction.commit().await?;
        Ok(ServerStatusManagementResult {
            server_id: server_id.to_string(),
            server_name,
            enabled,
            materializations,
        })
    }

    pub async fn set_direct_exposure_eligible(
        pool: &Pool<Sqlite>,
        server_id: &str,
        eligible: bool,
        source_revision_set: HashMap<String, i64>,
        actor: &str,
    ) -> Result<ServerDirectExposureManagementResult> {
        let coordinator = MaterializationCoordinator::new(pool.clone());
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let default_config_mode = load_default_config_mode(pool).await?;
        coordinator
            .verify_catalog_revision_set_in_transaction(&mut transaction, &source_revision_set)
            .await?;
        let server_name: String = sqlx::query_scalar("SELECT name FROM server_config WHERE id = ?")
            .bind(server_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or_else(|| CatalogError::InvalidSurfaceValue {
                field: "server direct exposure eligibility",
                value: server_id.to_string(),
            })?;
        let consumer_ids = load_direct_server_consumer_ids(&mut transaction, server_id, &default_config_mode).await?;
        let updated = sqlx::query(
            r#"
            UPDATE server_config
            SET unify_direct_exposure_eligible = ?, updated_at = CURRENT_TIMESTAMP
            WHERE id = ?
            "#,
        )
        .bind(eligible)
        .bind(server_id)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(CatalogError::ConcurrencyConflict {
                entity: "server direct exposure eligibility",
                id: server_id.to_string(),
            });
        }
        let trigger = MaterializationTrigger::for_consumer(
            "server_direct_exposure_eligibility_save",
            format!("{server_id}:{}", Uuid::new_v4()),
            actor,
        );
        let mut materializations = Vec::with_capacity(consumer_ids.len());
        for consumer_id in consumer_ids {
            let commit = coordinator
                .compile_consumer_in_transaction_with_default(
                    &mut transaction,
                    &consumer_id,
                    &default_config_mode,
                    &trigger,
                )
                .await?;
            materializations.push(ConsumerMaterialization { consumer_id, commit });
        }
        transaction.commit().await?;
        Ok(ServerDirectExposureManagementResult {
            server_id: server_id.to_string(),
            server_name,
            unify_direct_exposure_eligible: eligible,
            materializations,
        })
    }
}

async fn load_direct_server_consumer_ids(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    server_id: &str,
    default_config_mode: &str,
) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String, Option<String>)>(
        r#"
        SELECT DISTINCT client.identifier, client.config_mode
        FROM client
        WHERE client.approval_status = 'approved'
          AND (
                EXISTS (
                    SELECT 1
                    FROM direct_exposure_servers direct_server
                    WHERE direct_server.consumer_id = client.identifier
                      AND direct_server.server_id = ?
                )
             OR EXISTS (
                    SELECT 1
                    FROM direct_exposure_refs direct_ref
                    JOIN capability_refs capability_ref ON capability_ref.ref_id = direct_ref.ref_id
                    WHERE direct_ref.consumer_id = client.identifier
                      AND direct_ref.enabled = 1
                      AND capability_ref.server_id = ?
                )
          )
        ORDER BY client.identifier
        "#,
    )
    .bind(server_id)
    .bind(server_id)
    .fetch_all(&mut **transaction)
    .await?;
    Ok(filter_managed_consumers(rows, default_config_mode))
}

async fn load_server_consumer_ids(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    server_id: &str,
    default_config_mode: &str,
) -> Result<Vec<String>> {
    let rows = sqlx::query_as::<_, (String, Option<String>)>(
        r#"
        SELECT DISTINCT client.identifier, client.config_mode
        FROM client
        WHERE client.approval_status = 'approved'
          AND (
                EXISTS (
                    SELECT 1
                    FROM direct_exposure_servers direct_server
                    WHERE direct_server.consumer_id = client.identifier
                      AND direct_server.server_id = ?
                )
             OR EXISTS (
                    SELECT 1
                    FROM direct_exposure_refs direct_ref
                    JOIN capability_refs capability_ref ON capability_ref.ref_id = direct_ref.ref_id
                    WHERE direct_ref.consumer_id = client.identifier
                      AND direct_ref.enabled = 1
                      AND capability_ref.server_id = ?
                )
             OR EXISTS (
                    SELECT 1
                    FROM profile
                    LEFT JOIN profile_server_relationships profile_server
                      ON profile_server.profile_id = profile.id
                    LEFT JOIN profile_capability_refs profile_ref
                      ON profile_ref.profile_id = profile.id AND profile_ref.enabled = 1
                    LEFT JOIN capability_refs capability_ref ON capability_ref.ref_id = profile_ref.ref_id
                    WHERE (profile_server.server_id = ? OR capability_ref.server_id = ?)
                      AND (
                            client.custom_profile_id = profile.id
                         OR EXISTS (
                                SELECT 1 FROM json_each(client.selected_profile_ids)
                                WHERE json_each.value = profile.id
                            )
                         OR (client.capability_source = 'activated' AND profile.is_active = 1)
                      )
                )
          )
        ORDER BY client.identifier
        "#,
    )
    .bind(server_id)
    .bind(server_id)
    .bind(server_id)
    .bind(server_id)
    .fetch_all(&mut **transaction)
    .await?;
    Ok(filter_managed_consumers(rows, default_config_mode))
}

fn filter_managed_consumers(
    rows: Vec<(String, Option<String>)>,
    default_config_mode: &str,
) -> Vec<String> {
    rows.into_iter()
        .filter_map(|(consumer_id, config_mode)| {
            let effective_mode =
                crate::config::client::init::effective_client_config_mode(config_mode.as_deref(), default_config_mode);
            crate::config::client::init::is_managed_client_config_mode(effective_mode).then_some(consumer_id)
        })
        .collect()
}

impl ProfileSurfaceManagement {
    pub async fn delete_profile(
        pool: &Pool<Sqlite>,
        profile_id: &str,
        expected_authoring_generation: i64,
        actor: &str,
    ) -> Result<ProfileDeletionManagementResult> {
        let coordinator = MaterializationCoordinator::new(pool.clone());
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let default_config_mode = load_default_config_mode(pool).await?;
        let row = sqlx::query("SELECT name, is_default, role, authoring_generation FROM profile WHERE id = ?")
            .bind(profile_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or_else(|| CatalogError::InvalidSurfaceValue {
                field: "profile deletion",
                value: profile_id.to_string(),
            })?;
        let profile_name: String = row.try_get("name")?;
        let workflow_skill_name: Option<String> =
            sqlx::query_scalar("SELECT skill_name FROM workflow_profile_skills WHERE profile_id = ?")
                .bind(profile_id)
                .fetch_optional(&mut *transaction)
                .await?;
        let is_default: bool = row.try_get("is_default")?;
        let role: String = row.try_get("role")?;
        let current_authoring_generation: i64 = row.try_get("authoring_generation")?;
        if current_authoring_generation != expected_authoring_generation {
            return Err(CatalogError::ConcurrencyConflict {
                entity: "profile authoring generation",
                id: profile_id.to_string(),
            });
        }
        if is_default || role == "default_anchor" {
            return Err(CatalogError::InvalidSurfaceValue {
                field: "profile deletion",
                value: "default profiles cannot be deleted".to_string(),
            });
        }
        let consumer_ids = SurfaceAuthoringLoader::load_profile_consumer_ids_in_transaction(
            &mut transaction,
            profile_id,
            &default_config_mode,
        )
        .await?;
        let deleted = sqlx::query("DELETE FROM profile WHERE id = ? AND authoring_generation = ?")
            .bind(profile_id)
            .bind(expected_authoring_generation)
            .execute(&mut *transaction)
            .await?;
        if deleted.rows_affected() != 1 {
            return Err(CatalogError::ConcurrencyConflict {
                entity: "profile deletion",
                id: profile_id.to_string(),
            });
        }
        let trigger =
            MaterializationTrigger::for_consumer("profile_delete", format!("{profile_id}:{}", Uuid::new_v4()), actor);
        let mut materializations = Vec::with_capacity(consumer_ids.len());
        for consumer_id in consumer_ids {
            let commit = coordinator
                .compile_consumer_in_transaction_with_default(
                    &mut transaction,
                    &consumer_id,
                    &default_config_mode,
                    &trigger,
                )
                .await?;
            materializations.push(ConsumerMaterialization { consumer_id, commit });
        }
        transaction.commit().await?;
        Ok(ProfileDeletionManagementResult {
            profile_name,
            workflow_skill_name,
            materializations,
        })
    }

    pub async fn set_profiles_active(
        pool: &Pool<Sqlite>,
        profile_ids: &[String],
        action: ProfileActivationAction,
        expected_authoring_generations: HashMap<String, i64>,
        actor: &str,
    ) -> Result<ProfileActivationManagementResult> {
        let unique_profile_ids = profile_ids.iter().collect::<HashSet<_>>();
        if unique_profile_ids.len() != profile_ids.len() {
            return Err(CatalogError::InvalidSurfaceValue {
                field: "profile activation",
                value: "duplicate profile id".to_string(),
            });
        }
        let expected_profile_ids = expected_authoring_generations.keys().collect::<HashSet<_>>();
        if expected_profile_ids != unique_profile_ids {
            return Err(CatalogError::InvalidSurfaceValue {
                field: "profile authoring generations",
                value: "expected generation keys must exactly match Profile ids".to_string(),
            });
        }

        let coordinator = MaterializationCoordinator::new(pool.clone());
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let default_config_mode = load_default_config_mode(pool).await?;

        struct ActivationState {
            id: String,
            name: String,
            profile_mode: String,
            role: String,
            was_active: bool,
            is_active: bool,
            generation: i64,
        }

        let rows = sqlx::query(
            "SELECT id, name, profile_mode, role, is_active, authoring_generation FROM profile ORDER BY id",
        )
        .fetch_all(&mut *transaction)
        .await?;
        let mut states = rows
            .into_iter()
            .map(|row| -> std::result::Result<_, sqlx::Error> {
                let is_active = row.try_get("is_active")?;
                Ok(ActivationState {
                    id: row.try_get("id")?,
                    name: row.try_get("name")?,
                    profile_mode: row.try_get("profile_mode")?,
                    role: row.try_get("role")?,
                    was_active: is_active,
                    is_active,
                    generation: row.try_get("authoring_generation")?,
                })
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;

        for profile_id in profile_ids {
            let state = states.iter().find(|state| state.id == *profile_id).ok_or_else(|| {
                CatalogError::InvalidSurfaceValue {
                    field: "profile activation",
                    value: profile_id.clone(),
                }
            })?;
            if state.generation != expected_authoring_generations[profile_id] {
                return Err(CatalogError::ConcurrencyConflict {
                    entity: "profile authoring generation",
                    id: profile_id.clone(),
                });
            }
            if action == ProfileActivationAction::Deactivate && state.role == "default_anchor" {
                return Err(CatalogError::InvalidSurfaceValue {
                    field: "profile activation",
                    value: "default anchor cannot be deactivated".to_string(),
                });
            }
        }

        for profile_id in profile_ids {
            let target_index = states
                .iter()
                .position(|state| state.id == *profile_id)
                .expect("requested Profiles were prevalidated");
            states[target_index].is_active = action == ProfileActivationAction::Activate;
        }

        let requested_ids = profile_ids.iter().cloned().collect::<HashSet<_>>();
        for state in &states {
            if !requested_ids.contains(&state.id) && state.was_active == state.is_active {
                continue;
            }
            let updated = sqlx::query(
                r#"
                UPDATE profile
                SET is_active = ?,
                    authoring_generation = authoring_generation + 1,
                    updated_at = CURRENT_TIMESTAMP
                WHERE id = ? AND authoring_generation = ?
                "#,
            )
            .bind(state.is_active)
            .bind(&state.id)
            .bind(state.generation)
            .execute(&mut *transaction)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(CatalogError::ConcurrencyConflict {
                    entity: "profile authoring generation",
                    id: state.id.clone(),
                });
            }
        }

        let mutations = profile_ids
            .iter()
            .map(|profile_id| {
                let state = states
                    .iter()
                    .find(|state| state.id == *profile_id)
                    .expect("requested Profiles were prevalidated");
                ProfileActivationMutation {
                    profile_id: profile_id.clone(),
                    name: state.name.clone(),
                    is_active: state.is_active,
                }
            })
            .collect();

        let workflow_changed = profile_ids.iter().any(|profile_id| {
            states
                .iter()
                .any(|state| state.id == *profile_id && state.profile_mode == "workflow")
        });
        let capability_changed = profile_ids.iter().any(|profile_id| {
            states
                .iter()
                .any(|state| state.id == *profile_id && state.profile_mode != "workflow")
        });
        let mut consumer_ids = Vec::new();
        if capability_changed {
            consumer_ids.extend(
                SurfaceAuthoringLoader::load_activated_consumer_ids_in_transaction(
                    &mut transaction,
                    &default_config_mode,
                )
                .await?,
            );
        }
        if workflow_changed {
            consumer_ids.extend(
                crate::core::profile::publication::load_unify_consumer_ids_in_transaction(
                    &mut transaction,
                    &default_config_mode,
                )
                .await
                .map_err(|error| CatalogError::InvalidSurfaceValue {
                    field: "unify consumers",
                    value: error.to_string(),
                })?,
            );
        }
        consumer_ids.sort();
        consumer_ids.dedup();
        let trigger =
            MaterializationTrigger::for_consumer("profile_activation_save", Uuid::new_v4().to_string(), actor);
        let mut materializations = Vec::with_capacity(consumer_ids.len());
        for consumer_id in consumer_ids {
            let commit = coordinator
                .compile_consumer_in_transaction_with_default(
                    &mut transaction,
                    &consumer_id,
                    &default_config_mode,
                    &trigger,
                )
                .await?;
            materializations.push(ConsumerMaterialization { consumer_id, commit });
        }
        transaction.commit().await?;
        Ok(ProfileActivationManagementResult {
            mutations,
            materializations,
        })
    }

    pub async fn mutate_capabilities(
        pool: &Pool<Sqlite>,
        profile_id: &str,
        ref_ids: &[String],
        action: ProfileRelationshipAction,
        expected_authoring_generation: i64,
        source_revision_set: HashMap<String, i64>,
        actor: &str,
    ) -> Result<ProfileCapabilityManagementResult> {
        let unique_ref_ids = ref_ids.iter().collect::<HashSet<_>>();
        if unique_ref_ids.len() != ref_ids.len() {
            return Err(CatalogError::InvalidSurfaceValue {
                field: "profile capability refs",
                value: "duplicate ref id".to_string(),
            });
        }

        let coordinator = MaterializationCoordinator::new(pool.clone());
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let default_config_mode = load_default_config_mode(pool).await?;
        verify_profile_capability_dependencies(&mut transaction, ref_ids, &source_revision_set).await?;
        advance_profile_generation_in_transaction(&mut transaction, profile_id, expected_authoring_generation).await?;

        let mut mutations = Vec::with_capacity(ref_ids.len());
        for ref_id in ref_ids {
            let row = sqlx::query("SELECT kind, server_id FROM capability_refs WHERE ref_id = ?")
                .bind(ref_id)
                .fetch_optional(&mut *transaction)
                .await?
                .ok_or_else(|| CatalogError::InvalidSurfaceValue {
                    field: "profile capability ref",
                    value: ref_id.clone(),
                })?;
            let kind_value: String = row.try_get("kind")?;
            let kind = CapabilityKind::parse(&kind_value).ok_or_else(|| CatalogError::InvalidSurfaceValue {
                field: "capability kind",
                value: kind_value,
            })?;
            let server_id: String = row.try_get("server_id")?;
            if action == ProfileRelationshipAction::Enable {
                let server_enabled: Option<bool> = sqlx::query_scalar(
                    r#"
                    SELECT enabled
                    FROM profile_server_relationships
                    WHERE profile_id = ? AND server_id = ?
                    "#,
                )
                .bind(profile_id)
                .bind(&server_id)
                .fetch_optional(&mut *transaction)
                .await?;
                if server_enabled != Some(true) {
                    sqlx::query(
                        r#"
                        INSERT INTO profile_capability_refs (profile_id, ref_id, enabled)
                        SELECT ?, ref_id, 0
                        FROM capability_refs
                        WHERE server_id = ? AND state = 'active'
                        ON CONFLICT(profile_id, ref_id) DO NOTHING
                        "#,
                    )
                    .bind(profile_id)
                    .bind(&server_id)
                    .execute(&mut *transaction)
                    .await?;
                }
            }
            let updated = match action {
                ProfileRelationshipAction::Enable | ProfileRelationshipAction::Disable => {
                    sqlx::query(
                        r#"
                        INSERT INTO profile_capability_refs (profile_id, ref_id, enabled)
                        VALUES (?, ?, ?)
                        ON CONFLICT(profile_id, ref_id) DO UPDATE SET enabled = excluded.enabled
                        "#,
                    )
                    .bind(profile_id)
                    .bind(ref_id)
                    .bind(action == ProfileRelationshipAction::Enable)
                    .execute(&mut *transaction)
                    .await?
                }
                ProfileRelationshipAction::Remove => {
                    sqlx::query("DELETE FROM profile_capability_refs WHERE profile_id = ? AND ref_id = ?")
                        .bind(profile_id)
                        .bind(ref_id)
                        .execute(&mut *transaction)
                        .await?
                }
            };
            if updated.rows_affected() != 1 {
                return Err(CatalogError::ConcurrencyConflict {
                    entity: "profile capability relationship",
                    id: format!("{profile_id}/{ref_id}"),
                });
            }
            match action {
                ProfileRelationshipAction::Enable => {
                    sqlx::query(
                        r#"
                        INSERT INTO profile_server_relationships (profile_id, server_id, enabled, new_ref_policy)
                        VALUES (?, ?, 1, 'follow')
                        ON CONFLICT(profile_id, server_id) DO UPDATE SET enabled = 1
                        "#,
                    )
                    .bind(profile_id)
                    .bind(&server_id)
                    .execute(&mut *transaction)
                    .await?;
                }
                ProfileRelationshipAction::Disable => {
                    let has_enabled_capabilities: bool = sqlx::query_scalar(
                        r#"
                        SELECT EXISTS(
                            SELECT 1
                            FROM capability_refs capability_ref
                            LEFT JOIN profile_capability_refs profile_ref
                              ON profile_ref.profile_id = ?
                             AND profile_ref.ref_id = capability_ref.ref_id
                            LEFT JOIN profile_server_relationships profile_server
                              ON profile_server.profile_id = ?
                             AND profile_server.server_id = capability_ref.server_id
                            WHERE capability_ref.server_id = ?
                              AND capability_ref.state = 'active'
                              AND CASE
                                      WHEN profile_ref.ref_id IS NULL THEN COALESCE(profile_server.enabled, 0)
                                      ELSE profile_ref.enabled
                                  END = 1
                        )
                        "#,
                    )
                    .bind(profile_id)
                    .bind(profile_id)
                    .bind(&server_id)
                    .fetch_one(&mut *transaction)
                    .await?;
                    if !has_enabled_capabilities {
                        sqlx::query(
                            r#"
                            UPDATE profile_server_relationships
                            SET enabled = 0
                            WHERE profile_id = ? AND server_id = ?
                            "#,
                        )
                        .bind(profile_id)
                        .bind(&server_id)
                        .execute(&mut *transaction)
                        .await?;
                    }
                }
                ProfileRelationshipAction::Remove => {}
            }
            mutations.push(ProfileCapabilityMutation {
                ref_id: ref_id.clone(),
                kind,
            });
        }

        let materializations = Self::materialize_profile_consumers(
            &coordinator,
            &mut transaction,
            profile_id,
            &default_config_mode,
            actor,
            "profile_capability_save",
        )
        .await?;
        transaction.commit().await?;
        Ok(ProfileCapabilityManagementResult {
            mutations,
            materializations,
        })
    }

    pub async fn mutate_server(
        pool: &Pool<Sqlite>,
        profile_id: &str,
        server_id: &str,
        action: ProfileRelationshipAction,
        expected_authoring_generation: i64,
        actor: &str,
    ) -> Result<Vec<ConsumerMaterialization>> {
        Self::mutate_servers(
            pool,
            profile_id,
            &[server_id.to_string()],
            action,
            expected_authoring_generation,
            actor,
        )
        .await
    }

    pub async fn mutate_servers(
        pool: &Pool<Sqlite>,
        profile_id: &str,
        server_ids: &[String],
        action: ProfileRelationshipAction,
        expected_authoring_generation: i64,
        actor: &str,
    ) -> Result<Vec<ConsumerMaterialization>> {
        let unique_server_ids = server_ids.iter().collect::<HashSet<_>>();
        if unique_server_ids.len() != server_ids.len() {
            return Err(CatalogError::InvalidSurfaceValue {
                field: "profile servers",
                value: "duplicate server id".to_string(),
            });
        }
        let coordinator = MaterializationCoordinator::new(pool.clone());
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let default_config_mode = load_default_config_mode(pool).await?;
        advance_profile_generation_in_transaction(&mut transaction, profile_id, expected_authoring_generation).await?;
        for server_id in server_ids {
            let server_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM server_config WHERE id = ?)")
                .bind(server_id)
                .fetch_one(&mut *transaction)
                .await?;
            if !server_exists {
                return Err(CatalogError::InvalidSurfaceValue {
                    field: "profile server",
                    value: server_id.clone(),
                });
            }
            match action {
                ProfileRelationshipAction::Enable => {
                    sqlx::query(
                        r#"
                        INSERT INTO profile_server_relationships (profile_id, server_id, enabled, new_ref_policy)
                        VALUES (?, ?, 1, 'follow')
                        ON CONFLICT(profile_id, server_id) DO UPDATE SET enabled = 1
                        "#,
                    )
                    .bind(profile_id)
                    .bind(server_id)
                    .execute(&mut *transaction)
                    .await?;
                    sqlx::query(
                        r#"
                        INSERT INTO profile_capability_refs (profile_id, ref_id, enabled)
                        SELECT ?, ref_id, 1
                        FROM capability_refs
                        WHERE server_id = ? AND state = 'active'
                        ON CONFLICT(profile_id, ref_id) DO UPDATE SET enabled = 1
                        "#,
                    )
                    .bind(profile_id)
                    .bind(server_id)
                    .execute(&mut *transaction)
                    .await?;
                }
                ProfileRelationshipAction::Disable => {
                    let updated = sqlx::query(
                        "UPDATE profile_server_relationships SET enabled = 0 WHERE profile_id = ? AND server_id = ?",
                    )
                    .bind(profile_id)
                    .bind(server_id)
                    .execute(&mut *transaction)
                    .await?;
                    if updated.rows_affected() != 1 {
                        return Err(CatalogError::ConcurrencyConflict {
                            entity: "profile server relationship",
                            id: profile_id.to_string(),
                        });
                    }
                    sqlx::query(
                        r#"
                        UPDATE profile_capability_refs
                        SET enabled = 0
                        WHERE profile_id = ?
                          AND ref_id IN (SELECT ref_id FROM capability_refs WHERE server_id = ?)
                        "#,
                    )
                    .bind(profile_id)
                    .bind(server_id)
                    .execute(&mut *transaction)
                    .await?;
                    sqlx::query(
                        r#"
                        INSERT INTO profile_capability_refs (profile_id, ref_id, enabled)
                        SELECT ?, ref_id, 0
                        FROM capability_refs
                        WHERE server_id = ? AND state = 'active'
                        ON CONFLICT(profile_id, ref_id) DO UPDATE SET enabled = 0
                        "#,
                    )
                    .bind(profile_id)
                    .bind(server_id)
                    .execute(&mut *transaction)
                    .await?;
                }
                ProfileRelationshipAction::Remove => {
                    sqlx::query(
                        r#"
                        DELETE FROM profile_capability_refs
                        WHERE profile_id = ?
                          AND ref_id IN (SELECT ref_id FROM capability_refs WHERE server_id = ?)
                        "#,
                    )
                    .bind(profile_id)
                    .bind(server_id)
                    .execute(&mut *transaction)
                    .await?;
                    let deleted =
                        sqlx::query("DELETE FROM profile_server_relationships WHERE profile_id = ? AND server_id = ?")
                            .bind(profile_id)
                            .bind(server_id)
                            .execute(&mut *transaction)
                            .await?;
                    if deleted.rows_affected() != 1 {
                        return Err(CatalogError::ConcurrencyConflict {
                            entity: "profile server relationship",
                            id: profile_id.to_string(),
                        });
                    }
                }
            }
        }

        let materializations = Self::materialize_profile_consumers(
            &coordinator,
            &mut transaction,
            profile_id,
            &default_config_mode,
            actor,
            "profile_server_save",
        )
        .await?;
        transaction.commit().await?;
        Ok(materializations)
    }

    pub async fn replace_servers(
        pool: &Pool<Sqlite>,
        profile_id: &str,
        server_ids: &[String],
        expected_authoring_generation: i64,
        actor: &str,
    ) -> Result<Vec<ConsumerMaterialization>> {
        let unique_server_ids = server_ids.iter().collect::<HashSet<_>>();
        if unique_server_ids.len() != server_ids.len() {
            return Err(CatalogError::InvalidSurfaceValue {
                field: "profile servers",
                value: "duplicate server id".to_string(),
            });
        }
        let coordinator = MaterializationCoordinator::new(pool.clone());
        let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
        let default_config_mode = load_default_config_mode(pool).await?;
        advance_profile_generation_in_transaction(&mut transaction, profile_id, expected_authoring_generation).await?;
        for server_id in server_ids {
            let server_exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM server_config WHERE id = ?)")
                .bind(server_id)
                .fetch_one(&mut *transaction)
                .await?;
            if !server_exists {
                return Err(CatalogError::InvalidSurfaceValue {
                    field: "profile server",
                    value: server_id.clone(),
                });
            }
        }
        let existing_server_ids: Vec<String> =
            sqlx::query_scalar("SELECT server_id FROM profile_server_relationships WHERE profile_id = ?")
                .bind(profile_id)
                .fetch_all(&mut *transaction)
                .await?;
        for existing_server_id in existing_server_ids {
            if unique_server_ids.contains(&existing_server_id) {
                continue;
            }
            sqlx::query(
                r#"
                DELETE FROM profile_capability_refs
                WHERE profile_id = ?
                  AND ref_id IN (SELECT ref_id FROM capability_refs WHERE server_id = ?)
                "#,
            )
            .bind(profile_id)
            .bind(&existing_server_id)
            .execute(&mut *transaction)
            .await?;
            sqlx::query("DELETE FROM profile_server_relationships WHERE profile_id = ? AND server_id = ?")
                .bind(profile_id)
                .bind(&existing_server_id)
                .execute(&mut *transaction)
                .await?;
        }
        for server_id in server_ids {
            sqlx::query(
                r#"
                INSERT INTO profile_server_relationships (profile_id, server_id, enabled, new_ref_policy)
                VALUES (?, ?, 1, 'follow')
                ON CONFLICT(profile_id, server_id) DO NOTHING
                "#,
            )
            .bind(profile_id)
            .bind(server_id)
            .execute(&mut *transaction)
            .await?;
        }
        let materializations = Self::materialize_profile_consumers(
            &coordinator,
            &mut transaction,
            profile_id,
            &default_config_mode,
            actor,
            "profile_server_save",
        )
        .await?;
        transaction.commit().await?;
        Ok(materializations)
    }

    async fn materialize_profile_consumers(
        coordinator: &MaterializationCoordinator,
        transaction: &mut sqlx::Transaction<'_, Sqlite>,
        profile_id: &str,
        default_config_mode: &str,
        actor: &str,
        trigger_kind: &str,
    ) -> Result<Vec<ConsumerMaterialization>> {
        let consumer_ids = SurfaceAuthoringLoader::load_profile_consumer_ids_in_transaction(
            transaction,
            profile_id,
            default_config_mode,
        )
        .await?;
        let trigger =
            MaterializationTrigger::for_consumer(trigger_kind, format!("{profile_id}:{}", Uuid::new_v4()), actor);
        let mut commits = Vec::with_capacity(consumer_ids.len());
        for consumer_id in consumer_ids {
            let commit = coordinator
                .compile_consumer_in_transaction_with_default(transaction, &consumer_id, default_config_mode, &trigger)
                .await?;
            commits.push(ConsumerMaterialization { consumer_id, commit });
        }
        Ok(commits)
    }
}

pub(crate) async fn advance_profile_generation_in_transaction(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    profile_id: &str,
    expected_authoring_generation: i64,
) -> Result<i64> {
    let next_generation: Option<i64> = sqlx::query_scalar(
        r#"
        UPDATE profile
        SET authoring_generation = authoring_generation + 1,
            updated_at = CURRENT_TIMESTAMP
        WHERE id = ? AND authoring_generation = ?
        RETURNING authoring_generation
        "#,
    )
    .bind(profile_id)
    .bind(expected_authoring_generation)
    .fetch_optional(&mut **transaction)
    .await?;
    match next_generation {
        Some(generation) => Ok(generation),
        None => {
            let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profile WHERE id = ?)")
                .bind(profile_id)
                .fetch_one(&mut **transaction)
                .await?;
            if exists {
                Err(CatalogError::ConcurrencyConflict {
                    entity: "profile authoring generation",
                    id: profile_id.to_string(),
                })
            } else {
                Err(CatalogError::InvalidSurfaceValue {
                    field: "profile",
                    value: profile_id.to_string(),
                })
            }
        }
    }
}

async fn verify_profile_capability_dependencies(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    ref_ids: &[String],
    expected: &HashMap<String, i64>,
) -> Result<()> {
    let mut related_server_ids = BTreeSet::new();
    for ref_id in ref_ids {
        if let Some(server_id) =
            sqlx::query_scalar::<_, String>("SELECT server_id FROM capability_refs WHERE ref_id = ?")
                .bind(ref_id)
                .fetch_optional(&mut **transaction)
                .await?
        {
            related_server_ids.insert(server_id);
        }
    }
    let expected_server_ids = expected.keys().cloned().collect::<BTreeSet<_>>();
    if expected_server_ids != related_server_ids {
        return Err(CatalogError::InvalidSurfaceValue {
            field: "profile catalog dependency revisions",
            value: "dependency Server ids must exactly match the selected capabilities".to_string(),
        });
    }
    for server_id in &related_server_ids {
        let current: Option<i64> =
            sqlx::query_scalar("SELECT catalog_revision FROM capability_server_snapshots WHERE server_id = ?")
                .bind(server_id)
                .fetch_optional(&mut **transaction)
                .await?;
        if current != expected.get(server_id).copied() {
            return Err(CatalogError::ConcurrencyConflict {
                entity: "profile catalog dependency revisions",
                id: related_server_ids.iter().cloned().collect::<Vec<_>>().join(","),
            });
        }
    }
    Ok(())
}
