use std::collections::{BTreeSet, HashMap, HashSet};

use mcpmate_capability_store::{
    BUILTIN_CAPABILITY_SOURCE_ID, CapabilityChangeEvent, CapabilityId, CapabilityKind, CapabilityObservation,
    CapabilityRefId, CatalogCommit, CatalogError, CatalogRecord, ConsumerSurfaceBinding, DeclarationState,
    InventoryState, KindObservation, Result, ReviewResolutionAction, ReviewTargetKey, SqliteCapabilityCatalog,
    SqliteSurfaceStore, SurfaceManifest, SurfaceManifestEntryInput, SurfaceOutboxEvent, SurfaceProposal,
    SurfacePublication, SurfaceReviewItemDraft, SurfaceReviewOwner,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{Pool, Row, Sqlite, Transaction};
use uuid::Uuid;

use super::change_policy::{ChangeClass, NewRefPolicy, PolicyAction, RelationshipLevel, policy_action};
use super::dependency::CatalogDependencyRevisions;
use super::mode_policy::{
    DirectExposurePolicy, EffectiveConfigMode, ProfileScopePolicy, SurfaceParticipation,
    resolve_surface_composition_policy,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthoringRelationship {
    pub owner: SurfaceReviewOwner,
    pub ref_id: CapabilityRefId,
    pub kind: CapabilityKind,
    pub external_key: String,
    pub level: RelationshipLevel,
    pub new_ref_policy: NewRefPolicy,
}

impl AuthoringRelationship {
    pub fn new(
        owner: SurfaceReviewOwner,
        ref_id: CapabilityRefId,
        kind: CapabilityKind,
        external_key: impl Into<String>,
        level: RelationshipLevel,
        new_ref_policy: NewRefPolicy,
    ) -> Self {
        Self {
            owner,
            ref_id,
            kind,
            external_key: external_key.into(),
            level,
            new_ref_policy,
        }
    }

    pub fn builtin(
        owner_id: impl Into<String>,
        record: &mcpmate_capability_store::CatalogRecord,
    ) -> Self {
        Self::new(
            SurfaceReviewOwner::new(mcpmate_capability_store::ReviewOwnerType::ModeRule, owner_id),
            record.ref_id.clone(),
            record.kind(),
            record.external_key.clone(),
            RelationshipLevel::Builtin,
            NewRefPolicy::Follow,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CatalogTargetState {
    Active(CapabilityId),
    Unresolved,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CatalogTarget {
    pub ref_id: CapabilityRefId,
    pub state: CatalogTargetState,
    pub state_generation: i64,
    pub change_class: ChangeClass,
}

impl CatalogTarget {
    pub fn active(
        ref_id: CapabilityRefId,
        capability_id: CapabilityId,
        state_generation: i64,
        change_class: ChangeClass,
    ) -> Self {
        Self {
            ref_id,
            state: CatalogTargetState::Active(capability_id),
            state_generation,
            change_class,
        }
    }

    pub fn unresolved(
        ref_id: CapabilityRefId,
        state_generation: i64,
    ) -> Self {
        Self {
            ref_id,
            state: CatalogTargetState::Unresolved,
            state_generation,
            change_class: ChangeClass::Missing,
        }
    }

    fn target_key(&self) -> ReviewTargetKey {
        match &self.state {
            CatalogTargetState::Active(capability_id) if self.change_class == ChangeClass::Reappeared => {
                ReviewTargetKey::reappeared(capability_id, self.state_generation)
            }
            CatalogTargetState::Active(capability_id) => ReviewTargetKey::capability(capability_id),
            CatalogTargetState::Unresolved => ReviewTargetKey::missing(self.state_generation),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewDecisionState {
    pub ref_id: CapabilityRefId,
    pub target_key: ReviewTargetKey,
    pub resolution_action: ReviewResolutionAction,
}

impl ReviewDecisionState {
    pub fn new(
        ref_id: CapabilityRefId,
        capability_id: CapabilityId,
        state_generation: i64,
        resolution_action: ReviewResolutionAction,
    ) -> Self {
        let _ = state_generation;
        Self {
            ref_id,
            target_key: ReviewTargetKey::capability(&capability_id),
            resolution_action,
        }
    }

    pub fn for_target(
        ref_id: CapabilityRefId,
        target_key: ReviewTargetKey,
        resolution_action: ReviewResolutionAction,
    ) -> Self {
        Self {
            ref_id,
            target_key,
            resolution_action,
        }
    }
}

#[derive(Clone, Debug)]
pub struct MaterializationInput {
    pub consumer_id: String,
    pub relationships: Vec<AuthoringRelationship>,
    pub catalog_targets: Vec<CatalogTarget>,
    pub decisions: Vec<ReviewDecisionState>,
    pub dependency_server_ids: BTreeSet<String>,
}

impl MaterializationInput {
    pub fn new(
        consumer_id: impl Into<String>,
        relationships: Vec<AuthoringRelationship>,
        catalog_targets: Vec<CatalogTarget>,
        decisions: Vec<ReviewDecisionState>,
        dependency_server_ids: BTreeSet<String>,
    ) -> Self {
        Self {
            consumer_id: consumer_id.into(),
            relationships,
            catalog_targets,
            decisions,
            dependency_server_ids,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewCandidate {
    pub ref_id: CapabilityRefId,
    pub target_capability_id: Option<CapabilityId>,
    pub target_key: ReviewTargetKey,
    pub change_class: ChangeClass,
    pub policy_action: PolicyAction,
    pub owners: Vec<SurfaceReviewOwner>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewOwnerState {
    pub ref_id: CapabilityRefId,
    pub target_key: ReviewTargetKey,
    pub owners: Vec<SurfaceReviewOwner>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeEventCandidate {
    pub ref_id: CapabilityRefId,
    pub target_capability_id: Option<CapabilityId>,
    pub target_key: ReviewTargetKey,
    pub change_class: ChangeClass,
    pub policy_action: PolicyAction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationOutput {
    pub proposed_manifest: SurfaceManifest,
    pub publishable_manifest: SurfaceManifest,
    pub review_candidates: Vec<ReviewCandidate>,
    pub review_owner_states: Vec<ReviewOwnerState>,
    pub change_event_candidates: Vec<ChangeEventCandidate>,
}

pub struct SurfaceMaterializer;

impl SurfaceMaterializer {
    pub fn compile(input: MaterializationInput) -> Result<MaterializationOutput> {
        let mut grouped = HashMap::<CapabilityRefId, Vec<AuthoringRelationship>>::new();
        for relationship in input.relationships {
            grouped
                .entry(relationship.ref_id.clone())
                .or_default()
                .push(relationship);
        }
        let targets = input
            .catalog_targets
            .into_iter()
            .map(|target| (target.ref_id.clone(), target))
            .collect::<HashMap<_, _>>();
        let decisions = input
            .decisions
            .into_iter()
            .map(|decision| {
                (
                    (decision.ref_id.clone(), decision.target_key.clone()),
                    decision.resolution_action,
                )
            })
            .collect::<HashMap<_, _>>();

        let mut proposed_entries = Vec::new();
        let mut publishable_entries = Vec::new();
        let mut review_candidates = Vec::new();
        let mut review_owner_states = Vec::new();
        let mut change_event_candidates = Vec::new();

        for (ref_id, relationships) in grouped {
            let Some(target) = targets.get(&ref_id) else {
                return Err(CatalogError::SurfaceNotFound {
                    entity: "catalog target",
                    id: ref_id.to_string(),
                });
            };
            let first = relationships
                .first()
                .expect("grouped authoring relationships cannot be empty");
            if relationships
                .iter()
                .any(|relationship| relationship.kind != first.kind || relationship.external_key != first.external_key)
            {
                return Err(CatalogError::IntegrityMismatch {
                    identity: ref_id.to_string(),
                });
            }
            let action = relationships
                .iter()
                .map(|relationship| policy_action(target.change_class, relationship.level, relationship.new_ref_policy))
                .max()
                .expect("grouped authoring relationships cannot be empty");
            let mut owners = relationships
                .iter()
                .map(|relationship| relationship.owner.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            owners.sort_by(|left, right| {
                left.owner_type
                    .as_str()
                    .cmp(right.owner_type.as_str())
                    .then(left.owner_id.cmp(&right.owner_id))
            });
            let target_key = target.target_key();
            let decision = decisions.get(&(ref_id.clone(), target_key.clone())).copied();
            if target.change_class != ChangeClass::Unchanged {
                change_event_candidates.push(ChangeEventCandidate {
                    ref_id: ref_id.clone(),
                    target_capability_id: match &target.state {
                        CatalogTargetState::Active(capability_id) => Some(capability_id.clone()),
                        CatalogTargetState::Unresolved => None,
                    },
                    target_key: target_key.clone(),
                    change_class: target.change_class,
                    policy_action: action,
                });
            }

            if let CatalogTargetState::Active(capability_id) = &target.state {
                let entry = SurfaceManifestEntryInput::new(
                    ref_id.clone(),
                    capability_id.clone(),
                    first.kind,
                    &first.external_key,
                );
                proposed_entries.push(entry.clone());
                let publish = match action {
                    PolicyAction::Record => target.change_class != ChangeClass::NewRef,
                    PolicyAction::Follow => true,
                    PolicyAction::Review => decision == Some(ReviewResolutionAction::ApproveTarget),
                    PolicyAction::ManualRebind => false,
                };
                if publish {
                    publishable_entries.push(entry);
                }
            }

            let requires_pending_review =
                matches!(action, PolicyAction::Review | PolicyAction::ManualRebind) && decision.is_none();
            if matches!(action, PolicyAction::Review | PolicyAction::ManualRebind) {
                review_owner_states.push(ReviewOwnerState {
                    ref_id: ref_id.clone(),
                    target_key: target_key.clone(),
                    owners: owners.clone(),
                });
            }
            if requires_pending_review {
                review_candidates.push(ReviewCandidate {
                    ref_id,
                    target_capability_id: match &target.state {
                        CatalogTargetState::Active(capability_id) => Some(capability_id.clone()),
                        CatalogTargetState::Unresolved => None,
                    },
                    target_key,
                    change_class: target.change_class,
                    policy_action: action,
                    owners,
                });
            }
        }

        review_candidates.sort_by(|left, right| left.ref_id.cmp(&right.ref_id));
        review_owner_states.sort_by(|left, right| left.ref_id.cmp(&right.ref_id));
        change_event_candidates.sort_by(|left, right| left.ref_id.cmp(&right.ref_id));
        Ok(MaterializationOutput {
            proposed_manifest: SurfaceManifest::compile(&input.consumer_id, proposed_entries)?,
            publishable_manifest: SurfaceManifest::compile(input.consumer_id, publishable_entries)?,
            review_candidates,
            review_owner_states,
            change_event_candidates,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileAuthoringOwner {
    pub profile_id: String,
    pub owner_type: mcpmate_capability_store::ReviewOwnerType,
}

impl ProfileAuthoringOwner {
    pub fn new(
        profile_id: impl Into<String>,
        owner_type: mcpmate_capability_store::ReviewOwnerType,
    ) -> Self {
        Self {
            profile_id: profile_id.into(),
            owner_type,
        }
    }
}

pub struct SurfaceAuthoringLoader;

impl SurfaceAuthoringLoader {
    pub async fn load_catalog_revision_set(pool: &Pool<Sqlite>) -> Result<HashMap<String, i64>> {
        Ok(sqlx::query_as::<_, (String, i64)>(
            "SELECT server_id, catalog_revision FROM capability_server_snapshots ORDER BY server_id",
        )
        .fetch_all(pool)
        .await?
        .into_iter()
        .collect())
    }

    pub async fn load_catalog_revision_set_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>
    ) -> Result<HashMap<String, i64>> {
        Ok(sqlx::query_as::<_, (String, i64)>(
            "SELECT server_id, catalog_revision FROM capability_server_snapshots ORDER BY server_id",
        )
        .fetch_all(&mut **transaction)
        .await?
        .into_iter()
        .collect())
    }

    pub async fn load_profile_consumer_ids_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        profile_id: &str,
        default_config_mode: &str,
    ) -> Result<Vec<String>> {
        let rows = sqlx::query_as::<_, (String, Option<String>)>(
            r#"
            SELECT DISTINCT client.identifier, client.config_mode
            FROM client
            LEFT JOIN profile ON profile.id = ?
            WHERE client.approval_status = 'approved'
              AND (
                    (client.capability_source = 'activated' AND profile.is_active = 1 AND profile.profile_mode != 'workflow')
                 OR (
                        client.capability_source = 'profiles'
                    AND EXISTS (
                        SELECT 1
                        FROM json_each(client.selected_profile_ids)
                        WHERE json_each.value = ?
                    )
                 )
                 OR (client.capability_source = 'custom' AND client.custom_profile_id = ?)
              )
            ORDER BY client.identifier
            "#,
        )
        .bind(profile_id)
        .bind(profile_id)
        .bind(profile_id)
        .fetch_all(&mut **transaction)
        .await?;
        Ok(filter_managed_consumers(rows, default_config_mode))
    }

    pub async fn load_activated_consumer_ids_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        default_config_mode: &str,
    ) -> Result<Vec<String>> {
        let rows = sqlx::query_as::<_, (String, Option<String>)>(
            r#"
            SELECT identifier, config_mode
            FROM client
            WHERE approval_status = 'approved'
              AND capability_source = 'activated'
            ORDER BY identifier
            "#,
        )
        .fetch_all(&mut **transaction)
        .await?;
        Ok(filter_managed_consumers(rows, default_config_mode))
    }

    pub async fn load_consumer_input_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        consumer_id: &str,
        default_config_mode: &str,
    ) -> Result<MaterializationInput> {
        Self::load_consumer_input_with_changes_in_transaction(
            transaction,
            consumer_id,
            default_config_mode,
            HashMap::new(),
        )
        .await
    }

    pub async fn load_consumer_input_with_changes_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        consumer_id: &str,
        default_config_mode: &str,
        mut changes: HashMap<CapabilityRefId, ChangeClass>,
    ) -> Result<MaterializationInput> {
        let row = sqlx::query(
            r#"
            SELECT config_mode, capability_source, selected_profile_ids, custom_profile_id, approval_status,
                   unify_route_mode
            FROM client
            WHERE identifier = ?
            "#,
        )
        .bind(consumer_id)
        .fetch_optional(&mut **transaction)
        .await?
        .ok_or_else(|| CatalogError::SurfaceNotFound {
            entity: "consumer",
            id: consumer_id.to_string(),
        })?;
        let approval_status: String = row.try_get("approval_status")?;
        let config_mode: Option<String> = row.try_get("config_mode")?;
        let effective_config_mode =
            crate::config::client::init::effective_client_config_mode(config_mode.as_deref(), default_config_mode);
        let effective_mode =
            EffectiveConfigMode::parse(effective_config_mode).ok_or_else(|| CatalogError::InvalidSurfaceValue {
                field: "effective config mode",
                value: effective_config_mode.to_string(),
            })?;
        let capability_source_value: String = row.try_get("capability_source")?;
        let capability_source = capability_source_value
            .parse()
            .map_err(|_| CatalogError::InvalidSurfaceValue {
                field: "capability source",
                value: capability_source_value,
            })?;
        let unify_route_mode_value: String = row.try_get("unify_route_mode")?;
        let unify_route_mode = unify_route_mode_value
            .parse()
            .map_err(|_| CatalogError::InvalidSurfaceValue {
                field: "unify route mode",
                value: unify_route_mode_value,
            })?;
        let composition_policy =
            resolve_surface_composition_policy(effective_mode, capability_source, unify_route_mode);
        if approval_status != "approved" || composition_policy.participation != SurfaceParticipation::Managed {
            return Err(CatalogError::InvalidSurfaceValue {
                field: "managed consumer state",
                value: format!("{approval_status}/{effective_config_mode}"),
            });
        }
        let selected_profile_ids: Option<String> = row.try_get("selected_profile_ids")?;
        let custom_profile_id: Option<String> = row.try_get("custom_profile_id")?;
        let profiles = match composition_policy.profile_scope {
            ProfileScopePolicy::Ignored => Vec::new(),
            ProfileScopePolicy::Activated => {
                let ids: Vec<String> =
                    sqlx::query_scalar(
                        "SELECT id FROM profile WHERE is_active = 1 AND profile_mode != 'workflow' ORDER BY priority DESC, id",
                    )
                        .fetch_all(&mut **transaction)
                        .await?;
                ids.into_iter()
                    .map(|id| {
                        ProfileAuthoringOwner::new(id, mcpmate_capability_store::ReviewOwnerType::StandardProfile)
                    })
                    .collect()
            }
            ProfileScopePolicy::Selected => {
                let raw_ids = selected_profile_ids.unwrap_or_else(|| "[]".to_string());
                let ids: Vec<String> = sqlx::query_scalar(
                    r#"
                    SELECT profile.id
                    FROM json_each(?) AS selected
                    JOIN profile ON profile.id = selected.value
                    WHERE profile.profile_mode != 'workflow'
                    "#,
                )
                .bind(raw_ids)
                .fetch_all(&mut **transaction)
                .await?;
                ids.into_iter()
                    .map(|id| {
                        ProfileAuthoringOwner::new(id, mcpmate_capability_store::ReviewOwnerType::StandardProfile)
                    })
                    .collect()
            }
            ProfileScopePolicy::Custom => vec![ProfileAuthoringOwner::new(
                custom_profile_id.ok_or_else(|| CatalogError::InvalidSurfaceValue {
                    field: "custom profile id",
                    value: consumer_id.to_string(),
                })?,
                mcpmate_capability_store::ReviewOwnerType::CustomProfile,
            )],
        };
        let mut relationships = Self::load_relationships_in_transaction(
            transaction,
            consumer_id,
            &profiles,
            composition_policy.direct_exposure,
            &[],
            None,
        )
        .await?;
        let allowed_names =
            crate::mcper::builtin::names::builtin_tool_names_for_surface_set(composition_policy.builtins);
        if !allowed_names.is_empty() {
            let builtin_rows = sqlx::query(
                r#"
                SELECT r.ref_id, r.kind, r.origin_key, v.canonical_record
                FROM capability_refs r
                JOIN capability_ref_current c ON c.ref_id = r.ref_id
                JOIN capability_versions v ON v.capability_id = c.capability_id
                WHERE r.server_id = ? AND r.state = 'active' AND r.kind = 'tools'
                ORDER BY r.kind, r.origin_key
                "#,
            )
            .bind(BUILTIN_CAPABILITY_SOURCE_ID)
            .fetch_all(&mut **transaction)
            .await?;
            for row in builtin_rows {
                let origin_key: String = row.try_get("origin_key")?;
                if !allowed_names.contains(&origin_key.as_str()) {
                    continue;
                }
                relationships.push(parse_authoring_row(
                    row,
                    SurfaceReviewOwner::new(
                        mcpmate_capability_store::ReviewOwnerType::ModeRule,
                        effective_config_mode,
                    ),
                    RelationshipLevel::Builtin,
                    NewRefPolicy::Follow,
                )?);
            }
        }
        let dependency_server_ids = Self::load_dependency_server_ids_in_transaction(
            transaction,
            consumer_id,
            &profiles,
            composition_policy.direct_exposure,
            &relationships,
        )
        .await?;

        let review_rows = sqlx::query(
            r#"
            SELECT item.ref_id, item.target_key, item.lifecycle, decision.resolution_action, item.change_class
            FROM surface_review_items item
            LEFT JOIN surface_review_decisions decision ON decision.decision_id = item.current_decision_id
            WHERE item.consumer_id = ? AND item.lifecycle <> 'obsolete'
            "#,
        )
        .bind(consumer_id)
        .fetch_all(&mut **transaction)
        .await?;
        let mut decisions = Vec::new();
        for row in review_rows {
            let ref_id: CapabilityRefId = row.try_get::<String, _>("ref_id")?.parse()?;
            let change_class_value: String = row.try_get("change_class")?;
            let change_class =
                ChangeClass::parse(&change_class_value).ok_or_else(|| CatalogError::InvalidSurfaceValue {
                    field: "change class",
                    value: change_class_value,
                })?;
            changes.entry(ref_id.clone()).or_insert(change_class);
            let lifecycle: String = row.try_get("lifecycle")?;
            if lifecycle == "resolved"
                && let Some(action) = row.try_get::<Option<String>, _>("resolution_action")?
            {
                decisions.push(ReviewDecisionState::for_target(
                    ref_id,
                    row.try_get::<String, _>("target_key")?.parse()?,
                    action.parse()?,
                ));
            }
        }
        let catalog_targets = Self::load_catalog_targets_in_transaction(transaction, &relationships, &changes).await?;
        Ok(MaterializationInput::new(
            consumer_id,
            relationships,
            catalog_targets,
            decisions,
            dependency_server_ids,
        ))
    }

    pub async fn load_relationships_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        consumer_id: &str,
        profiles: &[ProfileAuthoringOwner],
        direct_exposure_policy: DirectExposurePolicy,
        builtin_records: &[mcpmate_capability_store::CatalogRecord],
        builtin_owner_id: Option<&str>,
    ) -> Result<Vec<AuthoringRelationship>> {
        let mut relationships = Vec::new();
        for profile in profiles {
            let explicit = sqlx::query(
                r#"
                SELECT r.ref_id, r.kind, v.canonical_record
                FROM profile_capability_refs p
                JOIN capability_refs r ON r.ref_id = p.ref_id
                JOIN server_config server ON server.id = r.server_id AND server.enabled = 1
                LEFT JOIN profile_server_relationships profile_server
                  ON profile_server.profile_id = p.profile_id
                 AND profile_server.server_id = r.server_id
                LEFT JOIN capability_ref_current c ON c.ref_id = r.ref_id
                JOIN capability_versions v ON v.capability_id = COALESCE(
                    c.capability_id,
                    (
                        SELECT previous.capability_id
                        FROM capability_versions previous
                        WHERE previous.ref_id = r.ref_id
                        ORDER BY previous.first_observed_revision DESC, previous.capability_id
                        LIMIT 1
                    )
                )
                WHERE p.profile_id = ?
                  AND p.enabled = 1
                  AND r.state <> 'retired'
                  AND COALESCE(profile_server.enabled, 1) = 1
                "#,
            )
            .bind(&profile.profile_id)
            .fetch_all(&mut **transaction)
            .await?;
            for row in explicit {
                relationships.push(parse_authoring_row(
                    row,
                    SurfaceReviewOwner::new(profile.owner_type, &profile.profile_id),
                    RelationshipLevel::Capability,
                    NewRefPolicy::Review,
                )?);
            }
            let server_level = sqlx::query(
                r#"
                SELECT r.ref_id, r.kind, v.canonical_record, p.new_ref_policy
                FROM profile_server_relationships p
                JOIN capability_refs r ON r.server_id = p.server_id
                JOIN server_config server ON server.id = r.server_id AND server.enabled = 1
                LEFT JOIN capability_ref_current c ON c.ref_id = r.ref_id
                JOIN capability_versions v ON v.capability_id = COALESCE(
                    c.capability_id,
                    (
                        SELECT previous.capability_id
                        FROM capability_versions previous
                        WHERE previous.ref_id = r.ref_id
                        ORDER BY previous.first_observed_revision DESC, previous.capability_id
                        LIMIT 1
                    )
                )
                WHERE p.profile_id = ?
                  AND p.enabled = 1
                  AND r.state <> 'retired'
                  AND NOT EXISTS (
                      SELECT 1
                      FROM profile_capability_refs explicit
                      WHERE explicit.profile_id = p.profile_id
                        AND explicit.ref_id = r.ref_id
                  )
                "#,
            )
            .bind(&profile.profile_id)
            .fetch_all(&mut **transaction)
            .await?;
            for row in server_level {
                let policy: String = row.try_get("new_ref_policy")?;
                relationships.push(parse_authoring_row(
                    row,
                    SurfaceReviewOwner::new(
                        mcpmate_capability_store::ReviewOwnerType::ProfileServerExposure,
                        &profile.profile_id,
                    ),
                    RelationshipLevel::Server,
                    parse_new_ref_policy(&policy)?,
                )?);
            }
        }

        if direct_exposure_policy == DirectExposurePolicy::CapabilityLevel {
            let direct = sqlx::query(
                r#"
                SELECT r.ref_id, r.kind, v.canonical_record
                FROM direct_exposure_refs d
                JOIN capability_refs r ON r.ref_id = d.ref_id
                JOIN server_config server
                  ON server.id = r.server_id
                 AND server.enabled = 1
                 AND server.unify_direct_exposure_eligible = 1
                LEFT JOIN capability_ref_current c ON c.ref_id = r.ref_id
                JOIN capability_versions v ON v.capability_id = COALESCE(
                    c.capability_id,
                    (
                        SELECT previous.capability_id
                        FROM capability_versions previous
                        WHERE previous.ref_id = r.ref_id
                        ORDER BY previous.first_observed_revision DESC, previous.capability_id
                        LIMIT 1
                    )
                )
                WHERE d.consumer_id = ?
                  AND d.enabled = 1
                  AND r.state <> 'retired'
                "#,
            )
            .bind(consumer_id)
            .fetch_all(&mut **transaction)
            .await?;
            for row in direct {
                relationships.push(parse_authoring_row(
                    row,
                    SurfaceReviewOwner::new(
                        mcpmate_capability_store::ReviewOwnerType::ConsumerDirectExposure,
                        consumer_id,
                    ),
                    RelationshipLevel::Capability,
                    NewRefPolicy::Review,
                )?);
            }
        }

        if direct_exposure_policy == DirectExposurePolicy::ServerLevel {
            let direct_servers = sqlx::query(
                r#"
                SELECT r.ref_id, r.kind, v.canonical_record, d.new_ref_policy
                FROM direct_exposure_servers d
                JOIN capability_refs r ON r.server_id = d.server_id
                JOIN server_config server
                  ON server.id = r.server_id
                 AND server.enabled = 1
                 AND server.unify_direct_exposure_eligible = 1
                LEFT JOIN capability_ref_current c ON c.ref_id = r.ref_id
                JOIN capability_versions v ON v.capability_id = COALESCE(
                    c.capability_id,
                    (
                        SELECT previous.capability_id
                        FROM capability_versions previous
                        WHERE previous.ref_id = r.ref_id
                        ORDER BY previous.first_observed_revision DESC, previous.capability_id
                        LIMIT 1
                    )
                )
                WHERE d.consumer_id = ?
                  AND r.state <> 'retired'
                "#,
            )
            .bind(consumer_id)
            .fetch_all(&mut **transaction)
            .await?;
            for row in direct_servers {
                let policy: String = row.try_get("new_ref_policy")?;
                relationships.push(parse_authoring_row(
                    row,
                    SurfaceReviewOwner::new(
                        mcpmate_capability_store::ReviewOwnerType::ConsumerServerExposure,
                        consumer_id,
                    ),
                    RelationshipLevel::Server,
                    parse_new_ref_policy(&policy)?,
                )?);
            }
        }
        if let Some(owner_id) = builtin_owner_id {
            relationships.extend(
                builtin_records
                    .iter()
                    .map(|record| AuthoringRelationship::builtin(owner_id, record)),
            );
        }
        if matches!(
            direct_exposure_policy,
            DirectExposurePolicy::None | DirectExposurePolicy::ServerLevel | DirectExposurePolicy::CapabilityLevel
        ) {
            relationships.extend(load_published_workflow_direct_relationships(transaction).await?);
        }
        Ok(relationships)
    }

    async fn load_dependency_server_ids_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        consumer_id: &str,
        profiles: &[ProfileAuthoringOwner],
        direct_exposure_policy: DirectExposurePolicy,
        relationships: &[AuthoringRelationship],
    ) -> Result<BTreeSet<String>> {
        let mut server_ids = BTreeSet::new();
        let relationship_ref_ids = relationships
            .iter()
            .map(|relationship| relationship.ref_id.as_str())
            .collect::<BTreeSet<_>>();
        if !relationship_ref_ids.is_empty() {
            let ref_ids_json = serde_json::to_string(&relationship_ref_ids)?;
            let relationship_servers = sqlx::query_as::<_, (String, String)>(
                r#"
                    SELECT ref_id, server_id
                    FROM capability_refs
                    WHERE ref_id IN (SELECT value FROM json_each(?))
                    ORDER BY ref_id
                    "#,
            )
            .bind(ref_ids_json)
            .fetch_all(&mut **transaction)
            .await?
            .into_iter()
            .collect::<HashMap<_, _>>();
            for ref_id in relationship_ref_ids {
                let server_id = relationship_servers
                    .get(ref_id)
                    .ok_or_else(|| CatalogError::SurfaceNotFound {
                        entity: "capability ref",
                        id: ref_id.to_string(),
                    })?;
                server_ids.insert(server_id.clone());
            }
        }
        for profile in profiles {
            server_ids.extend(
                sqlx::query_scalar::<_, String>(
                    r#"
                    SELECT relationship.server_id
                    FROM profile_server_relationships relationship
                    JOIN server_config server
                      ON server.id = relationship.server_id
                     AND server.enabled = 1
                    WHERE relationship.profile_id = ?
                      AND relationship.enabled = 1
                    ORDER BY relationship.server_id
                    "#,
                )
                .bind(&profile.profile_id)
                .fetch_all(&mut **transaction)
                .await?,
            );
        }
        if direct_exposure_policy == DirectExposurePolicy::ServerLevel {
            server_ids.extend(
                sqlx::query_scalar::<_, String>(
                    r#"
                    SELECT exposure.server_id
                    FROM direct_exposure_servers exposure
                    JOIN server_config server
                      ON server.id = exposure.server_id
                     AND server.enabled = 1
                     AND server.unify_direct_exposure_eligible = 1
                    WHERE exposure.consumer_id = ?
                    ORDER BY exposure.server_id
                    "#,
                )
                .bind(consumer_id)
                .fetch_all(&mut **transaction)
                .await?,
            );
        }
        Ok(server_ids)
    }

    pub async fn load_catalog_targets_in_transaction(
        transaction: &mut Transaction<'_, Sqlite>,
        relationships: &[AuthoringRelationship],
        changes: &HashMap<CapabilityRefId, ChangeClass>,
    ) -> Result<Vec<CatalogTarget>> {
        let mut unique_refs = relationships
            .iter()
            .map(|relationship| relationship.ref_id.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        unique_refs.sort();
        let mut targets = Vec::with_capacity(unique_refs.len());
        for ref_id in unique_refs {
            let row = sqlx::query(
                r#"
                SELECT r.state, r.state_generation, c.capability_id
                FROM capability_refs r
                LEFT JOIN capability_ref_current c ON c.ref_id = r.ref_id
                WHERE r.ref_id = ?
                "#,
            )
            .bind(ref_id.as_str())
            .fetch_optional(&mut **transaction)
            .await?
            .ok_or_else(|| CatalogError::SurfaceNotFound {
                entity: "capability ref",
                id: ref_id.to_string(),
            })?;
            let state: String = row.try_get("state")?;
            let state_generation: i64 = row.try_get("state_generation")?;
            if state == "active" {
                let capability_id: String = row.try_get::<Option<String>, _>("capability_id")?.ok_or_else(|| {
                    CatalogError::SurfaceNotFound {
                        entity: "current capability",
                        id: ref_id.to_string(),
                    }
                })?;
                targets.push(CatalogTarget::active(
                    ref_id.clone(),
                    capability_id.parse()?,
                    state_generation,
                    changes.get(&ref_id).copied().unwrap_or(ChangeClass::Unchanged),
                ));
            } else if state == "unresolved" {
                targets.push(CatalogTarget::unresolved(ref_id, state_generation));
            } else {
                return Err(CatalogError::InvalidSurfaceValue {
                    field: "capability ref state",
                    value: state,
                });
            }
        }
        Ok(targets)
    }
}

fn parse_authoring_row(
    row: sqlx::sqlite::SqliteRow,
    owner: SurfaceReviewOwner,
    level: RelationshipLevel,
    new_ref_policy: NewRefPolicy,
) -> Result<AuthoringRelationship> {
    let ref_id: CapabilityRefId = row.try_get::<String, _>("ref_id")?.parse()?;
    let kind_value: String = row.try_get("kind")?;
    let kind = CapabilityKind::parse(&kind_value).ok_or_else(|| CatalogError::InvalidSurfaceValue {
        field: "capability kind",
        value: kind_value,
    })?;
    let canonical_record: Vec<u8> = row.try_get("canonical_record")?;
    let record: mcpmate_capability_store::EffectiveCapabilityRecordV1 = serde_json::from_slice(&canonical_record)?;
    record.validate()?;
    if record.ref_id != ref_id || record.definition.kind() != kind {
        return Err(CatalogError::IntegrityMismatch {
            identity: ref_id.to_string(),
        });
    }
    Ok(AuthoringRelationship::new(
        owner,
        ref_id,
        kind,
        record.definition.external_key(),
        level,
        new_ref_policy,
    ))
}

async fn load_published_workflow_direct_relationships(
    transaction: &mut Transaction<'_, Sqlite>
) -> Result<Vec<AuthoringRelationship>> {
    let directs = crate::core::profile::publication::load_published_workflow_direct_refs_in_transaction(transaction)
        .await
        .map_err(|error| CatalogError::InvalidSurfaceValue {
            field: "published workflow directs",
            value: error.to_string(),
        })?;
    if directs.is_empty() {
        return Ok(Vec::new());
    }
    let ref_ids = directs.iter().map(|item| item.ref_id.as_str()).collect::<Vec<_>>();
    let ref_ids_json = serde_json::to_string(&ref_ids)?;
    let rows = sqlx::query(
        r#"
        SELECT r.ref_id, r.kind, v.canonical_record
        FROM capability_refs r
        JOIN server_config server ON server.id = r.server_id AND server.enabled = 1
        LEFT JOIN capability_ref_current c ON c.ref_id = r.ref_id
        JOIN capability_versions v ON v.capability_id = COALESCE(
            c.capability_id,
            (
                SELECT previous.capability_id
                FROM capability_versions previous
                WHERE previous.ref_id = r.ref_id
                ORDER BY previous.first_observed_revision DESC, previous.capability_id
                LIMIT 1
            )
        )
        WHERE r.ref_id IN (SELECT value FROM json_each(?))
          AND r.state <> 'retired'
        "#,
    )
    .bind(ref_ids_json)
    .fetch_all(&mut **transaction)
    .await?;
    let mut relationships = Vec::with_capacity(rows.len());
    for row in rows {
        let ref_id: String = row.try_get("ref_id")?;
        let owner_id = directs
            .iter()
            .find(|item| item.ref_id == ref_id)
            .map(|item| item.profile_id.as_str())
            .unwrap_or("workflow");
        relationships.push(parse_authoring_row(
            row,
            SurfaceReviewOwner::new(mcpmate_capability_store::ReviewOwnerType::StandardProfile, owner_id),
            RelationshipLevel::Capability,
            NewRefPolicy::Review,
        )?);
    }
    Ok(relationships)
}

fn parse_new_ref_policy(value: &str) -> Result<NewRefPolicy> {
    match value {
        "follow" => Ok(NewRefPolicy::Follow),
        "review" => Ok(NewRefPolicy::Review),
        _ => Err(CatalogError::InvalidSurfaceValue {
            field: "new ref policy",
            value: value.to_string(),
        }),
    }
}

#[derive(Clone, Debug)]
pub struct MaterializationTrigger {
    pub kind: String,
    pub id: String,
    pub catalog_dependency_revisions: CatalogDependencyRevisions,
    pub actor: String,
    pub review_baseline_manifest_id: Option<mcpmate_capability_store::SurfaceManifestId>,
}

impl MaterializationTrigger {
    pub fn for_consumer(
        kind: impl Into<String>,
        id: impl Into<String>,
        actor: impl Into<String>,
    ) -> Self {
        Self::from_dependencies(kind, id, CatalogDependencyRevisions::default(), actor)
    }

    pub fn from_dependencies(
        kind: impl Into<String>,
        id: impl Into<String>,
        catalog_dependency_revisions: CatalogDependencyRevisions,
        actor: impl Into<String>,
    ) -> Self {
        Self {
            kind: kind.into(),
            id: id.into(),
            catalog_dependency_revisions,
            actor: actor.into(),
            review_baseline_manifest_id: None,
        }
    }

    pub fn with_review_baseline_manifest_id(
        mut self,
        manifest_id: mcpmate_capability_store::SurfaceManifestId,
    ) -> Self {
        self.review_baseline_manifest_id = Some(manifest_id);
        self
    }

    fn with_catalog_dependency_revisions(
        &self,
        catalog_dependency_revisions: CatalogDependencyRevisions,
    ) -> Self {
        let mut trigger = self.clone();
        trigger.catalog_dependency_revisions = catalog_dependency_revisions;
        trigger
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializationCommit {
    pub proposal_id: Option<String>,
    pub binding: Option<ConsumerSurfaceBinding>,
    pub review_item_ids: Vec<String>,
    pub effective_surface_changed: bool,
}

#[derive(Clone)]
pub struct MaterializationCoordinator {
    pool: Pool<Sqlite>,
    store: SqliteSurfaceStore,
}

impl MaterializationCoordinator {
    pub fn new(pool: Pool<Sqlite>) -> Self {
        Self {
            store: SqliteSurfaceStore::new(pool.clone()),
            pool,
        }
    }

    pub async fn persist(
        &self,
        output: &MaterializationOutput,
        trigger: &MaterializationTrigger,
    ) -> Result<MaterializationCommit> {
        let mut transaction = self.pool.begin().await?;
        let committed = self.persist_in_transaction(&mut transaction, output, trigger).await?;
        transaction.commit().await?;
        Ok(committed)
    }

    pub async fn persist_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        output: &MaterializationOutput,
        trigger: &MaterializationTrigger,
    ) -> Result<MaterializationCommit> {
        self.verify_catalog_dependency_revisions_in_transaction(transaction, &trigger.catalog_dependency_revisions)
            .await?;
        let consumer_id = &output.proposed_manifest.consumer_id;
        if output.publishable_manifest.consumer_id != *consumer_id {
            return Err(CatalogError::InvalidSurfaceValue {
                field: "materialization consumer_id",
                value: output.publishable_manifest.consumer_id.clone(),
            });
        }
        let binding = self.store.load_binding_in_transaction(transaction, consumer_id).await?;
        let active_manifest_id = match &binding {
            Some(binding) => Some(
                self.store
                    .load_publication_manifest_id_in_transaction(transaction, &binding.active_publication_id)
                    .await?,
            ),
            None => None,
        };
        let publishable_changed = active_manifest_id.as_ref() != Some(&output.publishable_manifest.manifest_id);
        let proposed_changed = active_manifest_id.as_ref() != Some(&output.proposed_manifest.manifest_id);
        if !publishable_changed
            && !proposed_changed
            && output.review_candidates.is_empty()
            && output.change_event_candidates.is_empty()
        {
            return Ok(MaterializationCommit {
                proposal_id: None,
                binding,
                review_item_ids: Vec::new(),
                effective_surface_changed: false,
            });
        }

        self.store
            .insert_manifest_in_transaction(transaction, &output.proposed_manifest)
            .await?;
        self.store
            .insert_manifest_in_transaction(transaction, &output.publishable_manifest)
            .await?;

        let proposal_id = surface_proposal_id(
            consumer_id,
            trigger,
            &output.proposed_manifest.manifest_id,
            &output.publishable_manifest.manifest_id,
        );
        let proposal = SurfaceProposal::new(
            &proposal_id,
            consumer_id,
            binding.as_ref().map(|current| current.active_publication_id.clone()),
            output.proposed_manifest.manifest_id.clone(),
            &trigger.kind,
            &trigger.id,
            serde_json::to_value(&trigger.catalog_dependency_revisions)?,
            json!({
                "proposedEntries": output.proposed_manifest.entries.len(),
                "publishableEntries": output.publishable_manifest.entries.len(),
                "reviewItems": output.review_candidates.len(),
            }),
        );
        self.store
            .insert_proposal_in_transaction(transaction, &proposal)
            .await?;

        let review_baseline_manifest_id = trigger
            .review_baseline_manifest_id
            .as_ref()
            .or(active_manifest_id.as_ref());
        for candidate in &output.change_event_candidates {
            let before_capability_id = match review_baseline_manifest_id {
                Some(manifest_id) => {
                    self.store
                        .load_manifest_entry_capability_in_transaction(transaction, manifest_id, &candidate.ref_id)
                        .await?
                }
                None => None,
            };
            self.store
                .insert_capability_change_event_in_transaction(
                    transaction,
                    &CapabilityChangeEvent::new(
                        capability_change_event_id(
                            consumer_id,
                            &trigger.kind,
                            &trigger.id,
                            &candidate.ref_id,
                            &candidate.target_key,
                            candidate.target_capability_id.as_ref(),
                            candidate.change_class,
                            candidate.policy_action,
                        ),
                        consumer_id,
                        &proposal_id,
                        candidate.ref_id.clone(),
                        before_capability_id,
                        candidate.target_capability_id.clone(),
                        candidate.change_class.as_str(),
                        candidate.policy_action.as_str(),
                        &trigger.actor,
                    ),
                )
                .await?;
        }

        let mut review_item_ids = Vec::with_capacity(output.review_candidates.len());
        for candidate in &output.review_candidates {
            let review_item_id = format!("review-{}", Uuid::new_v4());
            let before_capability_id = match review_baseline_manifest_id {
                Some(manifest_id) => {
                    self.store
                        .load_manifest_entry_capability_in_transaction(transaction, manifest_id, &candidate.ref_id)
                        .await?
                }
                None => None,
            };
            let draft = SurfaceReviewItemDraft::new(
                &review_item_id,
                &proposal_id,
                consumer_id,
                candidate.ref_id.clone(),
                before_capability_id,
                candidate.target_capability_id.clone(),
                candidate.target_key.clone(),
                candidate.change_class.as_str(),
                candidate.policy_action.as_str(),
            );
            let item = self
                .store
                .create_or_reuse_review_item_in_transaction(transaction, &draft, &candidate.owners)
                .await?;
            review_item_ids.push(item.review_item_id);
        }
        review_item_ids.sort();
        review_item_ids.dedup();
        let mut represented_review_item_ids = review_item_ids.clone();
        for owner_state in &output.review_owner_states {
            if let Some(review_item_id) = self
                .store
                .sync_existing_review_item_owners_in_transaction(
                    transaction,
                    consumer_id,
                    &owner_state.ref_id,
                    &owner_state.target_key,
                    &proposal_id,
                    &owner_state.owners,
                )
                .await?
            {
                represented_review_item_ids.push(review_item_id);
            }
        }
        represented_review_item_ids.sort();
        represented_review_item_ids.dedup();
        self.store
            .obsolete_unrepresented_review_items_in_transaction(
                transaction,
                consumer_id,
                &proposal_id,
                &represented_review_item_ids,
            )
            .await?;

        let next_binding = if publishable_changed {
            let publication = SurfacePublication::new(
                format!("publication-{}", Uuid::new_v4()),
                consumer_id,
                output.publishable_manifest.manifest_id.clone(),
                Some(proposal_id.clone()),
                &trigger.kind,
                &trigger.actor,
                binding.as_ref().map(|current| current.active_publication_id.clone()),
            );
            let next_binding = self
                .store
                .publish_and_bind_in_transaction(
                    transaction,
                    &publication,
                    binding.as_ref().map(|current| current.generation),
                )
                .await?;
            self.store
                .enqueue_outbox_event_in_transaction(
                    transaction,
                    &SurfaceOutboxEvent::new(
                        format!("outbox-{}", publication.publication_id),
                        "surface_publication_changed",
                        consumer_id,
                        json!({
                            "publicationId": publication.publication_id,
                            "generation": next_binding.generation,
                            "reason": trigger.kind,
                        }),
                    ),
                )
                .await?;
            Some(next_binding)
        } else {
            binding
        };

        self.store
            .reconcile_proposal_lifecycles_in_transaction(transaction, consumer_id, &proposal_id)
            .await?;
        Ok(MaterializationCommit {
            proposal_id: Some(proposal_id),
            binding: next_binding,
            review_item_ids,
            effective_surface_changed: publishable_changed,
        })
    }

    pub async fn compile_consumer_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        consumer_id: &str,
        trigger: &MaterializationTrigger,
    ) -> Result<MaterializationCommit> {
        let default_config_mode = self.load_default_config_mode().await?;
        self.compile_consumer_in_transaction_with_default(transaction, consumer_id, &default_config_mode, trigger)
            .await
    }

    pub async fn compile_consumer_in_transaction_with_default(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        consumer_id: &str,
        default_config_mode: &str,
        trigger: &MaterializationTrigger,
    ) -> Result<MaterializationCommit> {
        let input =
            SurfaceAuthoringLoader::load_consumer_input_in_transaction(transaction, consumer_id, default_config_mode)
                .await?;
        let trigger = self
            .derive_consumer_trigger_in_transaction(transaction, consumer_id, &input, trigger)
            .await?;
        let output = SurfaceMaterializer::compile(input)?;
        self.persist_in_transaction(transaction, &output, &trigger).await
    }

    pub async fn compile_consumer_with_changes_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        consumer_id: &str,
        changes: HashMap<CapabilityRefId, ChangeClass>,
        trigger: &MaterializationTrigger,
    ) -> Result<MaterializationCommit> {
        let default_config_mode = self.load_default_config_mode().await?;
        let input = SurfaceAuthoringLoader::load_consumer_input_with_changes_in_transaction(
            transaction,
            consumer_id,
            &default_config_mode,
            changes,
        )
        .await?;
        let trigger = self
            .derive_consumer_trigger_in_transaction(transaction, consumer_id, &input, trigger)
            .await?;
        let output = SurfaceMaterializer::compile(input)?;
        self.persist_in_transaction(transaction, &output, &trigger).await
    }

    async fn derive_consumer_trigger_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        consumer_id: &str,
        input: &MaterializationInput,
        trigger: &MaterializationTrigger,
    ) -> Result<MaterializationTrigger> {
        let mut server_ids = input.dependency_server_ids.clone();
        server_ids.extend(trigger.catalog_dependency_revisions.server_ids().map(str::to_string));
        let catalog_dependency_revisions =
            CatalogDependencyRevisions::derive_in_transaction(transaction, consumer_id, &server_ids, None).await?;
        Ok(trigger.with_catalog_dependency_revisions(catalog_dependency_revisions))
    }

    async fn load_default_config_mode(&self) -> Result<String> {
        load_default_config_mode(&self.pool).await
    }

    pub async fn verify_catalog_revision_set_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        expected: &HashMap<String, i64>,
    ) -> Result<()> {
        let actual = sqlx::query_as::<_, (String, i64)>(
            "SELECT server_id, catalog_revision FROM capability_server_snapshots ORDER BY server_id",
        )
        .fetch_all(&mut **transaction)
        .await?
        .into_iter()
        .collect::<HashMap<_, _>>();
        if actual != *expected {
            return Err(CatalogError::ConcurrencyConflict {
                entity: "capability catalog revision set",
                id: "current".to_string(),
            });
        }
        Ok(())
    }

    async fn verify_catalog_dependency_revisions_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Sqlite>,
        expected: &CatalogDependencyRevisions,
    ) -> Result<()> {
        let actual =
            CatalogDependencyRevisions::load_current_for_expected_servers_in_transaction(transaction, expected).await?;
        if actual != *expected {
            return Err(CatalogError::ConcurrencyConflict {
                entity: "capability catalog revision set",
                id: "current".to_string(),
            });
        }
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn capability_change_event_id(
    consumer_id: &str,
    trigger_kind: &str,
    trigger_id: &str,
    ref_id: &CapabilityRefId,
    target_key: &ReviewTargetKey,
    target_capability_id: Option<&CapabilityId>,
    change_class: ChangeClass,
    policy_action: PolicyAction,
) -> String {
    let canonical = json!({
        "consumerId": consumer_id,
        "triggerKind": trigger_kind,
        "triggerId": trigger_id,
        "refId": ref_id.as_str(),
        "targetKey": target_key.as_str(),
        "targetCapabilityId": target_capability_id.map(CapabilityId::as_str),
        "changeClass": change_class.as_str(),
        "policyAction": policy_action.as_str(),
    });
    let digest = Sha256::digest(serde_json::to_vec(&canonical).expect("change event identity must serialize"));
    format!("change-{digest:x}")
}

fn surface_proposal_id(
    consumer_id: &str,
    trigger: &MaterializationTrigger,
    proposed_manifest_id: &mcpmate_capability_store::SurfaceManifestId,
    publishable_manifest_id: &mcpmate_capability_store::SurfaceManifestId,
) -> String {
    let source_revision_set = trigger
        .catalog_dependency_revisions
        .0
        .iter()
        .map(|(server_id, revision)| (server_id.as_str(), *revision))
        .collect::<Vec<_>>();
    let canonical = json!({
        "consumerId": consumer_id,
        "triggerKind": trigger.kind,
        "triggerId": trigger.id,
        "sourceRevisionSet": source_revision_set,
        "proposedManifestId": proposed_manifest_id.as_str(),
        "publishableManifestId": publishable_manifest_id.as_str(),
    });
    let digest = Sha256::digest(serde_json::to_vec(&canonical).expect("proposal identity must serialize"));
    format!("proposal-{digest:x}")
}

async fn load_managed_consumer_ids(
    pool: &Pool<Sqlite>,
    default_config_mode: &str,
) -> Result<Vec<String>> {
    let consumer_rows = sqlx::query_as::<_, (String, Option<String>)>(
        r#"
        SELECT identifier, config_mode
        FROM client
        WHERE approval_status = 'approved'
        ORDER BY identifier
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(filter_managed_consumers(consumer_rows, default_config_mode))
}

async fn materialize_managed_surfaces_in_transaction(
    pool: &Pool<Sqlite>,
    transaction: &mut Transaction<'_, Sqlite>,
    consumer_ids: &[String],
    default_config_mode: &str,
    republish_existing: bool,
    trigger_kind: &str,
    trigger_id: &str,
    trigger_actor: &str,
    trigger_server_id: Option<&str>,
    changes: &HashMap<CapabilityRefId, ChangeClass>,
) -> Result<Vec<(String, MaterializationCommit)>> {
    let coordinator = MaterializationCoordinator::new(pool.clone());
    let store = SqliteSurfaceStore::new(pool.clone());
    let mut commits = Vec::new();
    for consumer_id in consumer_ids {
        let has_binding = store
            .load_binding_in_transaction(transaction, consumer_id)
            .await?
            .is_some();
        if republish_existing || !has_binding {
            let input = SurfaceAuthoringLoader::load_consumer_input_with_changes_in_transaction(
                transaction,
                consumer_id,
                default_config_mode,
                changes.clone(),
            )
            .await?;
            let catalog_dependency_revisions = CatalogDependencyRevisions::derive_in_transaction(
                transaction,
                consumer_id,
                &input.dependency_server_ids,
                trigger_server_id,
            )
            .await?;
            let trigger = MaterializationTrigger::from_dependencies(
                trigger_kind,
                trigger_id,
                catalog_dependency_revisions,
                trigger_actor,
            );
            let output = SurfaceMaterializer::compile(input)?;
            let commit = coordinator
                .persist_in_transaction(transaction, &output, &trigger)
                .await?;
            commits.push((consumer_id.clone(), commit));
        }
    }
    Ok(commits)
}

async fn warm_managed_surfaces(
    pool: &Pool<Sqlite>,
    consumer_ids: &[String],
) -> Result<()> {
    let reader = super::surface_read::SurfaceReader::new(pool.clone());
    for consumer_id in consumer_ids {
        reader.load(consumer_id).await?;
    }
    Ok(())
}

async fn materialize_managed_surfaces(
    pool: &Pool<Sqlite>,
    republish_existing: bool,
    trigger_kind: &str,
    changes: &HashMap<CapabilityRefId, ChangeClass>,
) -> Result<Vec<(String, MaterializationCommit)>> {
    let default_config_mode = load_default_config_mode(pool).await?;
    let consumer_ids = load_managed_consumer_ids(pool, &default_config_mode).await?;
    let mut transaction = pool.begin().await?;
    let trigger_id = Uuid::new_v4().to_string();
    let commits = materialize_managed_surfaces_in_transaction(
        pool,
        &mut transaction,
        &consumer_ids,
        &default_config_mode,
        republish_existing,
        trigger_kind,
        &trigger_id,
        "startup",
        None,
        changes,
    )
    .await?;
    transaction.commit().await?;
    warm_managed_surfaces(pool, &consumer_ids).await?;
    Ok(commits)
}

/// Bootstrap one managed Consumer Surface when it has no active publication yet.
///
/// Reuses the same transactional materialization path as startup bootstrap instead of
/// introducing a separate client-registration code path.
pub async fn bootstrap_managed_consumer_surface_if_missing(
    pool: &Pool<Sqlite>,
    consumer_id: &str,
    trigger_kind: &str,
    trigger_id: impl Into<String>,
    actor: &str,
) -> Result<Option<MaterializationCommit>> {
    let default_config_mode = load_default_config_mode(pool).await?;
    let mut transaction = pool.begin().await?;
    let trigger_id = trigger_id.into();
    let commits = materialize_managed_surfaces_in_transaction(
        pool,
        &mut transaction,
        &[consumer_id.to_string()],
        &default_config_mode,
        false,
        trigger_kind,
        &trigger_id,
        actor,
        None,
        &HashMap::new(),
    )
    .await?;
    transaction.commit().await?;
    if commits.is_empty() {
        return Ok(None);
    }
    warm_managed_surfaces(pool, &[consumer_id.to_string()]).await?;
    Ok(commits
        .into_iter()
        .find_map(|(id, commit)| (id == consumer_id).then_some(commit)))
}

pub async fn bootstrap_managed_surfaces(pool: &Pool<Sqlite>) -> Result<Vec<(String, MaterializationCommit)>> {
    materialize_managed_surfaces(pool, false, "startup_bootstrap", &HashMap::new()).await
}

pub async fn synchronize_builtin_catalog_and_bootstrap_managed_surfaces(
    pool: &Pool<Sqlite>,
    records: Vec<CatalogRecord>,
) -> Result<(CatalogCommit, Vec<(String, MaterializationCommit)>)> {
    let mode_rule_definition = json!({
        "version": 1,
        "unify": crate::mcper::UNIFY_BUILTIN_TOOL_NAMES,
        "hostedProfiles": crate::mcper::HOSTED_BUILTIN_TOOL_NAMES,
        "hostedActivated": [],
        "hostedCustom": [],
        "transparent": [],
    });
    let mode_rule_fingerprint = format!(
        "builtin-services-{digest:x}",
        digest = Sha256::digest(serde_json::to_vec(&mode_rule_definition)?),
    );
    let initialize = crate::mcper::builtin::metadata::create_catalog_initialize_result();
    let observation = CapabilityObservation::new(
        BUILTIN_CAPABILITY_SOURCE_ID,
        "MCPMate Builtin Services",
        mode_rule_fingerprint,
        initialize,
        vec![
            KindObservation::new(
                CapabilityKind::Tools,
                DeclarationState::Supported,
                InventoryState::Complete,
            ),
            KindObservation::new(
                CapabilityKind::Prompts,
                DeclarationState::Supported,
                InventoryState::Complete,
            ),
            KindObservation::new(
                CapabilityKind::Resources,
                DeclarationState::Unsupported,
                InventoryState::Complete,
            ),
            KindObservation::new(
                CapabilityKind::ResourceTemplates,
                DeclarationState::Unsupported,
                InventoryState::Complete,
            ),
        ],
        records,
    );
    let default_config_mode = load_default_config_mode(pool).await?;
    let consumer_ids = load_managed_consumer_ids(pool, &default_config_mode).await?;
    let catalog = SqliteCapabilityCatalog::new(pool.clone());
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await?;
    let reconciliation = catalog.reconcile_observation_in(&mut transaction, observation).await?;
    let mut changes = HashMap::new();
    for ref_id in &reconciliation.delta.added_refs {
        changes.insert(ref_id.clone(), ChangeClass::NewRef);
    }
    for version_change in &reconciliation.delta.changed_versions {
        changes.insert(version_change.ref_id.clone(), ChangeClass::BuiltinDefinition);
    }
    for ref_id in &reconciliation.delta.unresolved_refs {
        changes.insert(ref_id.clone(), ChangeClass::Missing);
    }
    for ref_id in &reconciliation.delta.reappeared_refs {
        changes.insert(ref_id.clone(), ChangeClass::Reappeared);
    }
    let trigger_id = Uuid::new_v4().to_string();
    let materializations = materialize_managed_surfaces_in_transaction(
        pool,
        &mut transaction,
        &consumer_ids,
        &default_config_mode,
        reconciliation.commit.changed,
        "builtin_catalog_startup_sync",
        &trigger_id,
        "startup",
        Some(BUILTIN_CAPABILITY_SOURCE_ID),
        &changes,
    )
    .await?;
    let catalog_commit = reconciliation.commit;
    transaction.commit().await?;
    warm_managed_surfaces(pool, &consumer_ids).await?;
    Ok((catalog_commit, materializations))
}

pub async fn converge_inherited_consumers_for_default_mode_in_transaction(
    pool: &Pool<Sqlite>,
    transaction: &mut Transaction<'_, Sqlite>,
    target_mode: &str,
    trigger: &MaterializationTrigger,
) -> Result<Vec<(String, MaterializationCommit)>> {
    let mode = EffectiveConfigMode::parse(target_mode).ok_or_else(|| CatalogError::InvalidSurfaceValue {
        field: "default client config mode",
        value: target_mode.to_string(),
    })?;
    let consumer_ids = sqlx::query_scalar::<_, String>(
        r#"
        SELECT identifier
        FROM client
        WHERE approval_status = 'approved'
          AND (config_mode IS NULL OR TRIM(config_mode) = '')
        ORDER BY identifier
        "#,
    )
    .fetch_all(&mut **transaction)
    .await?;

    if mode == EffectiveConfigMode::Transparent {
        for consumer_id in &consumer_ids {
            revoke_managed_surface_in_transaction(pool, transaction, consumer_id, &trigger.id).await?;
        }
        return Ok(Vec::new());
    }

    let coordinator = MaterializationCoordinator::new(pool.clone());
    let mut commits = Vec::with_capacity(consumer_ids.len());
    for consumer_id in consumer_ids {
        let commit = coordinator
            .compile_consumer_in_transaction_with_default(transaction, &consumer_id, target_mode, trigger)
            .await?;
        commits.push((consumer_id, commit));
    }
    Ok(commits)
}

pub async fn revoke_managed_surface_in_transaction(
    pool: &Pool<Sqlite>,
    transaction: &mut Transaction<'_, Sqlite>,
    consumer_id: &str,
    trigger_id: &str,
) -> Result<bool> {
    let store = SqliteSurfaceStore::new(pool.clone());
    let Some(binding) = store.load_binding_in_transaction(transaction, consumer_id).await? else {
        return Ok(false);
    };
    store
        .obsolete_consumer_review_items_in_transaction(transaction, consumer_id)
        .await?;
    store
        .enqueue_outbox_event_in_transaction(
            transaction,
            &SurfaceOutboxEvent::new(
                format!(
                    "outbox-managed-revocation-{trigger_id}-{consumer_id}-{}",
                    binding.generation
                ),
                "surface_publication_changed",
                consumer_id,
                json!({
                    "publicationId": binding.active_publication_id,
                    "generation": binding.generation,
                    "reason": "managed_access_revoked",
                }),
            ),
        )
        .await?;
    sqlx::query("DELETE FROM consumer_surface_bindings WHERE consumer_id = ?")
        .bind(consumer_id)
        .execute(&mut **transaction)
        .await?;
    Ok(true)
}

pub async fn load_default_config_mode(pool: &Pool<Sqlite>) -> Result<String> {
    crate::config::client::init::resolve_default_client_config_mode(pool)
        .await
        .map_err(|error| CatalogError::InvalidSurfaceValue {
            field: "default client config mode",
            value: error.to_string(),
        })
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
