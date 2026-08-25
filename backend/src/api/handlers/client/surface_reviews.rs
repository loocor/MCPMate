use std::{collections::BTreeMap, sync::Arc};

use axum::{
    Json,
    extract::{Path, Query, State},
};
use mcpmate_capability_store::{
    CapabilityId, CapabilityRefId, ReviewLifecycle, ReviewOwnerType, ReviewResolutionAction, RollbackBlock,
    SqliteSurfaceStore, SurfaceOutboxEvent, SurfacePublication, SurfaceReviewDecisionDraft, SurfaceReviewFilter,
    SurfaceReviewOwner, SurfaceReviewRecord,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{Pool, Sqlite};

use crate::{
    api::{
        handlers::ApiError,
        models::client::{
            SurfaceIntentPreviewData, SurfaceIntentPreviewReq, SurfaceIntentPreviewResp,
            SurfaceIntentResolutionActionData, SurfaceIntentResolveReq, SurfacePublicationData,
            SurfacePublicationListData, SurfacePublicationListQuery, SurfacePublicationListResp,
            SurfacePublicationPath, SurfaceReviewActionData, SurfaceReviewActionReq, SurfaceReviewActionResp,
            SurfaceReviewDecisionData, SurfaceReviewFieldDiffData, SurfaceReviewItemData, SurfaceReviewItemResp,
            SurfaceReviewLifecycleData, SurfaceReviewListData, SurfaceReviewListQuery, SurfaceReviewListResp,
            SurfaceReviewOwnerData, SurfaceReviewOwnerTypeData, SurfaceReviewPath, SurfaceReviewResolutionActionData,
            SurfaceReviewSummaryData, SurfaceReviewSummaryEntryData, SurfaceReviewSummaryResp, SurfaceRollbackData,
            SurfaceRollbackReq, SurfaceRollbackResp,
        },
        routes::AppState,
    },
    audit::{AuditAction, AuditEvent, AuditStatus},
    core::capability::materializer::{MaterializationCoordinator, MaterializationTrigger},
};

pub async fn list_surface_reviews(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SurfaceReviewListQuery>,
) -> Result<Json<SurfaceReviewListResp>, ApiError> {
    let pool = database_pool(&state)?;
    let store = SqliteSurfaceStore::new(pool.clone());
    let records = store
        .list_review_items(&SurfaceReviewFilter {
            consumer_id: query.consumer_id,
            owner_type: query.owner_type.map(review_owner_type),
            owner_id: query.owner_id,
            lifecycle: query.state.map(review_lifecycle),
        })
        .await
        .map_err(store_error)?;
    let mut items = Vec::with_capacity(records.len());
    for record in records {
        items.push(review_item_data(pool, record).await?);
    }
    Ok(Json(SurfaceReviewListResp::success(SurfaceReviewListData { items })))
}

pub async fn get_surface_review(
    State(state): State<Arc<AppState>>,
    Path(path): Path<SurfaceReviewPath>,
) -> Result<Json<SurfaceReviewItemResp>, ApiError> {
    let review_item_id = path.review_item_id;
    let pool = database_pool(&state)?;
    let store = SqliteSurfaceStore::new(pool.clone());
    let record = store
        .load_review_record(&review_item_id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| ApiError::NotFound(format!("Surface review item '{review_item_id}' was not found")))?;
    Ok(Json(SurfaceReviewItemResp::success(
        review_item_data(pool, record).await?,
    )))
}

pub async fn summarize_surface_reviews(
    State(state): State<Arc<AppState>>
) -> Result<Json<SurfaceReviewSummaryResp>, ApiError> {
    let pool = database_pool(&state)?;
    let store = SqliteSurfaceStore::new(pool.clone());
    let records = store
        .list_review_items(&SurfaceReviewFilter {
            lifecycle: Some(ReviewLifecycle::Pending),
            ..SurfaceReviewFilter::default()
        })
        .await
        .map_err(store_error)?;
    let mut grouped = BTreeMap::<(ReviewOwnerType, String), SurfaceReviewSummaryEntryData>::new();
    for record in &records {
        for owner in &record.owners {
            let key = (owner.owner_type, owner.owner_id.clone());
            let entry = grouped.entry(key).or_insert_with(|| SurfaceReviewSummaryEntryData {
                owner: SurfaceReviewOwnerData {
                    owner_type: review_owner_type_data(owner.owner_type),
                    owner_id: owner.owner_id.clone(),
                },
                pending_count: 0,
                earliest_created_at: record.created_at.to_rfc3339(),
                change_classes: BTreeMap::new(),
            });
            entry.pending_count += 1;
            let created_at = record.created_at.to_rfc3339();
            if created_at < entry.earliest_created_at {
                entry.earliest_created_at = created_at;
            }
            *entry
                .change_classes
                .entry(record.item.change_class.clone())
                .or_default() += 1;
        }
    }
    let failed_reconciliation_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM surface_reconciliation_jobs WHERE status = 'failed'")
            .fetch_one(pool)
            .await
            .map_err(database_error)?;
    Ok(Json(SurfaceReviewSummaryResp::success(SurfaceReviewSummaryData {
        pending_count: records.len() as u64,
        failed_reconciliation_count: failed_reconciliation_count as u64,
        entries: grouped.into_values().collect(),
    })))
}

pub async fn list_surface_publications(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SurfacePublicationListQuery>,
) -> Result<Json<SurfacePublicationListResp>, ApiError> {
    let pool = database_pool(&state)?;
    let store = SqliteSurfaceStore::new(pool.clone());
    let binding = store.load_binding(&query.consumer_id).await.map_err(store_error)?;
    let history = store
        .load_publication_history(&query.consumer_id)
        .await
        .map_err(store_error)?;
    let mut publications = Vec::with_capacity(history.len());
    for publication in history {
        let mut transaction = pool.begin().await.map_err(database_error)?;
        let eligibility = store
            .is_publication_rollback_eligible_in_transaction(&mut transaction, &publication.publication_id)
            .await
            .map_err(store_error)?;
        transaction.rollback().await.map_err(database_error)?;
        let (rollback_eligible, rollback_blocks) = match eligibility {
            Ok(()) => (true, Vec::new()),
            Err(blocks) => (false, blocks.iter().map(rollback_block_message).collect()),
        };
        publications.push(SurfacePublicationData {
            active: binding
                .as_ref()
                .is_some_and(|binding| binding.active_publication_id == publication.publication_id),
            publication_id: publication.publication_id,
            consumer_id: publication.consumer_id,
            manifest_id: publication.manifest_id.to_string(),
            proposal_id: publication.proposal_id,
            reason: publication.reason,
            published_by: publication.published_by,
            published_at: publication.published_at.to_rfc3339(),
            supersedes_publication_id: publication.supersedes_publication_id,
            rollback_eligible,
            rollback_blocks,
        });
    }
    Ok(Json(SurfacePublicationListResp::success(SurfacePublicationListData {
        publications,
    })))
}

pub async fn rollback_surface_publication(
    State(state): State<Arc<AppState>>,
    Path(path): Path<SurfacePublicationPath>,
    Json(request): Json<SurfaceRollbackReq>,
) -> Result<Json<SurfaceRollbackResp>, ApiError> {
    let pool = database_pool(&state)?.clone();
    let store = SqliteSurfaceStore::new(pool.clone());
    let mut transaction = pool.begin().await.map_err(database_error)?;
    let target = store
        .load_publication_in_transaction(&mut transaction, &path.publication_id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| ApiError::NotFound(format!("Surface publication '{}' was not found", path.publication_id)))?;
    if target.consumer_id != request.consumer_id {
        return Err(ApiError::BadRequest(
            "Publication does not belong to the requested Consumer".to_string(),
        ));
    }
    let current = store
        .load_binding_in_transaction(&mut transaction, &request.consumer_id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| ApiError::Conflict("Consumer has no active Surface publication".to_string()))?;
    if current.generation != request.expected_binding_generation {
        return Err(ApiError::Conflict(
            "Surface binding changed before rollback".to_string(),
        ));
    }
    if current.active_publication_id == target.publication_id {
        return Err(ApiError::BadRequest("Target publication is already active".to_string()));
    }
    if let Err(blocks) = store
        .is_publication_rollback_eligible_in_transaction(&mut transaction, &target.publication_id)
        .await
        .map_err(store_error)?
    {
        return Err(ApiError::Conflict(format!(
            "Publication is not executable: {}",
            blocks.iter().map(rollback_block_message).collect::<Vec<_>>().join("; ")
        )));
    }
    let rollback_publication_id = format!("publication-{}", uuid::Uuid::new_v4());
    let binding = store
        .publish_and_bind_in_transaction(
            &mut transaction,
            &SurfacePublication::new(
                &rollback_publication_id,
                &request.consumer_id,
                target.manifest_id.clone(),
                None,
                format!("rollback:{}", request.reason),
                &request.actor,
                Some(current.active_publication_id.clone()),
            ),
            Some(current.generation),
        )
        .await
        .map_err(store_error)?;
    store
        .enqueue_outbox_event_in_transaction(
            &mut transaction,
            &SurfaceOutboxEvent::new(
                format!("outbox-{rollback_publication_id}"),
                "surface_publication_changed",
                &request.consumer_id,
                serde_json::json!({
                    "publicationId": rollback_publication_id,
                    "generation": binding.generation,
                    "reason": "rollback",
                }),
            ),
        )
        .await
        .map_err(store_error)?;
    transaction.commit().await.map_err(database_error)?;
    crate::audit::interceptor::emit_event(
        state.audit_service.as_ref(),
        AuditEvent::new(AuditAction::SurfaceRollback, AuditStatus::Success)
            .with_http_route("POST", "/api/client/surface/publications/{publication_id}/rollback")
            .with_actor(request.actor.clone())
            .with_client_id(request.consumer_id.clone())
            .with_target(target.publication_id.clone())
            .with_data(serde_json::json!({
                "rollback_publication_id": rollback_publication_id,
                "before_publication_id": current.active_publication_id,
                "target_manifest_id": target.manifest_id,
                "binding_generation": binding.generation,
                "reason": request.reason,
                "outcome": "published",
            }))
            .build(),
    )
    .await;
    emit_surface_publish_audit(
        &state,
        "/api/client/surface/publications/{publication_id}/rollback",
        &request.actor,
        &binding,
        None,
        "rollback",
    )
    .await;
    Ok(Json(SurfaceRollbackResp::success(SurfaceRollbackData {
        publication_id: rollback_publication_id,
        rolled_back_to_publication_id: target.publication_id,
        consumer_id: request.consumer_id,
        active_manifest_id: target.manifest_id.to_string(),
        binding_generation: binding.generation,
    })))
}

pub async fn approve_surface_review(
    State(state): State<Arc<AppState>>,
    Path(path): Path<SurfaceReviewPath>,
    Json(request): Json<SurfaceReviewActionReq>,
) -> Result<Json<SurfaceReviewActionResp>, ApiError> {
    resolve_target_review(
        state,
        path.review_item_id,
        request,
        ReviewResolutionAction::ApproveTarget,
    )
    .await
}

pub async fn reject_surface_review(
    State(state): State<Arc<AppState>>,
    Path(path): Path<SurfaceReviewPath>,
    Json(request): Json<SurfaceReviewActionReq>,
) -> Result<Json<SurfaceReviewActionResp>, ApiError> {
    resolve_target_review(
        state,
        path.review_item_id,
        request,
        ReviewResolutionAction::RejectTarget,
    )
    .await
}

pub async fn preview_surface_intent_resolution(
    State(state): State<Arc<AppState>>,
    Path(path): Path<SurfaceReviewPath>,
    Json(request): Json<SurfaceIntentPreviewReq>,
) -> Result<Json<SurfaceIntentPreviewResp>, ApiError> {
    preview_intent_resolution(state, path.review_item_id, request).await
}

pub async fn resolve_surface_intent(
    State(state): State<Arc<AppState>>,
    Path(path): Path<SurfaceReviewPath>,
    Json(request): Json<SurfaceIntentResolveReq>,
) -> Result<Json<SurfaceReviewActionResp>, ApiError> {
    resolve_intent_review(state, path.review_item_id, request).await
}

async fn resolve_target_review(
    state: Arc<AppState>,
    review_item_id: String,
    request: SurfaceReviewActionReq,
    action: ReviewResolutionAction,
) -> Result<Json<SurfaceReviewActionResp>, ApiError> {
    let pool = database_pool(&state)?.clone();
    let store = SqliteSurfaceStore::new(pool.clone());
    let mut transaction = pool.begin().await.map_err(database_error)?;
    let item = store
        .load_review_record_in_transaction(&mut transaction, &review_item_id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| ApiError::NotFound(format!("Surface review item '{review_item_id}' was not found")))?;
    if item.item.lifecycle == ReviewLifecycle::Obsolete {
        return Err(ApiError::Conflict(format!(
            "Surface review item '{review_item_id}' is obsolete"
        )));
    }
    let target_key = item.item.target_key.to_string();
    if target_key != request.expected_target_key {
        return Err(ApiError::Conflict(format!(
            "Surface review target changed from '{}' to '{}'",
            request.expected_target_key, target_key
        )));
    }
    if !(target_key.starts_with("capability:") || target_key.starts_with("reappeared:")) {
        return Err(ApiError::BadRequest(format!(
            "Review target '{target_key}' does not accept approve/reject actions"
        )));
    }
    let binding = store
        .load_binding_in_transaction(&mut transaction, &item.item.consumer_id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| {
            ApiError::Conflict(format!(
                "Consumer '{}' has no active Surface publication",
                item.item.consumer_id
            ))
        })?;
    if binding.generation != request.expected_binding_generation {
        return Err(ApiError::Conflict(format!(
            "Surface binding generation changed from {} to {}",
            request.expected_binding_generation, binding.generation
        )));
    }
    let decision_id = format!("decision-{}", uuid::Uuid::new_v4());
    store
        .append_review_decision_in_transaction(
            &mut transaction,
            &SurfaceReviewDecisionDraft::new(&decision_id, &review_item_id, action, None, &request.actor),
            item.item.current_decision_id.as_deref(),
        )
        .await
        .map_err(store_error)?;
    let trigger = MaterializationTrigger::for_consumer("review_resolution", &decision_id, &request.actor);
    let commit = MaterializationCoordinator::new(pool.clone())
        .compile_consumer_in_transaction(&mut transaction, &item.item.consumer_id, &trigger)
        .await
        .map_err(store_error)?;
    transaction.commit().await.map_err(database_error)?;
    let next_generation = commit
        .binding
        .as_ref()
        .map(|binding| binding.generation)
        .unwrap_or(binding.generation);
    let action_data = review_resolution_action_data(action);
    crate::audit::interceptor::emit_event(
        state.audit_service.as_ref(),
        AuditEvent::new(AuditAction::SurfaceReviewResolve, AuditStatus::Success)
            .with_http_route(
                "POST",
                if action == ReviewResolutionAction::ApproveTarget {
                    "/api/client/surface/reviews/{review_item_id}/approve"
                } else {
                    "/api/client/surface/reviews/{review_item_id}/reject"
                },
            )
            .with_actor(request.actor.clone())
            .with_client_id(item.item.consumer_id)
            .with_target(review_item_id.clone())
            .with_data(serde_json::json!({
                "decision_id": decision_id,
                "proposal_id": item.item.created_by_proposal_id,
                "ref_id": item.item.ref_id,
                "before_capability_id": item.item.before_capability_id,
                "target_capability_id": item.item.target_capability_id,
                "target_key": target_key,
                "resolution_action": action_data,
                "binding_generation": next_generation,
                "effective_surface_changed": commit.effective_surface_changed,
                "outcome": "resolved",
            }))
            .build(),
    )
    .await;
    if commit.effective_surface_changed
        && let Some(next_binding) = commit.binding.as_ref()
    {
        emit_surface_publish_audit(
            &state,
            if action == ReviewResolutionAction::ApproveTarget {
                "/api/client/surface/reviews/{review_item_id}/approve"
            } else {
                "/api/client/surface/reviews/{review_item_id}/reject"
            },
            &request.actor,
            next_binding,
            commit.proposal_id.as_deref(),
            "review_resolution",
        )
        .await;
    }
    Ok(Json(SurfaceReviewActionResp::success(SurfaceReviewActionData {
        review_item_id,
        decision_id,
        resolution_action: action_data,
        binding_generation: next_generation,
        effective_surface_changed: commit.effective_surface_changed,
    })))
}

#[derive(Clone)]
struct IntentImpact {
    owner: Option<SurfaceReviewOwner>,
    owner_revision: String,
    profile_authoring_generation: Option<i64>,
    impacted_consumer_ids: Vec<String>,
    impact_token: String,
}

async fn preview_intent_resolution(
    state: Arc<AppState>,
    review_item_id: String,
    request: SurfaceIntentPreviewReq,
) -> Result<Json<SurfaceIntentPreviewResp>, ApiError> {
    let pool = database_pool(&state)?.clone();
    let default_config_mode = crate::core::capability::materializer::load_default_config_mode(&pool)
        .await
        .map_err(store_error)?;
    let store = SqliteSurfaceStore::new(pool.clone());
    let mut transaction = pool.begin().await.map_err(database_error)?;
    let item = store
        .load_review_record_in_transaction(&mut transaction, &review_item_id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| ApiError::NotFound(format!("Surface review item '{review_item_id}' was not found")))?;
    validate_intent_item(&item, &request.action)?;
    let owner = request.owner.as_ref().map(surface_review_owner);
    let impact = calculate_intent_impact(
        &mut transaction,
        &item,
        &request.action,
        owner.as_ref(),
        request.new_ref_id.as_deref(),
        &default_config_mode,
    )
    .await?;
    transaction.rollback().await.map_err(database_error)?;
    Ok(Json(SurfaceIntentPreviewResp::success(SurfaceIntentPreviewData {
        review_item_id,
        action: request.action,
        owner: impact.owner.map(surface_review_owner_data),
        owner_revision: impact.owner_revision,
        impacted_consumer_ids: impact.impacted_consumer_ids,
        impact_token: impact.impact_token,
    })))
}

async fn resolve_intent_review(
    state: Arc<AppState>,
    review_item_id: String,
    request: SurfaceIntentResolveReq,
) -> Result<Json<SurfaceReviewActionResp>, ApiError> {
    let pool = database_pool(&state)?.clone();
    let store = SqliteSurfaceStore::new(pool.clone());
    let mut transaction = pool.begin_with("BEGIN IMMEDIATE").await.map_err(database_error)?;
    let default_config_mode = crate::core::capability::materializer::load_default_config_mode(&pool)
        .await
        .map_err(store_error)?;
    let item = store
        .load_review_record_in_transaction(&mut transaction, &review_item_id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| ApiError::NotFound(format!("Surface review item '{review_item_id}' was not found")))?;
    validate_intent_item(&item, &request.action)?;
    if item.item.target_key.to_string() != request.expected_target_key {
        return Err(ApiError::Conflict(
            "Surface review target changed after preview".to_string(),
        ));
    }
    let binding = store
        .load_binding_in_transaction(&mut transaction, &item.item.consumer_id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| ApiError::Conflict("Consumer has no active Surface publication".to_string()))?;
    if binding.generation != request.expected_binding_generation {
        return Err(ApiError::Conflict("Surface binding changed after preview".to_string()));
    }
    let owner = request.owner.as_ref().map(surface_review_owner);
    let impact = calculate_intent_impact(
        &mut transaction,
        &item,
        &request.action,
        owner.as_ref(),
        request.new_ref_id.as_deref(),
        &default_config_mode,
    )
    .await?;
    if impact.owner_revision != request.expected_owner_revision || impact.impact_token != request.impact_token {
        return Err(ApiError::Conflict(
            "Intent owner or impacted Consumer set changed after preview".to_string(),
        ));
    }

    let resolution_action = match request.action {
        SurfaceIntentResolutionActionData::KeepIntent => ReviewResolutionAction::KeepIntent,
        SurfaceIntentResolutionActionData::RemoveIntent => ReviewResolutionAction::RemoveIntent,
        SurfaceIntentResolutionActionData::RebindRef => ReviewResolutionAction::RebindRef,
    };
    let decision_id = format!("decision-{}", uuid::Uuid::new_v4());
    let resolution_payload = match request.action {
        SurfaceIntentResolutionActionData::KeepIntent => None,
        SurfaceIntentResolutionActionData::RemoveIntent => Some(serde_json::json!({
            "owner": request.owner.as_ref(),
        })),
        SurfaceIntentResolutionActionData::RebindRef => Some(serde_json::json!({
            "owner": request.owner.as_ref(),
            "new_ref_id": request.new_ref_id.as_ref(),
        })),
    };
    store
        .append_review_decision_in_transaction(
            &mut transaction,
            &SurfaceReviewDecisionDraft::new(
                &decision_id,
                &review_item_id,
                resolution_action,
                resolution_payload,
                &request.actor,
            ),
            item.item.current_decision_id.as_deref(),
        )
        .await
        .map_err(store_error)?;

    if request.action != SurfaceIntentResolutionActionData::KeepIntent {
        let owner = impact
            .owner
            .as_ref()
            .ok_or_else(|| ApiError::BadRequest("Intent mutation requires an Owner".to_string()))?;
        if let Some(expected_generation) = impact.profile_authoring_generation {
            crate::core::capability::management::advance_profile_generation_in_transaction(
                &mut transaction,
                &owner.owner_id,
                expected_generation,
            )
            .await
            .map_err(store_error)?;
        }
        mutate_owner_relationship(
            &mut transaction,
            owner,
            &item.item.ref_id,
            &request.action,
            request.new_ref_id.as_deref(),
        )
        .await?;
        store
            .deactivate_review_owner_in_transaction(&mut transaction, &review_item_id, owner)
            .await
            .map_err(store_error)?;
    }

    let coordinator = MaterializationCoordinator::new(pool.clone());
    let mut effective_surface_changed = false;
    let mut response_generation = binding.generation;
    let mut publication_audits = Vec::new();
    for consumer_id in &impact.impacted_consumer_ids {
        let commit = coordinator
            .compile_consumer_in_transaction_with_default(
                &mut transaction,
                consumer_id,
                &default_config_mode,
                &MaterializationTrigger::for_consumer("intent_resolution", &decision_id, &request.actor),
            )
            .await
            .map_err(store_error)?;
        effective_surface_changed |= commit.effective_surface_changed;
        if commit.effective_surface_changed
            && let Some(next_binding) = commit.binding.as_ref()
        {
            publication_audits.push((next_binding.clone(), commit.proposal_id.clone()));
        }
        if consumer_id == &item.item.consumer_id
            && let Some(next_binding) = commit.binding.as_ref()
        {
            response_generation = next_binding.generation;
        }
    }
    transaction.commit().await.map_err(database_error)?;
    crate::audit::interceptor::emit_event(
        state.audit_service.as_ref(),
        AuditEvent::new(AuditAction::SurfaceReviewResolve, AuditStatus::Success)
            .with_http_route("POST", "/api/client/surface/reviews/{review_item_id}/resolve-intent")
            .with_actor(request.actor.clone())
            .with_client_id(item.item.consumer_id)
            .with_target(review_item_id.clone())
            .with_data(serde_json::json!({
                "decision_id": decision_id,
                "proposal_id": item.item.created_by_proposal_id,
                "ref_id": item.item.ref_id,
                "before_capability_id": item.item.before_capability_id,
                "target_capability_id": item.item.target_capability_id,
                "action": review_resolution_action_name(resolution_action),
                "owner": request.owner,
                "impacted_consumer_ids": impact.impacted_consumer_ids,
                "binding_generation": response_generation,
                "effective_surface_changed": effective_surface_changed,
                "outcome": "resolved",
            }))
            .build(),
    )
    .await;
    for (next_binding, proposal_id) in publication_audits {
        emit_surface_publish_audit(
            &state,
            "/api/client/surface/reviews/{review_item_id}/resolve-intent",
            &request.actor,
            &next_binding,
            proposal_id.as_deref(),
            "intent_resolution",
        )
        .await;
    }
    Ok(Json(SurfaceReviewActionResp::success(SurfaceReviewActionData {
        review_item_id,
        decision_id,
        resolution_action: review_resolution_action_data(resolution_action),
        binding_generation: response_generation,
        effective_surface_changed,
    })))
}

async fn emit_surface_publish_audit(
    state: &Arc<AppState>,
    route: &str,
    actor: &str,
    binding: &mcpmate_capability_store::ConsumerSurfaceBinding,
    proposal_id: Option<&str>,
    trigger: &str,
) {
    crate::audit::interceptor::emit_event(
        state.audit_service.as_ref(),
        AuditEvent::new(AuditAction::SurfacePublish, AuditStatus::Success)
            .with_http_route("POST", route)
            .with_actor(actor)
            .with_client_id(binding.consumer_id.clone())
            .with_target(binding.active_publication_id.clone())
            .with_data(serde_json::json!({
                "binding_generation": binding.generation,
                "proposal_id": proposal_id,
                "trigger": trigger,
            }))
            .build(),
    )
    .await;
}

fn validate_intent_item(
    item: &SurfaceReviewRecord,
    action: &SurfaceIntentResolutionActionData,
) -> Result<(), ApiError> {
    if item.item.lifecycle == ReviewLifecycle::Obsolete {
        return Err(ApiError::Conflict("Surface review item is obsolete".to_string()));
    }
    let target_key = item.item.target_key.to_string();
    if !(target_key.starts_with("missing:") || item.item.policy_action == "manual_rebind") {
        return Err(ApiError::BadRequest(format!(
            "Surface review target '{target_key}' does not accept Intent actions"
        )));
    }
    if *action == SurfaceIntentResolutionActionData::RebindRef && item.item.policy_action != "manual_rebind" {
        return Err(ApiError::BadRequest(
            "Rebind is only valid for a manual_rebind review item".to_string(),
        ));
    }
    Ok(())
}

async fn calculate_intent_impact(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    item: &SurfaceReviewRecord,
    action: &SurfaceIntentResolutionActionData,
    owner: Option<&SurfaceReviewOwner>,
    new_ref_id: Option<&str>,
    default_config_mode: &str,
) -> Result<IntentImpact, ApiError> {
    if *action == SurfaceIntentResolutionActionData::KeepIntent {
        let impacted_consumer_ids = vec![item.item.consumer_id.clone()];
        let owner_revision = "intent-preserved".to_string();
        let impact_token = impact_token(
            &item.item.review_item_id,
            action,
            None,
            &owner_revision,
            &impacted_consumer_ids,
            new_ref_id,
        )?;
        return Ok(IntentImpact {
            owner: None,
            owner_revision,
            profile_authoring_generation: None,
            impacted_consumer_ids,
            impact_token,
        });
    }
    let owner = owner.ok_or_else(|| ApiError::BadRequest("Intent mutation requires an Owner".to_string()))?;
    if !item.owners.contains(owner) {
        return Err(ApiError::Conflict(
            "Selected Owner is no longer active for this review item".to_string(),
        ));
    }
    if *action == SurfaceIntentResolutionActionData::RebindRef && new_ref_id.is_none() {
        return Err(ApiError::BadRequest("Rebind requires new_ref_id".to_string()));
    }
    let (owner_revision, profile_authoring_generation) =
        load_owner_revision(transaction, owner, &item.item.ref_id).await?;
    let impacted_consumer_ids = load_impacted_consumers(transaction, owner, default_config_mode).await?;
    let impact_token = impact_token(
        &item.item.review_item_id,
        action,
        Some(owner),
        &owner_revision,
        &impacted_consumer_ids,
        new_ref_id,
    )?;
    Ok(IntentImpact {
        owner: Some(owner.clone()),
        owner_revision,
        profile_authoring_generation,
        impacted_consumer_ids,
        impact_token,
    })
}

async fn load_owner_revision(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    owner: &SurfaceReviewOwner,
    ref_id: &CapabilityRefId,
) -> Result<(String, Option<i64>), ApiError> {
    let server_id: String = sqlx::query_scalar("SELECT server_id FROM capability_refs WHERE ref_id = ?")
        .bind(ref_id.as_str())
        .fetch_one(&mut **transaction)
        .await
        .map_err(database_error)?;
    let profile_authoring_generation = match owner.owner_type {
        ReviewOwnerType::StandardProfile | ReviewOwnerType::CustomProfile | ReviewOwnerType::ProfileServerExposure => {
            Some(
                sqlx::query_scalar::<_, i64>("SELECT authoring_generation FROM profile WHERE id = ?")
                    .bind(&owner.owner_id)
                    .fetch_optional(&mut **transaction)
                    .await
                    .map_err(database_error)?
                    .ok_or_else(|| ApiError::Conflict("Selected Profile Owner no longer exists".to_string()))?,
            )
        }
        _ => None,
    };
    let relationship = match owner.owner_type {
        ReviewOwnerType::StandardProfile | ReviewOwnerType::CustomProfile => {
            sqlx::query_scalar::<_, String>(
                "SELECT ref_id || ':' || enabled FROM profile_capability_refs WHERE profile_id = ? AND ref_id = ?",
            )
            .bind(&owner.owner_id)
            .bind(ref_id.as_str())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(database_error)?
        }
        ReviewOwnerType::ProfileServerExposure => sqlx::query_scalar::<_, String>(
            "SELECT server_id || ':' || enabled || ':' || new_ref_policy FROM profile_server_relationships WHERE profile_id = ? AND server_id = ?",
        )
        .bind(&owner.owner_id)
        .bind(&server_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(database_error)?,
        ReviewOwnerType::ConsumerDirectExposure => {
            sqlx::query_scalar::<_, String>(
                "SELECT ref_id || ':' || enabled FROM direct_exposure_refs WHERE consumer_id = ? AND ref_id = ?",
            )
            .bind(&owner.owner_id)
            .bind(ref_id.as_str())
            .fetch_optional(&mut **transaction)
            .await
            .map_err(database_error)?
        }
        ReviewOwnerType::ConsumerServerExposure => sqlx::query_scalar::<_, String>(
            "SELECT server_id || ':' || new_ref_policy FROM direct_exposure_servers WHERE consumer_id = ? AND server_id = ?",
        )
        .bind(&owner.owner_id)
        .bind(&server_id)
        .fetch_optional(&mut **transaction)
        .await
        .map_err(database_error)?,
        ReviewOwnerType::ModeRule => {
            return Err(ApiError::BadRequest("Mode rule intent cannot be removed or rebound".to_string()));
        }
    }
    .ok_or_else(|| ApiError::Conflict("Selected Owner relationship no longer exists".to_string()))?;
    let revision = match profile_authoring_generation {
        Some(generation) => format!("profile-generation:{generation}:{relationship}"),
        None => relationship,
    };
    Ok((
        format!("sha256:{:x}", Sha256::digest(revision.as_bytes())),
        profile_authoring_generation,
    ))
}

async fn load_impacted_consumers(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    owner: &SurfaceReviewOwner,
    default_config_mode: &str,
) -> Result<Vec<String>, ApiError> {
    let mut consumers = match owner.owner_type {
        ReviewOwnerType::ConsumerDirectExposure | ReviewOwnerType::ConsumerServerExposure => {
            vec![owner.owner_id.clone()]
        }
        ReviewOwnerType::StandardProfile | ReviewOwnerType::ProfileServerExposure => {
            let rows = sqlx::query_as::<_, (String, Option<String>)>(
                r#"
                SELECT DISTINCT client.identifier
                     , client.config_mode
                FROM client
                WHERE client.approval_status = 'approved'
                  AND (
                    (
                      client.capability_source = 'activated'
                      AND EXISTS (
                        SELECT 1 FROM profile
                        WHERE profile.id = ? AND profile.is_active = 1
                      )
                    )
                    OR (
                      client.capability_source = 'profiles'
                      AND EXISTS (
                        SELECT 1 FROM json_each(client.selected_profile_ids)
                        WHERE json_each.value = ?
                      )
                    )
                  )
                "#,
            )
            .bind(&owner.owner_id)
            .bind(&owner.owner_id)
            .fetch_all(&mut **transaction)
            .await
            .map_err(database_error)?;
            filter_managed_consumers(rows, default_config_mode)
        }
        ReviewOwnerType::CustomProfile => {
            let rows = sqlx::query_as::<_, (String, Option<String>)>(
                r#"
                SELECT identifier, config_mode FROM client
                WHERE approval_status = 'approved'
                  AND capability_source = 'custom'
                  AND custom_profile_id = ?
                "#,
            )
            .bind(&owner.owner_id)
            .fetch_all(&mut **transaction)
            .await
            .map_err(database_error)?;
            filter_managed_consumers(rows, default_config_mode)
        }
        ReviewOwnerType::ModeRule => {
            return Err(ApiError::BadRequest("Mode rule intent cannot be mutated".to_string()));
        }
    };
    consumers.sort();
    consumers.dedup();
    if consumers.is_empty() {
        return Err(ApiError::Conflict(
            "Selected Owner no longer affects a managed Consumer".to_string(),
        ));
    }
    Ok(consumers)
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

fn impact_token(
    review_item_id: &str,
    action: &SurfaceIntentResolutionActionData,
    owner: Option<&SurfaceReviewOwner>,
    owner_revision: &str,
    impacted_consumer_ids: &[String],
    new_ref_id: Option<&str>,
) -> Result<String, ApiError> {
    let canonical = serde_json_canonicalizer::to_vec(&serde_json::json!({
        "format": "mcpmate.surface-intent-impact.v1",
        "reviewItemId": review_item_id,
        "action": action,
        "owner": owner.map(|owner| serde_json::json!({
            "ownerType": owner.owner_type.as_str(),
            "ownerId": owner.owner_id,
        })),
        "ownerRevision": owner_revision,
        "impactedConsumerIds": impacted_consumer_ids,
        "newRefId": new_ref_id,
    }))
    .map_err(|error| ApiError::InternalError(format!("Failed to canonicalize impact preview: {error}")))?;
    Ok(format!("impact_sha256:{:x}", Sha256::digest(canonical)))
}

async fn mutate_owner_relationship(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    owner: &SurfaceReviewOwner,
    old_ref_id: &CapabilityRefId,
    action: &SurfaceIntentResolutionActionData,
    new_ref_id: Option<&str>,
) -> Result<(), ApiError> {
    let server_id: String = sqlx::query_scalar("SELECT server_id FROM capability_refs WHERE ref_id = ?")
        .bind(old_ref_id.as_str())
        .fetch_one(&mut **transaction)
        .await
        .map_err(database_error)?;
    let result = match (owner.owner_type, action) {
        (
            ReviewOwnerType::StandardProfile | ReviewOwnerType::CustomProfile,
            SurfaceIntentResolutionActionData::RemoveIntent,
        ) => sqlx::query("DELETE FROM profile_capability_refs WHERE profile_id = ? AND ref_id = ?")
            .bind(&owner.owner_id)
            .bind(old_ref_id.as_str())
            .execute(&mut **transaction)
            .await
            .map_err(database_error)?,
        (ReviewOwnerType::ProfileServerExposure, SurfaceIntentResolutionActionData::RemoveIntent) => {
            sqlx::query("DELETE FROM profile_server_relationships WHERE profile_id = ? AND server_id = ?")
                .bind(&owner.owner_id)
                .bind(&server_id)
                .execute(&mut **transaction)
                .await
                .map_err(database_error)?
        }
        (ReviewOwnerType::ConsumerDirectExposure, SurfaceIntentResolutionActionData::RemoveIntent) => {
            sqlx::query("DELETE FROM direct_exposure_refs WHERE consumer_id = ? AND ref_id = ?")
                .bind(&owner.owner_id)
                .bind(old_ref_id.as_str())
                .execute(&mut **transaction)
                .await
                .map_err(database_error)?
        }
        (ReviewOwnerType::ConsumerServerExposure, SurfaceIntentResolutionActionData::RemoveIntent) => {
            sqlx::query("DELETE FROM direct_exposure_servers WHERE consumer_id = ? AND server_id = ?")
                .bind(&owner.owner_id)
                .bind(&server_id)
                .execute(&mut **transaction)
                .await
                .map_err(database_error)?
        }
        (
            ReviewOwnerType::StandardProfile | ReviewOwnerType::CustomProfile,
            SurfaceIntentResolutionActionData::RebindRef,
        ) => {
            let new_ref_id = validate_rebind_target(transaction, old_ref_id, new_ref_id).await?;
            sqlx::query("UPDATE profile_capability_refs SET ref_id = ? WHERE profile_id = ? AND ref_id = ?")
                .bind(new_ref_id.as_str())
                .bind(&owner.owner_id)
                .bind(old_ref_id.as_str())
                .execute(&mut **transaction)
                .await
                .map_err(database_error)?
        }
        (ReviewOwnerType::ConsumerDirectExposure, SurfaceIntentResolutionActionData::RebindRef) => {
            let new_ref_id = validate_rebind_target(transaction, old_ref_id, new_ref_id).await?;
            sqlx::query("UPDATE direct_exposure_refs SET ref_id = ? WHERE consumer_id = ? AND ref_id = ?")
                .bind(new_ref_id.as_str())
                .bind(&owner.owner_id)
                .bind(old_ref_id.as_str())
                .execute(&mut **transaction)
                .await
                .map_err(database_error)?
        }
        (
            ReviewOwnerType::ProfileServerExposure | ReviewOwnerType::ConsumerServerExposure,
            SurfaceIntentResolutionActionData::RebindRef,
        ) => {
            return Err(ApiError::BadRequest(
                "Server-level relationships cannot be rebound to a single Capability Ref".to_string(),
            ));
        }
        _ => {
            return Err(ApiError::BadRequest(
                "Unsupported Owner and Intent action combination".to_string(),
            ));
        }
    };
    if result.rows_affected() != 1 {
        return Err(ApiError::Conflict(
            "Selected Owner relationship changed before commit".to_string(),
        ));
    }
    Ok(())
}

async fn validate_rebind_target(
    transaction: &mut sqlx::Transaction<'_, Sqlite>,
    old_ref_id: &CapabilityRefId,
    new_ref_id: Option<&str>,
) -> Result<CapabilityRefId, ApiError> {
    let new_ref_id: CapabilityRefId = new_ref_id
        .ok_or_else(|| ApiError::BadRequest("Rebind requires new_ref_id".to_string()))?
        .parse()
        .map_err(|error| ApiError::BadRequest(format!("Invalid new_ref_id: {error}")))?;
    let matches: i64 = sqlx::query_scalar(
        r#"
        SELECT COUNT(*)
        FROM capability_refs old
        JOIN capability_refs new
          ON new.server_id = old.server_id
         AND new.kind = old.kind
         AND new.state = 'active'
        WHERE old.ref_id = ? AND new.ref_id = ?
        "#,
    )
    .bind(old_ref_id.as_str())
    .bind(new_ref_id.as_str())
    .fetch_one(&mut **transaction)
    .await
    .map_err(database_error)?;
    if matches != 1 {
        return Err(ApiError::BadRequest(
            "Rebind target must be an active Ref from the same Server and Capability kind".to_string(),
        ));
    }
    Ok(new_ref_id)
}

fn surface_review_owner(data: &SurfaceReviewOwnerData) -> SurfaceReviewOwner {
    SurfaceReviewOwner::new(review_owner_type(data.owner_type.clone()), &data.owner_id)
}

fn surface_review_owner_data(owner: SurfaceReviewOwner) -> SurfaceReviewOwnerData {
    SurfaceReviewOwnerData {
        owner_type: review_owner_type_data(owner.owner_type),
        owner_id: owner.owner_id,
    }
}

fn database_pool(state: &AppState) -> Result<&Pool<Sqlite>, ApiError> {
    state
        .database
        .as_ref()
        .map(|database| &database.pool)
        .ok_or_else(|| ApiError::ServiceUnavailable("Database is unavailable".to_string()))
}

async fn review_item_data(
    pool: &Pool<Sqlite>,
    record: SurfaceReviewRecord,
) -> Result<SurfaceReviewItemData, ApiError> {
    let store = SqliteSurfaceStore::new(pool.clone());
    let binding_generation = store
        .load_binding(&record.item.consumer_id)
        .await
        .map_err(store_error)?
        .map(|binding| binding.generation);
    if binding_generation.is_none() && record.item.lifecycle != ReviewLifecycle::Obsolete {
        return Err(ApiError::Conflict(format!(
            "Consumer '{}' has no active Surface publication",
            record.item.consumer_id
        )));
    }
    let before_record = load_effective_record(pool, record.item.before_capability_id.as_ref()).await?;
    let target_record = load_effective_record(pool, record.item.target_capability_id.as_ref()).await?;
    let field_diff = field_diff(before_record.as_ref(), target_record.as_ref());
    Ok(SurfaceReviewItemData {
        review_item_id: record.item.review_item_id,
        proposal_id: record.item.created_by_proposal_id,
        consumer_id: record.item.consumer_id,
        binding_generation,
        ref_id: record.item.ref_id.to_string(),
        before_capability_id: record.item.before_capability_id.map(|id| id.to_string()),
        target_capability_id: record.item.target_capability_id.map(|id| id.to_string()),
        target_key: record.item.target_key.to_string(),
        change_class: record.item.change_class,
        policy_action: record.item.policy_action,
        lifecycle: review_lifecycle_data(record.item.lifecycle),
        owners: record
            .owners
            .into_iter()
            .map(|owner| SurfaceReviewOwnerData {
                owner_type: review_owner_type_data(owner.owner_type),
                owner_id: owner.owner_id,
            })
            .collect(),
        current_decision: record.current_decision.map(|decision| SurfaceReviewDecisionData {
            decision_id: decision.decision_id,
            resolution_action: review_resolution_action_data(decision.resolution_action),
            resolution_payload: decision.resolution_payload,
            actor: decision.actor,
            decided_at: decision.decided_at.to_rfc3339(),
            supersedes_decision_id: decision.supersedes_decision_id,
        }),
        before_record,
        target_record,
        field_diff,
        created_at: record.created_at.to_rfc3339(),
        updated_at: record.updated_at.to_rfc3339(),
    })
}

async fn load_effective_record(
    pool: &Pool<Sqlite>,
    capability_id: Option<&CapabilityId>,
) -> Result<Option<Value>, ApiError> {
    let Some(capability_id) = capability_id else {
        return Ok(None);
    };
    let bytes: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT canonical_record FROM capability_versions WHERE capability_id = ?")
            .bind(capability_id.as_str())
            .fetch_optional(pool)
            .await
            .map_err(database_error)?;
    let bytes = bytes.ok_or_else(|| {
        ApiError::InternalError(format!(
            "Capability record '{}' referenced by review item was not found",
            capability_id
        ))
    })?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| ApiError::InternalError(format!("Failed to decode Capability record: {error}")))
}

fn field_diff(
    before: Option<&Value>,
    target: Option<&Value>,
) -> Vec<SurfaceReviewFieldDiffData> {
    let mut output = Vec::new();
    collect_field_diff("", before, target, &mut output);
    output
}

fn collect_field_diff(
    path: &str,
    before: Option<&Value>,
    target: Option<&Value>,
    output: &mut Vec<SurfaceReviewFieldDiffData>,
) {
    if before == target {
        return;
    }
    match (before, target) {
        (Some(Value::Object(before)), Some(Value::Object(target))) => {
            let keys = before
                .keys()
                .chain(target.keys())
                .collect::<std::collections::BTreeSet<_>>();
            for key in keys {
                collect_field_diff(
                    &format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
                    before.get(key),
                    target.get(key),
                    output,
                );
            }
        }
        _ => output.push(SurfaceReviewFieldDiffData {
            path: if path.is_empty() {
                "/".to_string()
            } else {
                path.to_string()
            },
            before: before.cloned(),
            target: target.cloned(),
        }),
    }
}

fn review_lifecycle(value: SurfaceReviewLifecycleData) -> ReviewLifecycle {
    match value {
        SurfaceReviewLifecycleData::Pending => ReviewLifecycle::Pending,
        SurfaceReviewLifecycleData::Resolved => ReviewLifecycle::Resolved,
        SurfaceReviewLifecycleData::Obsolete => ReviewLifecycle::Obsolete,
    }
}

fn review_lifecycle_data(value: ReviewLifecycle) -> SurfaceReviewLifecycleData {
    match value {
        ReviewLifecycle::Pending => SurfaceReviewLifecycleData::Pending,
        ReviewLifecycle::Resolved => SurfaceReviewLifecycleData::Resolved,
        ReviewLifecycle::Obsolete => SurfaceReviewLifecycleData::Obsolete,
    }
}

fn review_owner_type(value: SurfaceReviewOwnerTypeData) -> ReviewOwnerType {
    match value {
        SurfaceReviewOwnerTypeData::StandardProfile => ReviewOwnerType::StandardProfile,
        SurfaceReviewOwnerTypeData::CustomProfile => ReviewOwnerType::CustomProfile,
        SurfaceReviewOwnerTypeData::ConsumerDirectExposure => ReviewOwnerType::ConsumerDirectExposure,
        SurfaceReviewOwnerTypeData::ProfileServerExposure => ReviewOwnerType::ProfileServerExposure,
        SurfaceReviewOwnerTypeData::ConsumerServerExposure => ReviewOwnerType::ConsumerServerExposure,
        SurfaceReviewOwnerTypeData::ModeRule => ReviewOwnerType::ModeRule,
    }
}

fn review_owner_type_data(value: ReviewOwnerType) -> SurfaceReviewOwnerTypeData {
    match value {
        ReviewOwnerType::StandardProfile => SurfaceReviewOwnerTypeData::StandardProfile,
        ReviewOwnerType::CustomProfile => SurfaceReviewOwnerTypeData::CustomProfile,
        ReviewOwnerType::ConsumerDirectExposure => SurfaceReviewOwnerTypeData::ConsumerDirectExposure,
        ReviewOwnerType::ProfileServerExposure => SurfaceReviewOwnerTypeData::ProfileServerExposure,
        ReviewOwnerType::ConsumerServerExposure => SurfaceReviewOwnerTypeData::ConsumerServerExposure,
        ReviewOwnerType::ModeRule => SurfaceReviewOwnerTypeData::ModeRule,
    }
}

fn review_resolution_action_data(value: ReviewResolutionAction) -> SurfaceReviewResolutionActionData {
    match value {
        ReviewResolutionAction::ApproveTarget => SurfaceReviewResolutionActionData::ApproveTarget,
        ReviewResolutionAction::RejectTarget => SurfaceReviewResolutionActionData::RejectTarget,
        ReviewResolutionAction::KeepIntent => SurfaceReviewResolutionActionData::KeepIntent,
        ReviewResolutionAction::RemoveIntent => SurfaceReviewResolutionActionData::RemoveIntent,
        ReviewResolutionAction::RebindRef => SurfaceReviewResolutionActionData::RebindRef,
    }
}

fn review_resolution_action_name(value: ReviewResolutionAction) -> &'static str {
    match value {
        ReviewResolutionAction::ApproveTarget => "approve_target",
        ReviewResolutionAction::RejectTarget => "reject_target",
        ReviewResolutionAction::KeepIntent => "keep_intent",
        ReviewResolutionAction::RemoveIntent => "remove_intent",
        ReviewResolutionAction::RebindRef => "rebind_ref",
    }
}

fn rollback_block_message(block: &RollbackBlock) -> String {
    format!(
        "{}: pinned {}, current {}",
        block.ref_id,
        block.pinned_capability_id,
        block
            .current_capability_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_else(|| "unavailable".to_string())
    )
}

fn store_error(error: mcpmate_capability_store::CatalogError) -> ApiError {
    match error {
        mcpmate_capability_store::CatalogError::ConcurrencyConflict { .. } => ApiError::Conflict(error.to_string()),
        mcpmate_capability_store::CatalogError::SurfaceNotFound { .. } => ApiError::NotFound(error.to_string()),
        _ => ApiError::InternalError(error.to_string()),
    }
}

fn database_error(error: sqlx::Error) -> ApiError {
    ApiError::InternalError(format!("Surface review database operation failed: {error}"))
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, time::Duration};

    use mcpmate_capability_store::{
        CapabilityCatalog, CapabilityKind, CapabilityObservation, CapabilityPayload, CatalogRecord, DeclarationState,
        InventoryState, KindObservation, ReviewOwnerType, ReviewTargetKey, SqliteCapabilityCatalog, SurfaceManifest,
        SurfaceManifestId, SurfaceProposal, SurfacePublication, SurfaceReviewItemDraft, SurfaceReviewOwner,
    };
    use rmcp::model::{InitializeResult, Tool};
    use serde_json::json;
    use sqlx::sqlite::SqlitePoolOptions;
    use tokio::sync::{Mutex, RwLock};

    use crate::{
        api::routes::AppState,
        config::database::Database,
        core::{models::Config, pool::UpstreamConnectionPool, profile::ConfigApplicationStateManager},
        inspector::{calls::InspectorCallRegistry, sessions::InspectorSessionManager},
        system::metrics::MetricsCollector,
    };

    use super::*;

    fn tool_record(description: &str) -> CatalogRecord {
        let tool: Tool = serde_json::from_value(json!({
            "name": "analyze",
            "description": description,
            "inputSchema": {"type": "object"}
        }))
        .expect("tool fixture");
        CatalogRecord::materialize("server-a", "analyze", "fixture__analyze", CapabilityPayload::Tool(tool))
            .expect("materialize fixture")
    }

    fn observation(record: CatalogRecord) -> CapabilityObservation {
        let initialize_result: InitializeResult = serde_json::from_value(json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {"tools": {"listChanged": true}},
            "serverInfo": {"name": "fixture", "version": "1.0.0"}
        }))
        .expect("initialize fixture");
        CapabilityObservation::new(
            "server-a",
            "fixture",
            "config-v1",
            initialize_result,
            vec![KindObservation::new(
                CapabilityKind::Tools,
                DeclarationState::Supported,
                InventoryState::Complete,
            )],
            vec![record],
        )
    }

    async fn review_fixture() -> (Arc<AppState>, String, String) {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite pool");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("server schema");
        crate::config::client::init::initialize_client_table(&pool)
            .await
            .expect("client schema");
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .expect("profile schema");
        let catalog = SqliteCapabilityCatalog::new(pool.clone());
        catalog.ensure_schema().await.expect("capability schema");
        sqlx::query(
            r#"
            INSERT INTO server_config (
                id, name, server_type, command, enabled,
                unify_direct_exposure_eligible
            ) VALUES ('server-a', 'fixture', 'stdio', '', 1, 1)
            "#,
        )
        .execute(&pool)
        .await
        .expect("server fixture");
        sqlx::query(
            r#"
            INSERT INTO client (
                id, name, identifier, config_mode, capability_source, unify_route_mode, approval_status
            ) VALUES (
                'consumer-a', 'Consumer A', 'consumer-a', 'unify', 'activated', 'capability_level', 'approved'
            )
            "#,
        )
        .execute(&pool)
        .await
        .expect("consumer fixture");

        let before = tool_record("version one");
        catalog
            .commit_observation(observation(before.clone()))
            .await
            .expect("initial observation");
        sqlx::query("INSERT INTO direct_exposure_refs (consumer_id, ref_id, enabled) VALUES (?, ?, 1)")
            .bind("consumer-a")
            .bind(before.ref_id.as_str())
            .execute(&pool)
            .await
            .expect("direct exposure fixture");
        let store = SqliteSurfaceStore::new(pool.clone());
        let initial_manifest = SurfaceManifest::compile("consumer-a", vec![]).expect("safe manifest");
        let mut transaction = pool.begin().await.expect("initial transaction");
        store
            .insert_manifest_in_transaction(&mut transaction, &initial_manifest)
            .await
            .expect("insert initial manifest");
        store
            .publish_and_bind_in_transaction(
                &mut transaction,
                &SurfacePublication::new(
                    "publication-safe",
                    "consumer-a",
                    initial_manifest.manifest_id,
                    None,
                    "safe_contraction",
                    "system",
                    None,
                ),
                None,
            )
            .await
            .expect("publish safe manifest");
        transaction.commit().await.expect("commit initial publication");

        let target = tool_record("version two");
        catalog
            .commit_observation(observation(target.clone()))
            .await
            .expect("target observation");
        let proposed_manifest = SurfaceManifest::compile(
            "consumer-a",
            vec![mcpmate_capability_store::SurfaceManifestEntryInput::new(
                target.ref_id.clone(),
                target.capability_id.clone(),
                target.kind(),
                target.external_key.clone(),
            )],
        )
        .expect("proposed manifest");
        let proposal = SurfaceProposal::new(
            "proposal-review",
            "consumer-a",
            Some("publication-safe".to_string()),
            proposed_manifest.manifest_id.clone(),
            "catalog_delta",
            "revision-2",
            json!({"server-a": 2}),
            json!({"reviewItems": 1}),
        );
        let draft = SurfaceReviewItemDraft::new(
            "review-target",
            "proposal-review",
            "consumer-a",
            target.ref_id,
            Some(before.capability_id),
            Some(target.capability_id.clone()),
            ReviewTargetKey::capability(&target.capability_id),
            "model_visible",
            "review",
        );
        let mut transaction = pool.begin().await.expect("review transaction");
        store
            .insert_manifest_in_transaction(&mut transaction, &proposed_manifest)
            .await
            .expect("insert proposed manifest");
        store
            .insert_proposal_in_transaction(&mut transaction, &proposal)
            .await
            .expect("insert proposal");
        store
            .create_or_reuse_review_item_in_transaction(
                &mut transaction,
                &draft,
                &[SurfaceReviewOwner::new(
                    ReviewOwnerType::ConsumerDirectExposure,
                    "consumer-a",
                )],
            )
            .await
            .expect("insert review item");
        transaction.commit().await.expect("commit review item");

        let database = Arc::new(Database {
            pool,
            path: PathBuf::from(":memory:"),
            capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
        });
        let state = Arc::new(AppState {
            connection_pool: Arc::new(Mutex::new(UpstreamConnectionPool::new(
                Arc::new(Config::default()),
                Some(database.clone()),
            ))),
            metrics_collector: Arc::new(MetricsCollector::new(Duration::from_secs(5))),
            http_proxy: None,
            profile_merge_service: None,
            database: Some(database),
            audit_database: None,
            audit_service: None,
            config_application_state: Arc::new(ConfigApplicationStateManager::new()),
            client_service: None,
            inspector_calls: Arc::new(InspectorCallRegistry::new()),
            inspector_sessions: Arc::new(InspectorSessionManager::new()),
            oauth_manager: RwLock::new(None),
            secret_store: RwLock::new(None),
            secret_store_readiness: RwLock::new(crate::api::routes::unavailable_secret_store_readiness("test")),
        });
        (
            state,
            "review-target".to_string(),
            format!("capability:{}", target.capability_id),
        )
    }

    async fn configure_profile_owned_missing_intent(
        state: &Arc<AppState>,
        review_item_id: &str,
    ) -> i64 {
        let pool = &state.database.as_ref().expect("test database").pool;
        let ref_id: String = sqlx::query_scalar("SELECT ref_id FROM surface_review_items WHERE review_item_id = ?")
            .bind(review_item_id)
            .fetch_one(pool)
            .await
            .expect("load reviewed ref");
        sqlx::query(
            "INSERT INTO profile (id, name, type, role, is_active, authoring_generation)
             VALUES ('profile-a', 'Profile A', 'shared', 'user', 1, 0)",
        )
        .execute(pool)
        .await
        .expect("insert Profile owner");
        sqlx::query(
            "INSERT INTO profile_capability_refs (profile_id, ref_id, enabled)
             VALUES ('profile-a', ?, 1)",
        )
        .bind(&ref_id)
        .execute(pool)
        .await
        .expect("insert Profile ref intent");
        sqlx::query(
            "UPDATE client
             SET config_mode = 'hosted', capability_source = 'profiles', selected_profile_ids = '[\"profile-a\"]'
             WHERE identifier = 'consumer-a'",
        )
        .execute(pool)
        .await
        .expect("select Profile for Consumer");
        sqlx::query(
            "UPDATE surface_review_owners
             SET owner_type = 'standard_profile', owner_id = 'profile-a'
             WHERE review_item_id = ? AND active = 1",
        )
        .bind(review_item_id)
        .execute(pool)
        .await
        .expect("replace review owner");
        let manifest_id: SurfaceManifestId = sqlx::query_scalar::<_, String>(
            "SELECT proposed_manifest_id FROM surface_proposals WHERE proposal_id = 'proposal-review'",
        )
        .fetch_one(pool)
        .await
        .expect("load reviewed manifest")
        .parse()
        .expect("parse reviewed manifest id");
        let store = SqliteSurfaceStore::new(pool.clone());
        let binding = store
            .load_binding("consumer-a")
            .await
            .expect("load initial binding")
            .expect("initial binding");
        let mut transaction = pool.begin().await.expect("bind Profile-owned manifest");
        store
            .publish_and_bind_in_transaction(
                &mut transaction,
                &SurfacePublication::new(
                    "publication-profile-current",
                    "consumer-a",
                    manifest_id,
                    None,
                    "profile_intent_fixture",
                    "test",
                    Some(binding.active_publication_id),
                ),
                Some(binding.generation),
            )
            .await
            .expect("publish Profile-owned surface");
        transaction.commit().await.expect("commit Profile-owned surface");
        sqlx::query("UPDATE capability_refs SET state = 'unresolved', state_generation = 1 WHERE ref_id = ?")
            .bind(&ref_id)
            .execute(pool)
            .await
            .expect("mark ref unresolved");
        sqlx::query(
            "UPDATE surface_review_items
             SET target_capability_id = NULL, target_key = 'missing:1', change_class = 'missing'
             WHERE review_item_id = ?",
        )
        .bind(review_item_id)
        .execute(pool)
        .await
        .expect("mark review missing");
        SqliteSurfaceStore::new(pool.clone())
            .load_binding("consumer-a")
            .await
            .expect("load Profile binding")
            .expect("active Profile binding")
            .generation
    }

    #[tokio::test]
    async fn managed_surface_revocation_obsoletes_reviews_without_breaking_projection() {
        let (state, review_item_id, _) = review_fixture().await;
        let pool = &state.database.as_ref().expect("test database").pool;
        let mut transaction = pool.begin().await.expect("revocation transaction");
        crate::core::capability::materializer::revoke_managed_surface_in_transaction(
            pool,
            &mut transaction,
            "consumer-a",
            "test-revocation",
        )
        .await
        .expect("revoke managed Surface");
        transaction.commit().await.expect("commit revocation");

        let pending = list_surface_reviews(
            State(state.clone()),
            Query(SurfaceReviewListQuery {
                consumer_id: None,
                owner_type: None,
                owner_id: None,
                state: Some(SurfaceReviewLifecycleData::Pending),
            }),
        )
        .await
        .expect("pending review projection remains available")
        .0
        .data
        .expect("pending review data");
        assert!(pending.items.is_empty());

        let obsolete = get_surface_review(
            State(state.clone()),
            Path(SurfaceReviewPath {
                review_item_id: review_item_id.clone(),
            }),
        )
        .await
        .expect("obsolete review remains readable")
        .0
        .data
        .expect("obsolete review data");
        assert_eq!(obsolete.lifecycle, SurfaceReviewLifecycleData::Obsolete);
        assert!(obsolete.owners.is_empty());
        assert!(obsolete.binding_generation.is_none());

        let active_owner_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM surface_review_owners WHERE review_item_id = ? AND active = 1")
                .bind(review_item_id)
                .fetch_one(pool)
                .await
                .expect("count active review owners");
        assert_eq!(active_owner_count, 0);
    }

    #[tokio::test]
    async fn review_detail_exposes_current_binding_generation() {
        let (state, review_item_id, _) = review_fixture().await;

        let response = get_surface_review(
            State(state),
            Path(SurfaceReviewPath {
                review_item_id: review_item_id.clone(),
            }),
        )
        .await
        .expect("load review detail");
        let data = response.0.data.expect("review detail data");

        assert_eq!(data.review_item_id, review_item_id);
        assert_eq!(data.binding_generation, Some(1));
    }

    #[tokio::test]
    async fn review_summary_exposes_failed_reconciliation_count() {
        let (state, _, _) = review_fixture().await;
        let pool = &state.database.as_ref().unwrap().pool;
        sqlx::query(
            r#"
            INSERT INTO surface_reconciliation_jobs (
                idempotency_key, cause_kind, cause_id, consumer_id,
                target_revision_set, expected_binding_generation, status,
                attempt_count, next_attempt_at, last_error, created_at, updated_at
            ) VALUES (
                'failed-job', 'catalog_delta', 'server-a:2', 'consumer-a',
                '{"server-a":2}', 1, 'failed', 3,
                CURRENT_TIMESTAMP, 'materialization failed',
                CURRENT_TIMESTAMP, CURRENT_TIMESTAMP
            )
            "#,
        )
        .execute(pool)
        .await
        .expect("failed reconciliation fixture");

        let summary = summarize_surface_reviews(State(state))
            .await
            .expect("load review summary")
            .0
            .data
            .expect("summary data");

        assert_eq!(summary.failed_reconciliation_count, 1);
    }

    #[tokio::test]
    async fn approve_target_publishes_only_after_target_and_binding_cas() {
        let (state, review_item_id, target_key) = review_fixture().await;
        let response = resolve_target_review(
            state.clone(),
            review_item_id.clone(),
            SurfaceReviewActionReq {
                expected_target_key: target_key.clone(),
                expected_binding_generation: 1,
                actor: "reviewer-a".to_string(),
            },
            ReviewResolutionAction::ApproveTarget,
        )
        .await
        .expect("approve target");
        let data = response.0.data.expect("action response data");
        assert_eq!(data.review_item_id, review_item_id);
        assert_eq!(data.binding_generation, 2);
        assert!(data.effective_surface_changed);

        let store = SqliteSurfaceStore::new(state.database.as_ref().unwrap().pool.clone());
        let binding = store
            .load_binding("consumer-a")
            .await
            .expect("load binding")
            .expect("active binding");
        assert_eq!(binding.generation, 2);
        let record = store
            .load_review_record("review-target")
            .await
            .expect("load review")
            .expect("review item");
        assert_eq!(record.item.lifecycle, ReviewLifecycle::Resolved);
        assert_eq!(
            record
                .current_decision
                .as_ref()
                .map(|decision| decision.resolution_action),
            Some(ReviewResolutionAction::ApproveTarget)
        );

        let stale = resolve_target_review(
            state,
            "review-target".to_string(),
            SurfaceReviewActionReq {
                expected_target_key: target_key,
                expected_binding_generation: 1,
                actor: "reviewer-b".to_string(),
            },
            ReviewResolutionAction::RejectTarget,
        )
        .await
        .expect_err("stale binding generation must conflict");
        assert!(matches!(stale, ApiError::Conflict(_)));
    }

    #[tokio::test]
    async fn intent_preview_is_owner_scoped_and_detects_stale_impact_tokens() {
        let (state, review_item_id, _) = review_fixture().await;
        let pool = &state.database.as_ref().unwrap().pool;
        sqlx::query(
            r#"
            UPDATE capability_refs
            SET state = 'unresolved', state_generation = 1
            WHERE ref_id = (SELECT ref_id FROM surface_review_items WHERE review_item_id = ?)
            "#,
        )
        .bind(&review_item_id)
        .execute(pool)
        .await
        .expect("mark ref unresolved");
        sqlx::query(
            r#"
            UPDATE surface_review_items
            SET target_capability_id = NULL, target_key = 'missing:1', change_class = 'missing'
            WHERE review_item_id = ?
            "#,
        )
        .bind(&review_item_id)
        .execute(pool)
        .await
        .expect("mark review missing");

        let preview = preview_intent_resolution(
            state.clone(),
            review_item_id.clone(),
            SurfaceIntentPreviewReq {
                action: SurfaceIntentResolutionActionData::RemoveIntent,
                owner: Some(SurfaceReviewOwnerData {
                    owner_type: SurfaceReviewOwnerTypeData::ConsumerDirectExposure,
                    owner_id: "consumer-a".to_string(),
                }),
                new_ref_id: None,
            },
        )
        .await
        .expect("preview remove intent")
        .0
        .data
        .expect("preview data");
        assert_eq!(preview.impacted_consumer_ids, vec!["consumer-a"]);
        assert!(!preview.owner_revision.is_empty());
        assert!(!preview.impact_token.is_empty());

        let stale = resolve_intent_review(
            state.clone(),
            review_item_id.clone(),
            SurfaceIntentResolveReq {
                action: SurfaceIntentResolutionActionData::RemoveIntent,
                owner: preview.owner.clone(),
                new_ref_id: None,
                expected_owner_revision: preview.owner_revision.clone(),
                impact_token: "stale-token".to_string(),
                expected_target_key: "missing:1".to_string(),
                expected_binding_generation: 1,
                actor: "reviewer-a".to_string(),
            },
        )
        .await
        .expect_err("stale impact token must conflict");
        assert!(matches!(stale, ApiError::Conflict(_)));
        let relationship_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM direct_exposure_refs WHERE consumer_id = 'consumer-a'")
                .fetch_one(pool)
                .await
                .expect("count direct relationship");
        assert_eq!(relationship_count, 1);

        let _ = resolve_intent_review(
            state.clone(),
            review_item_id,
            SurfaceIntentResolveReq {
                action: SurfaceIntentResolutionActionData::RemoveIntent,
                owner: preview.owner,
                new_ref_id: None,
                expected_owner_revision: preview.owner_revision,
                impact_token: preview.impact_token,
                expected_target_key: "missing:1".to_string(),
                expected_binding_generation: 1,
                actor: "reviewer-a".to_string(),
            },
        )
        .await
        .expect("remove intent");

        let payload: String = sqlx::query_scalar(
            "SELECT resolution_payload FROM surface_review_decisions WHERE resolution_action = 'remove_intent'",
        )
        .fetch_one(pool)
        .await
        .expect("load owner-scoped decision payload");
        let payload: Value = serde_json::from_str(&payload).expect("parse decision payload");
        assert_eq!(
            payload["owner"]["owner_type"],
            serde_json::json!("consumer_direct_exposure")
        );
        assert_eq!(payload["owner"]["owner_id"], serde_json::json!("consumer-a"));
    }

    #[tokio::test]
    async fn profile_intent_resolution_rejects_stale_authoring_then_advances_once_with_materialization() {
        let (state, review_item_id, _) = review_fixture().await;
        let binding_generation = configure_profile_owned_missing_intent(&state, &review_item_id).await;
        let owner = SurfaceReviewOwnerData {
            owner_type: SurfaceReviewOwnerTypeData::StandardProfile,
            owner_id: "profile-a".to_string(),
        };
        let preview = preview_intent_resolution(
            state.clone(),
            review_item_id.clone(),
            SurfaceIntentPreviewReq {
                action: SurfaceIntentResolutionActionData::RemoveIntent,
                owner: Some(owner.clone()),
                new_ref_id: None,
            },
        )
        .await
        .expect("preview Profile intent removal")
        .0
        .data
        .expect("preview data");
        let pool = &state.database.as_ref().unwrap().pool;
        sqlx::query("UPDATE profile SET authoring_generation = 1 WHERE id = 'profile-a'")
            .execute(pool)
            .await
            .expect("simulate concurrent Profile authoring");

        let stale = resolve_intent_review(
            state.clone(),
            review_item_id.clone(),
            SurfaceIntentResolveReq {
                action: SurfaceIntentResolutionActionData::RemoveIntent,
                owner: Some(owner.clone()),
                new_ref_id: None,
                expected_owner_revision: preview.owner_revision,
                impact_token: preview.impact_token,
                expected_target_key: "missing:1".to_string(),
                expected_binding_generation: binding_generation,
                actor: "reviewer-stale".to_string(),
            },
        )
        .await
        .expect_err("stale Profile authoring snapshot must conflict");
        assert!(matches!(stale, ApiError::Conflict(_)));

        let fresh = preview_intent_resolution(
            state.clone(),
            review_item_id.clone(),
            SurfaceIntentPreviewReq {
                action: SurfaceIntentResolutionActionData::RemoveIntent,
                owner: Some(owner.clone()),
                new_ref_id: None,
            },
        )
        .await
        .expect("refresh Profile intent preview")
        .0
        .data
        .expect("fresh preview data");
        let resolved = resolve_intent_review(
            state.clone(),
            review_item_id,
            SurfaceIntentResolveReq {
                action: SurfaceIntentResolutionActionData::RemoveIntent,
                owner: Some(owner),
                new_ref_id: None,
                expected_owner_revision: fresh.owner_revision,
                impact_token: fresh.impact_token,
                expected_target_key: "missing:1".to_string(),
                expected_binding_generation: binding_generation,
                actor: "reviewer-current".to_string(),
            },
        )
        .await
        .expect("resolve current Profile intent")
        .0
        .data
        .expect("resolution data");

        assert!(resolved.effective_surface_changed);
        assert_eq!(resolved.binding_generation, binding_generation + 1);
        let authoring_generation: i64 =
            sqlx::query_scalar("SELECT authoring_generation FROM profile WHERE id = 'profile-a'")
                .fetch_one(pool)
                .await
                .unwrap();
        let relationship_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM profile_capability_refs WHERE profile_id = 'profile-a'")
                .fetch_one(pool)
                .await
                .unwrap();
        assert_eq!((authoring_generation, relationship_count), (2, 0));
    }

    #[tokio::test]
    async fn rollback_rejects_stale_generation_and_rebinds_to_eligible_history() {
        let (state, review_item_id, target_key) = review_fixture().await;
        let _ = resolve_target_review(
            state.clone(),
            review_item_id,
            SurfaceReviewActionReq {
                expected_target_key: target_key,
                expected_binding_generation: 1,
                actor: "reviewer-a".to_string(),
            },
            ReviewResolutionAction::ApproveTarget,
        )
        .await
        .expect("approve target before rollback");

        let stale = rollback_surface_publication(
            State(state.clone()),
            Path(SurfacePublicationPath {
                publication_id: "publication-safe".to_string(),
            }),
            Json(SurfaceRollbackReq {
                consumer_id: "consumer-a".to_string(),
                expected_binding_generation: 1,
                actor: "reviewer-a".to_string(),
                reason: "restore safe surface".to_string(),
            }),
        )
        .await
        .expect_err("stale rollback generation must conflict");
        assert!(matches!(stale, ApiError::Conflict(_)));

        let response = rollback_surface_publication(
            State(state.clone()),
            Path(SurfacePublicationPath {
                publication_id: "publication-safe".to_string(),
            }),
            Json(SurfaceRollbackReq {
                consumer_id: "consumer-a".to_string(),
                expected_binding_generation: 2,
                actor: "reviewer-a".to_string(),
                reason: "restore safe surface".to_string(),
            }),
        )
        .await
        .expect("rollback eligible publication");
        let data = response.0.data.expect("rollback response");
        assert_eq!(data.rolled_back_to_publication_id, "publication-safe");
        assert_eq!(data.binding_generation, 3);
        let binding = SqliteSurfaceStore::new(state.database.as_ref().unwrap().pool.clone())
            .load_binding("consumer-a")
            .await
            .expect("load binding")
            .expect("active binding");
        assert_eq!(binding.active_publication_id, data.publication_id);
        assert_eq!(binding.generation, 3);
        let outbox_count: i64 = sqlx::query_scalar(
            r#"
            SELECT COUNT(*)
            FROM surface_outbox_events
            WHERE aggregate_id = 'consumer-a'
              AND event_kind = 'surface_publication_changed'
              AND payload LIKE '%"reason":"rollback"%'
            "#,
        )
        .fetch_one(&state.database.as_ref().unwrap().pool)
        .await
        .expect("load rollback outbox event");
        assert_eq!(outbox_count, 1);
    }
}
