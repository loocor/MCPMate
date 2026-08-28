use std::collections::HashMap;
use std::sync::Arc;

use mcpmate::config::models::ProfileMode;
use mcpmate::config::profile::get_active_profile;
use mcpmate::core::capability::management::{ProfileActivationAction, ProfileSurfaceManagement};
use mcpmate::core::capability::materializer::bootstrap_managed_surfaces;
use mcpmate::core::profile::authoring::{ProfileAuthoringCommand, ProfileAuthoringService};
use mcpmate::core::profile::publication::{
    SkillPackageDistribution, load_published_skill_packages, skill_catalog_summary,
};
use mcpmate::core::profile::workflow::{
    WorkflowBindingCommand, WorkflowBindingPolicy, WorkflowSpecificationSaveCommand, WorkflowSpecificationService,
    WorkflowStepCommand,
};
use mcpmate_capability_store::{
    CapabilityCatalog, CapabilityKind, CapabilityObservation, CapabilityPayload, CatalogRecord, DeclarationState,
    InventoryState, KindObservation, SqliteCapabilityCatalog,
};
use rmcp::model::{InitializeResult, Tool};
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;

#[path = "support/database.rs"]
mod database_support;

async fn pool() -> sqlx::SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("connect test database");
    database_support::prepare_config(&pool).await;
    pool
}

fn tool_record(
    server_id: &str,
    name: &str,
) -> CatalogRecord {
    CatalogRecord::materialize(
        server_id,
        name,
        format!("{}__{name}", server_id.replace('-', "_")),
        CapabilityPayload::Tool(Tool::new(
            name.to_string(),
            format!("{name} description"),
            Arc::new(json!({"type": "object"}).as_object().unwrap().clone()),
        )),
    )
    .expect("materialize fixture tool")
}

async fn add_server(pool: &sqlx::SqlitePool) -> Vec<String> {
    sqlx::query(
        "INSERT INTO server_config (id, name, server_type, command, enabled)
         VALUES ('server-a', 'Server A', 'stdio', '', 1)",
    )
    .execute(pool)
    .await
    .expect("insert server");
    let records = vec![tool_record("server-a", "lookup"), tool_record("server-a", "search")];
    let ref_ids = records
        .iter()
        .map(|record| record.ref_id.to_string())
        .collect::<Vec<_>>();
    let initialize: InitializeResult = serde_json::from_value(json!({
        "protocolVersion": "2025-11-25",
        "capabilities": {"tools": {"listChanged": true}},
        "serverInfo": {"name": "Server A", "version": "1.0.0"}
    }))
    .expect("initialize");
    SqliteCapabilityCatalog::new(pool.clone())
        .commit_observation(CapabilityObservation::new(
            "server-a",
            "Server A",
            "config-v1",
            initialize,
            vec![KindObservation::new(
                CapabilityKind::Tools,
                DeclarationState::Supported,
                InventoryState::Complete,
            )],
            records,
        ))
        .await
        .expect("observe server");
    ref_ids
}

async fn add_client(
    pool: &sqlx::SqlitePool,
    identifier: &str,
    config_mode: &str,
) {
    sqlx::query(
        r#"
        INSERT INTO client (
            id, identifier, name, config_mode, approval_status, capability_source, selected_profile_ids
        ) VALUES (?, ?, ?, ?, 'approved', 'activated', '[]')
        "#,
    )
    .bind(identifier)
    .bind(identifier)
    .bind(identifier)
    .bind(config_mode)
    .execute(pool)
    .await
    .expect("insert client");
}

async fn published_ref_ids(
    pool: &sqlx::SqlitePool,
    consumer_id: &str,
) -> Vec<String> {
    sqlx::query_scalar(
        r#"
        SELECT entry.ref_id
        FROM consumer_surface_bindings binding
        JOIN surface_publications publication
          ON publication.publication_id = binding.active_publication_id
        JOIN surface_manifest_entries entry ON entry.manifest_id = publication.manifest_id
        WHERE binding.consumer_id = ?
        ORDER BY entry.ref_id
        "#,
    )
    .bind(consumer_id)
    .fetch_all(pool)
    .await
    .expect("load published refs")
}

fn workflow_command(is_active: bool) -> ProfileAuthoringCommand {
    ProfileAuthoringCommand {
        id: None,
        expected_authoring_generation: None,
        name: "Release flow".to_string(),
        description: Some("Ship a tagged build".to_string()),
        profile_type: "shared".to_string(),
        priority: 0,
        is_active,
        is_default: false,
        server_ids: vec!["server-a".to_string()],
        clone_from_id: None,
        profile_mode: Some(ProfileMode::Workflow),
        skill_name: Some("release-flow".to_string()),
        package_distribution: None,
        workflow_guidance: None,
    }
}

#[tokio::test]
async fn published_workflow_directs_compile_for_unify_and_stay_out_of_hosted_surface() {
    let pool = pool().await;
    let ref_ids = add_server(&pool).await;
    add_client(&pool, "unify-client", "unify").await;
    add_client(&pool, "hosted-client", "hosted").await;
    bootstrap_managed_surfaces(&pool).await.expect("bootstrap");
    let hosted_before = published_ref_ids(&pool, "hosted-client").await;

    let created = ProfileAuthoringService::with_skills_root(pool.clone(), tempfile::tempdir().unwrap().keep())
        .save(workflow_command(false), "test")
        .await
        .expect("create workflow");
    let profile_id = created.profile.id.clone().expect("profile id");
    WorkflowSpecificationService::new(pool.clone())
        .save(WorkflowSpecificationSaveCommand {
            profile_id: profile_id.clone(),
            expected_specification_revision: None,
            validation_notes: None,
            avoid_rules: None,
            steps: vec![
                WorkflowStepCommand {
                    step_id: None,
                    title: "Prepare".to_string(),
                    description: None,
                    bindings: vec![WorkflowBindingCommand {
                        ref_id: ref_ids[0].clone(),
                        binding_policy: WorkflowBindingPolicy::MetaOnDemand,
                    }],
                },
                WorkflowStepCommand {
                    step_id: None,
                    title: "Publish".to_string(),
                    description: None,
                    bindings: vec![WorkflowBindingCommand {
                        ref_id: ref_ids[1].clone(),
                        binding_policy: WorkflowBindingPolicy::Direct,
                    }],
                },
            ],
        })
        .await
        .expect("save unpublished specification");

    ProfileSurfaceManagement::set_profiles_active(
        &pool,
        std::slice::from_ref(&profile_id),
        ProfileActivationAction::Activate,
        HashMap::from([(profile_id.clone(), created.profile.authoring_generation)]),
        "test",
    )
    .await
    .expect("publish workflow");

    let unify_refs = published_ref_ids(&pool, "unify-client").await;
    assert!(
        unify_refs.contains(&ref_ids[1]),
        "Direct search must appear on Unify Active Surface: {unify_refs:?}"
    );
    assert!(
        !unify_refs.contains(&ref_ids[0]),
        "Meta lookup must stay off Unify Active Surface: {unify_refs:?}"
    );
    assert_eq!(published_ref_ids(&pool, "hosted-client").await, hosted_before);
    let packages = load_published_skill_packages(&pool)
        .await
        .expect("load published packages");
    assert_eq!(packages[0].direct_capabilities.len(), 1);
    assert_eq!(packages[0].direct_capabilities[0].name, "server_a__search");
    assert_eq!(packages[0].meta_capabilities[0].name, "server_a__lookup");

    WorkflowSpecificationService::new(pool.clone())
        .save(WorkflowSpecificationSaveCommand {
            profile_id: profile_id.clone(),
            expected_specification_revision: Some(0),
            validation_notes: None,
            avoid_rules: None,
            steps: vec![WorkflowStepCommand {
                step_id: None,
                title: "Publish".to_string(),
                description: None,
                bindings: vec![WorkflowBindingCommand {
                    ref_id: ref_ids[0].clone(),
                    binding_policy: WorkflowBindingPolicy::Direct,
                }],
            }],
        })
        .await
        .expect("change specification after publish");
    let unify_after = published_ref_ids(&pool, "unify-client").await;
    assert!(
        unify_after.contains(&ref_ids[0]),
        "spec save must rematerialize the new Direct: {unify_after:?}"
    );
    assert!(
        !unify_after.contains(&ref_ids[1]),
        "removed Direct must leave Unify Active Surface: {unify_after:?}"
    );

    ProfileSurfaceManagement::set_profiles_active(
        &pool,
        std::slice::from_ref(&profile_id),
        ProfileActivationAction::Deactivate,
        HashMap::from([(profile_id.clone(), 1)]),
        "test",
    )
    .await
    .expect("unpublish workflow");
    let unify_unpublished = published_ref_ids(&pool, "unify-client").await;
    assert!(
        !unify_unpublished.contains(&ref_ids[0]),
        "unpublish must drop Workflow Directs: {unify_unpublished:?}"
    );
}

#[tokio::test]
async fn unpublished_packages_and_unset_distribution_do_not_mount() {
    let pool = pool().await;
    let _ref_ids = add_server(&pool).await;
    let skills_root = tempfile::tempdir().unwrap().keep();
    let service = ProfileAuthoringService::with_skills_root(pool.clone(), skills_root);
    let created = service
        .save(workflow_command(true), "test")
        .await
        .expect("create published workflow");
    let profile_id = created.profile.id.expect("profile id");
    WorkflowSpecificationService::new(pool.clone())
        .save(WorkflowSpecificationSaveCommand {
            profile_id: profile_id.clone(),
            expected_specification_revision: None,
            validation_notes: None,
            avoid_rules: None,
            steps: vec![WorkflowStepCommand {
                step_id: None,
                title: "Prepare".to_string(),
                description: None,
                bindings: Vec::new(),
            }],
        })
        .await
        .expect("save step title");

    let packages = load_published_skill_packages(&pool).await.expect("load packages");
    assert_eq!(packages.len(), 1);
    assert_eq!(packages[0].skill_name, "release-flow");
    assert_eq!(packages[0].distribution, None);
    assert_eq!(packages[0].step_titles, vec!["Prepare".to_string()]);
    assert!(skill_catalog_summary(packages[0].description.as_deref(), &packages[0].step_titles,).contains("Prepare"));

    let created = service
        .save(
            ProfileAuthoringCommand {
                id: Some(profile_id.clone()),
                expected_authoring_generation: Some(created.profile.authoring_generation),
                package_distribution: Some(SkillPackageDistribution::Copy),
                ..workflow_command(true)
            },
            "test",
        )
        .await
        .expect("set explicit distribution");
    let packages = load_published_skill_packages(&pool).await.expect("reload packages");
    assert_eq!(packages[0].distribution, Some(SkillPackageDistribution::Copy));

    service
        .save(
            ProfileAuthoringCommand {
                id: Some(profile_id),
                expected_authoring_generation: Some(created.profile.authoring_generation),
                package_distribution: None,
                ..workflow_command(true)
            },
            "test",
        )
        .await
        .expect("clear distribution");
    let packages = load_published_skill_packages(&pool).await.expect("reload after clear");
    assert_eq!(packages[0].distribution, None);
}

#[tokio::test]
async fn published_workflow_stays_out_of_capability_working_set() {
    let pool = pool().await;
    let _ref_ids = add_server(&pool).await;
    ProfileAuthoringService::with_skills_root(pool.clone(), tempfile::tempdir().unwrap().keep())
        .save(workflow_command(true), "test")
        .await
        .expect("publish workflow");

    let active = get_active_profile(&pool).await.expect("load working-set profiles");
    assert!(
        active
            .iter()
            .all(|profile| profile.profile_mode != ProfileMode::Workflow),
        "published Workflow must not appear in Hosted/Transparent Active: {active:?}"
    );
}
