use std::{collections::HashMap, path::PathBuf, sync::Arc};

use axum::{
    Json,
    extract::{Query, State},
};
use mcpmate::{
    api::{
        handlers::profile::workflow_specification_preview,
        models::{profile::ProfileIdReq, resp::ResponseConverter},
        routes::{AppState, unavailable_secret_store_readiness},
    },
    config::{database::Database, models::ProfileMode},
    core::{
        capability::management::{ProfileActivationAction, ProfileSurfaceManagement},
        pool::UpstreamConnectionPool,
        profile::{
            authoring::{ProfileAuthoringCommand, ProfileAuthoringService},
            guide::{
                WorkflowGuideError, WorkflowGuidePackageCategory, WorkflowGuidePackageFileSaveCommand,
                WorkflowGuidePreviewCommand, WorkflowGuideReclamationConfirmation, WorkflowGuideSaveCommand,
                WorkflowGuideService,
            },
            materials::{
                WorkflowMaterialKind, WorkflowMaterialSaveCommand, WorkflowMaterialsReorderCommand,
                WorkflowMaterialsService, WorkflowStepMaterialsSaveCommand,
            },
            workflow::{
                WorkflowBindingCommand, WorkflowBindingPolicy, WorkflowBindingValidation, WorkflowSpecificationError,
                WorkflowSpecificationSaveCommand, WorkflowSpecificationService, WorkflowStepCommand,
            },
        },
        proxy::ProxyServer,
    },
    inspector::{calls::InspectorCallRegistry, service as inspector_service, sessions::InspectorSessionManager},
    system::metrics::MetricsCollector,
};
use mcpmate_capability_store::{
    CapabilityCatalog, CapabilityKind, CapabilityObservation, CapabilityPayload, CatalogRecord, DeclarationState,
    InventoryState, KindObservation, SqliteCapabilityCatalog,
};
use rmcp::model::{InitializeResult, Tool};
use serde_json::json;
use sqlx::sqlite::SqlitePoolOptions;
use tokio::sync::{Mutex, RwLock};

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

#[tokio::test]
async fn profile_authoring_saves_workflow_guidance_without_replacing_guide_steps() {
    let pool = pool().await;
    add_server(&pool, "server-a", "Server A", 1).await;
    let profile = workflow_authoring_service(&pool)
        .save(workflow_profile_command(), "test")
        .await
        .expect("create Workflow Profile");
    let profile_id = profile.profile.id.expect("created Workflow Profile ID");
    let skills_root = tempfile::tempdir().expect("create temporary Skills root");
    let guide = WorkflowGuideService::new(pool.clone());
    guide.view(&profile_id).await.expect("initialize Workflow Guide");
    guide
        .save_and_project(
            WorkflowGuideSaveCommand {
                profile_id: profile_id.clone(),
                expected_guide_revision: 0,
                markdown: "# Investigate incident\n\n:::capability {\"name\":\"server_a__lookup\",\"exposure\":\"direct\"}\nCollect evidence before deciding.\n:::\n".to_string(),
                reclamation_confirmation: None,
            },
            skills_root.path().to_path_buf(),
        )
        .await
        .expect("save Guide-derived Workflow Step");
    let specification_before = WorkflowSpecificationService::new(pool.clone())
        .view(&profile_id)
        .await
        .expect("load Workflow specification before guidance save");
    assert_eq!(specification_before.steps.len(), 1);

    let mut command = workflow_profile_command();
    command.id = Some(profile_id.clone());
    command.expected_authoring_generation = Some(profile.profile.authoring_generation);
    command.workflow_guidance = Some(mcpmate::core::profile::workflow::WorkflowGuidanceSaveCommand {
        expected_specification_revision: specification_before.specification_revision,
        validation_notes: Some("Verify all release evidence.".to_string()),
        avoid_rules: Some("Do not infer unpublished release content.".to_string()),
    });

    workflow_authoring_service(&pool)
        .save(command, "test")
        .await
        .expect("save Workflow Profile guidance");
    let specification = WorkflowSpecificationService::new(pool)
        .view(&profile_id)
        .await
        .expect("load Workflow guidance");

    assert_eq!(
        specification.validation_notes.as_deref(),
        Some("Verify all release evidence.")
    );
    assert_eq!(
        specification.avoid_rules.as_deref(),
        Some("Do not infer unpublished release content.")
    );
    assert_eq!(specification.steps, specification_before.steps);
}

#[tokio::test]
async fn projected_skill_frontmatter_reflects_bound_server_compatibility() {
    let pool = pool().await;
    add_server(&pool, "server-a", "Server A", 1).await;
    let profile = workflow_authoring_service(&pool)
        .save(workflow_profile_command(), "test")
        .await
        .expect("create workflow Profile");
    let profile_id = profile.profile.id.expect("created workflow Profile ID");
    let skills_root = tempfile::tempdir().expect("create temporary Skills root");
    let guide = WorkflowGuideService::new(pool.clone());
    guide.view(&profile_id).await.expect("initialize Workflow Guide");

    let without_bindings = guide
        .save_and_project(
            WorkflowGuideSaveCommand {
                profile_id: profile_id.clone(),
                expected_guide_revision: 0,
                markdown: "# Investigate incident\n".to_string(),
                reclamation_confirmation: None,
            },
            skills_root.path().to_path_buf(),
        )
        .await
        .expect("save Guide without capability bindings");
    assert!(!without_bindings.projected_skill.markdown.contains("compatibility:"));

    let with_bindings = guide
        .save_and_project(
            WorkflowGuideSaveCommand {
                profile_id: profile_id.clone(),
                expected_guide_revision: without_bindings.guide.guide_revision,
                markdown: "# Investigate incident\n\n:::capability {\"name\":\"server_a__lookup\",\"exposure\":\"direct\"}\nCollect evidence before deciding.\n:::\n".to_string(),
                reclamation_confirmation: None,
            },
            skills_root.path().to_path_buf(),
        )
        .await
        .expect("save Guide with capability bindings");
    assert!(
        with_bindings
            .projected_skill
            .markdown
            .contains("compatibility: Requires Server A via MCPMate.")
    );

    let skill_path = skills_root.path().join("investigate-incident/SKILL.md");
    let skill_on_disk = std::fs::read_to_string(&skill_path).expect("read projected SKILL.md");
    assert!(skill_on_disk.contains("compatibility: Requires Server A via MCPMate."));

    let preview = guide
        .preview(WorkflowGuidePreviewCommand {
            profile_id,
            relative_path: None,
            markdown: with_bindings.guide.markdown.clone(),
        })
        .await
        .expect("preview projected Skill");
    assert_eq!(preview.projected_skill.markdown, with_bindings.projected_skill.markdown);
}

#[tokio::test]
async fn workflow_guide_capability_binding_projects_readable_skill_and_normalizes_step_policy() {
    let pool = pool().await;
    let ref_ids = add_server(&pool, "server-a", "Server A", 1).await;
    let profile = workflow_authoring_service(&pool)
        .save(workflow_profile_command(), "test")
        .await
        .expect("create workflow Profile");
    let profile_id = profile.profile.id.expect("created workflow Profile ID");
    let skills_root = tempfile::tempdir().expect("create temporary Skills root");
    let guide = WorkflowGuideService::new(pool.clone());
    guide.view(&profile_id).await.expect("initialize Workflow Guide");
    let saved = guide
        .save_and_project(
            WorkflowGuideSaveCommand {
                profile_id: profile_id.clone(),
                expected_guide_revision: 0,
                markdown: "# Investigate incident\n\n:::capability {\"name\":\"server_a__lookup\",\"exposure\":\"direct\"}\nUse it before deciding.\n:::\n".to_string(),
                reclamation_confirmation: None,
            },
            skills_root.path().to_path_buf(),
        )
        .await
        .expect("save and project Guide with a configured Capability");

    assert!(saved.projected_skill.markdown.contains("Use it before deciding."));
    assert!(
        !saved
            .projected_skill
            .markdown
            .contains("**Capability: server_a__lookup**")
    );
    assert!(
        !saved
            .projected_skill
            .markdown
            .contains("Use `server_a__lookup` directly.")
    );
    assert!(!saved.projected_skill.markdown.contains(":::capability"));
    assert!(!saved.projected_skill.markdown.contains("Exposure:"));
    assert!(!saved.projected_skill.markdown.contains(&ref_ids[0]));
    let step_policy: String =
        sqlx::query_scalar("SELECT binding_policy FROM workflow_profile_step_bindings WHERE profile_id = ?")
            .bind(&profile_id)
            .fetch_one(&pool)
            .await
            .expect("load normalized Workflow Step policy");
    assert_eq!(step_policy, "direct");
    assert!(skills_root.path().join("investigate-incident/SKILL.md").is_file());

    let remove_command = WorkflowGuideSaveCommand {
        profile_id: profile_id.clone(),
        expected_guide_revision: saved.guide.guide_revision,
        markdown: "# Investigate incident\n".to_string(),
        reclamation_confirmation: None,
    };
    let error = guide
        .save_and_project(remove_command.clone(), skills_root.path().to_path_buf())
        .await
        .expect_err("newly unreferenced Capability requires confirmation");
    let WorkflowGuideError::ReclamationConfirmationRequired(plan) = error else {
        panic!("expected Capability reclamation confirmation requirement");
    };
    assert_eq!(
        plan.capabilities
            .iter()
            .map(|capability| capability.name.as_str())
            .collect::<Vec<_>>(),
        vec!["server_a__lookup"]
    );
    let removed = guide
        .save_and_project(
            WorkflowGuideSaveCommand {
                reclamation_confirmation: Some(WorkflowGuideReclamationConfirmation {
                    package_files: Vec::new(),
                    capability_names: vec!["server_a__lookup".to_string()],
                }),
                ..remove_command
            },
            skills_root.path().to_path_buf(),
        )
        .await
        .expect("save after Capability reclamation confirmation");
    assert!(removed.guide.capabilities.is_empty());
}

#[tokio::test]
async fn external_guide_document_persists_capabilities_steps_and_readable_projection() {
    let pool = pool().await;
    add_server(&pool, "server-a", "Server A", 1).await;
    let profile = workflow_authoring_service(&pool)
        .save(workflow_profile_command(), "test")
        .await
        .expect("create workflow Profile");
    let profile_id = profile.profile.id.expect("created workflow Profile ID");
    let skills_root = tempfile::tempdir().expect("create temporary Skills root");
    let guide = WorkflowGuideService::new(pool.clone());
    guide.view(&profile_id).await.expect("initialize Workflow Guide");
    sqlx::query(
        "INSERT INTO workflow_profile_specifications (
            profile_id, validation_notes, avoid_rules
         ) VALUES (?, 'Preserve validation guidance', 'Preserve avoidance guidance')",
    )
    .bind(&profile_id)
    .execute(&pool)
    .await
    .expect("seed Workflow guidance");

    let saved_external = guide
        .save_package_file_and_project(
            WorkflowGuidePackageFileSaveCommand {
                profile_id: profile_id.clone(),
                package_file_id: None,
                expected_file_revision: None,
                expected_guide_revision: Some(0),
                title: "Evidence procedure".to_string(),
                category: WorkflowGuidePackageCategory::Reference,
                original_filename: "evidence-procedure.md".to_string(),
                bytes: b"# Evidence procedure\n\n:::capability {\"name\":\"server_a__lookup\",\"exposure\":\"direct\"}\nUse it to collect external evidence.\n:::\n".to_vec(),
                reclamation_confirmation: None,
            },
            skills_root.path().to_path_buf(),
        )
        .await
        .expect("save external document with a Capability");
    assert_eq!(
        saved_external.package_file.package_file_id,
        saved_external.guide.package_files[0].package_file_id
    );
    assert_eq!(
        saved_external.package_file.category,
        WorkflowGuidePackageCategory::Reference
    );
    let unlinked_step_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM workflow_profile_steps WHERE profile_id = ?")
            .bind(&profile_id)
            .fetch_one(&pool)
            .await
            .expect("count steps before the external document is linked");
    assert_eq!(unlinked_step_count, 0);

    let external = saved_external
        .guide
        .package_files
        .first()
        .expect("saved external document");
    let saved_index = guide
        .save_package_file_and_project(
            WorkflowGuidePackageFileSaveCommand {
                profile_id: profile_id.clone(),
                package_file_id: None,
                expected_file_revision: None,
                expected_guide_revision: Some(saved_external.guide.guide_revision),
                title: "Evidence index".to_string(),
                category: WorkflowGuidePackageCategory::Reference,
                original_filename: "evidence-index.md".to_string(),
                bytes: format!("# Evidence index\n\n[{}]({})\n", external.title, external.relative_path).into_bytes(),
                reclamation_confirmation: None,
            },
            skills_root.path().to_path_buf(),
        )
        .await
        .expect("save an external document that links to the evidence procedure");
    let index = saved_index
        .guide
        .package_files
        .iter()
        .find(|file| file.title == "Evidence index")
        .expect("saved external index");
    let linked = guide
        .save_and_project(
            WorkflowGuideSaveCommand {
                profile_id: profile_id.clone(),
                expected_guide_revision: saved_index.guide.guide_revision,
                markdown: format!("# Investigate incident\n\n[{}]({})\n", index.title, index.relative_path),
                reclamation_confirmation: None,
            },
            skills_root.path().to_path_buf(),
        )
        .await
        .expect("recursively link external documents from the root Guide");
    let guidance: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT validation_notes, avoid_rules
         FROM workflow_profile_specifications WHERE profile_id = ?",
    )
    .bind(&profile_id)
    .fetch_one(&pool)
    .await
    .expect("load preserved Workflow guidance");
    assert_eq!(guidance.0.as_deref(), Some("Preserve validation guidance"));
    assert_eq!(guidance.1.as_deref(), Some("Preserve avoidance guidance"));
    assert_eq!(linked.guide.documents.len(), 2);
    let projected_root = std::fs::read_to_string(skills_root.path().join("investigate-incident/SKILL.md"))
        .expect("read projected root Skill");
    assert!(projected_root.contains("# Investigate incident"));
    assert!(projected_root.contains("[Evidence index]"));
    assert!(projected_root.contains(&index.relative_path));
    assert!(!projected_root.contains("**External document: Evidence index**"));
    assert!(!projected_root.contains("Consult `"));
    let preview = guide
        .preview(WorkflowGuidePreviewCommand {
            profile_id: profile_id.clone(),
            relative_path: None,
            markdown: linked.guide.markdown.clone(),
        })
        .await
        .expect("preview root Guide with recursively reachable capabilities");
    assert!(preview.projected_skill.markdown.contains("# Investigate incident"));
    assert!(preview.projected_skill.markdown.contains("[Evidence index]"));
    assert!(preview.projected_skill.markdown.contains(&index.relative_path));
    assert!(
        !preview
            .projected_skill
            .markdown
            .contains("**External document: Evidence index**")
    );
    assert!(!preview.projected_skill.markdown.contains("Consult `"));
    let projected = std::fs::read_to_string(
        skills_root
            .path()
            .join("investigate-incident")
            .join(&external.relative_path),
    )
    .expect("read projected external document");
    assert!(projected.contains("Use it to collect external evidence."));
    assert!(!projected.contains("**Capability: server_a__lookup**"));
    assert!(!projected.contains("Use `server_a__lookup` directly."));
    assert!(!projected.contains(":::capability"));
    assert!(!projected.contains("Exposure:"));
    let step_policy: String =
        sqlx::query_scalar("SELECT binding_policy FROM workflow_profile_step_bindings WHERE profile_id = ?")
            .bind(&profile_id)
            .fetch_one(&pool)
            .await
            .expect("load external document Workflow Step binding");
    assert_eq!(step_policy, "direct");

    let source = guide
        .read_external_document(&profile_id, &external.package_file_id, skills_root.path().to_path_buf())
        .await
        .expect("load external Guide source");
    assert!(source.markdown.contains(":::capability"));
}

#[tokio::test]
async fn mixed_exposure_occurrences_project_one_invocation_strategy() {
    let pool = pool().await;
    add_server(&pool, "server-a", "Server A", 1).await;
    let profile = workflow_authoring_service(&pool)
        .save(workflow_profile_command(), "test")
        .await
        .expect("create workflow Profile");
    let profile_id = profile.profile.id.expect("created workflow Profile ID");
    let skills_root = tempfile::tempdir().expect("create temporary Skills root");
    let guide = WorkflowGuideService::new(pool.clone());
    guide.view(&profile_id).await.expect("initialize Workflow Guide");

    let saved_external = guide
        .save_package_file_and_project(
            WorkflowGuidePackageFileSaveCommand {
                profile_id: profile_id.clone(),
                package_file_id: None,
                expected_file_revision: None,
                expected_guide_revision: Some(0),
                title: "Evidence procedure".to_string(),
                category: WorkflowGuidePackageCategory::Reference,
                original_filename: "evidence-procedure.md".to_string(),
                bytes: b"# Evidence procedure\n\n:::capability {\"name\":\"server_a__lookup\",\"exposure\":\"direct\"}\nCapture the screenshot here.\n:::\n".to_vec(),
                reclamation_confirmation: None,
            },
            skills_root.path().to_path_buf(),
        )
        .await
        .expect("save external document with a Direct Capability");
    let external = saved_external
        .guide
        .package_files
        .first()
        .expect("saved external document");
    let linked = guide
        .save_and_project(
            WorkflowGuideSaveCommand {
                profile_id: profile_id.clone(),
                expected_guide_revision: saved_external.guide.guide_revision,
                markdown: format!(
                    "# Investigate incident\n\n:::capability {{\"name\":\"server_a__lookup\",\"exposure\":\"meta_on_demand\"}}\nInspect details first.\n:::\n\n[{}]({})\n",
                    external.title, external.relative_path
                ),
                reclamation_confirmation: None,
            },
            skills_root.path().to_path_buf(),
        )
        .await
        .expect("save root Guide with a Meta occurrence of the same Capability");

    let projected_root = std::fs::read_to_string(skills_root.path().join("investigate-incident/SKILL.md"))
        .expect("read projected root Skill");
    let projected_external = std::fs::read_to_string(
        skills_root
            .path()
            .join("investigate-incident")
            .join(&external.relative_path),
    )
    .expect("read projected external document");
    let preview = guide
        .preview(WorkflowGuidePreviewCommand {
            profile_id: profile_id.clone(),
            relative_path: None,
            markdown: linked.guide.markdown.clone(),
        })
        .await
        .expect("preview mixed-exposure Guide");

    assert_eq!(preview.projected_skill.markdown, projected_root);
    assert!(projected_root.contains("Inspect details first."));
    assert!(projected_root.contains("[Evidence procedure]"));
    assert!(projected_root.contains(&external.relative_path));
    assert!(!projected_root.contains("Use `server_a__lookup` directly."));
    assert!(!projected_root.contains("mcpmate_ucan_details"));
    assert!(!projected_root.contains("Exposure:"));
    assert!(projected_external.contains("Capture the screenshot here."));
    assert!(!projected_external.contains("Use `server_a__lookup` directly."));
    assert!(!projected_external.contains("mcpmate_ucan_details"));
    let policies: Vec<String> = sqlx::query_scalar(
        "SELECT binding_policy FROM workflow_profile_step_bindings WHERE profile_id = ? ORDER BY step_index",
    )
    .bind(&profile_id)
    .fetch_all(&pool)
    .await
    .expect("load occurrence binding policies");
    assert_eq!(policies, vec!["meta_on_demand".to_string(), "direct".to_string()]);
}

fn tool_record(
    server_id: &str,
    name: &str,
    description: &str,
) -> CatalogRecord {
    CatalogRecord::materialize(
        server_id,
        name,
        format!("{}__{name}", server_id.replace('-', "_")),
        CapabilityPayload::Tool(Tool::new(
            name.to_string(),
            description.to_string(),
            Arc::new(json!({"type": "object"}).as_object().unwrap().clone()),
        )),
    )
    .expect("materialize fixture tool")
}

async fn observe_server(
    pool: &sqlx::SqlitePool,
    server_id: &str,
    server_name: &str,
    description: &str,
) -> Vec<String> {
    let initialize: InitializeResult = serde_json::from_value(json!({
        "protocolVersion": "2025-11-25",
        "capabilities": {"tools": {"listChanged": true}},
        "serverInfo": {"name": server_name, "version": "1.0.0"}
    }))
    .expect("fixture initialize result");
    let records = vec![
        tool_record(server_id, "lookup", description),
        tool_record(server_id, "search", "search supporting evidence"),
    ];
    let ref_ids = records.iter().map(|record| record.ref_id.to_string()).collect();
    SqliteCapabilityCatalog::new(pool.clone())
        .commit_observation(CapabilityObservation::new(
            server_id,
            server_name,
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
        .expect("commit fixture capability observation");
    ref_ids
}

async fn add_server(
    pool: &sqlx::SqlitePool,
    server_id: &str,
    server_name: &str,
    enabled: i64,
) -> Vec<String> {
    sqlx::query(
        "INSERT INTO server_config (id, name, server_type, command, enabled)
         VALUES (?, ?, 'stdio', '', ?)",
    )
    .bind(server_id)
    .bind(server_name)
    .bind(enabled)
    .execute(pool)
    .await
    .expect("insert fixture server");
    let ref_ids = observe_server(pool, server_id, server_name, "initial lookup").await;
    for (index, name) in ["lookup", "search"].into_iter().enumerate() {
        sqlx::query(
            "INSERT INTO server_tools (id, server_id, server_name, tool_name, unique_name)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&ref_ids[index])
        .bind(server_id)
        .bind(server_name)
        .bind(name)
        .bind(format!("{}__{name}", server_id.replace('-', "_")))
        .execute(pool)
        .await
        .expect("insert fixture capability name");
    }
    ref_ids
}

fn workflow_profile_command() -> ProfileAuthoringCommand {
    ProfileAuthoringCommand {
        id: None,
        expected_authoring_generation: None,
        name: "Investigate incident".to_string(),
        description: Some("Workflow-only authoring profile".to_string()),
        profile_type: "scenario".to_string(),
        priority: 0,
        is_active: false,
        is_default: false,
        server_ids: vec!["server-a".to_string()],
        clone_from_id: None,
        profile_mode: Some(ProfileMode::Workflow),
        skill_name: Some("investigate-incident".to_string()),
        package_distribution: None,
        workflow_guidance: None,
    }
}

fn workflow_authoring_service(pool: &sqlx::SqlitePool) -> ProfileAuthoringService {
    ProfileAuthoringService::with_skills_root(pool.clone(), tempfile::tempdir().unwrap().keep())
}

fn workflow_steps(ref_ids: &[String]) -> Vec<WorkflowStepCommand> {
    vec![
        WorkflowStepCommand {
            step_id: None,
            title: "Discover".to_string(),
            description: Some("Inspect available evidence".to_string()),
            bindings: vec![
                WorkflowBindingCommand {
                    ref_id: ref_ids[0].clone(),
                    binding_policy: WorkflowBindingPolicy::default(),
                },
                WorkflowBindingCommand {
                    ref_id: ref_ids[1].clone(),
                    binding_policy: WorkflowBindingPolicy::Direct,
                },
            ],
        },
        WorkflowStepCommand {
            step_id: None,
            title: "Resolve".to_string(),
            description: None,
            bindings: vec![WorkflowBindingCommand {
                ref_id: ref_ids[0].clone(),
                binding_policy: WorkflowBindingPolicy::Direct,
            }],
        },
    ]
}

fn app_state(pool: sqlx::SqlitePool) -> Arc<AppState> {
    let database = Arc::new(Database {
        pool,
        path: PathBuf::new(),
        capability_cache: Arc::new(mcpmate_capability_store::DerivedCapabilityCache::default()),
    });
    let inspector_calls = Arc::new(InspectorCallRegistry::new());
    inspector_service::set_call_registry(inspector_calls.clone());
    let config = Arc::new(mcpmate::core::models::Config::default());
    let connection_pool = Arc::new(Mutex::new(UpstreamConnectionPool::new(
        config.clone(),
        Some(database.clone()),
    )));
    let mut proxy = ProxyServer::new(config);
    proxy.connection_pool = connection_pool.clone();
    proxy.database = Some(database.clone());
    Arc::new(AppState {
        connection_pool,
        metrics_collector: Arc::new(MetricsCollector::new(std::time::Duration::from_secs(1))),
        http_proxy: Some(Arc::new(proxy)),
        profile_merge_service: None,
        database: Some(database),
        audit_database: None,
        audit_service: None,
        config_application_state: Arc::new(mcpmate::core::profile::ConfigApplicationStateManager::new()),
        client_service: None,
        inspector_calls,
        inspector_sessions: Arc::new(InspectorSessionManager::new()),
        oauth_manager: RwLock::new(None),
        secret_store: RwLock::new(None),
        secret_store_readiness: RwLock::new(unavailable_secret_store_readiness("test_unavailable")),
    })
}

#[tokio::test]
async fn workflow_specification_is_ordered_cas_aware_and_does_not_compile_hosted_surface() {
    let pool = pool().await;
    let ref_ids = add_server(&pool, "server-a", "Server A", 1).await;
    let profile = workflow_authoring_service(&pool)
        .save(workflow_profile_command(), "test")
        .await
        .expect("create workflow Profile");
    assert_eq!(profile.profile_mode, ProfileMode::Workflow);
    assert!(profile.materializations.is_empty());
    let profile_response = ResponseConverter::profile_to_response(&profile.profile);
    assert_eq!(profile_response.profile_mode, ProfileMode::Workflow);
    assert!(
        profile_response
            .allowed_operations
            .iter()
            .any(|operation| operation == "activate")
    );
    let profile_id = profile.profile.id.clone().expect("created workflow Profile ID");
    let activation = ProfileSurfaceManagement::set_profiles_active(
        &pool,
        std::slice::from_ref(&profile_id),
        ProfileActivationAction::Activate,
        HashMap::from([(profile_id.clone(), profile.profile.authoring_generation)]),
        "test",
    )
    .await
    .expect("workflow activation should be allowed");
    assert!(activation.mutations.iter().any(|mutation| mutation.is_active));
    let baseline_publications: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM surface_publications")
        .fetch_one(&pool)
        .await
        .expect("count surface publications");

    let service = WorkflowSpecificationService::new(pool.clone());
    let empty = service
        .view(&profile_id)
        .await
        .expect("load empty workflow specification");
    assert_eq!(empty.specification_revision, None);
    assert_eq!(empty.validation_notes, None);
    assert_eq!(empty.avoid_rules, None);
    assert_eq!(empty.tool_binding_count, 0);
    assert!(empty.steps.is_empty());
    let saved = service
        .save(WorkflowSpecificationSaveCommand {
            profile_id: profile_id.clone(),
            expected_specification_revision: None,
            validation_notes: Some("Confirm the evidence before resolving the incident".to_string()),
            avoid_rules: Some("Do not change production state during investigation".to_string()),
            steps: workflow_steps(&ref_ids),
        })
        .await
        .expect("create workflow specification");
    assert_eq!(saved.specification_revision, Some(0));
    assert_eq!(
        saved.validation_notes.as_deref(),
        Some("Confirm the evidence before resolving the incident")
    );
    assert_eq!(
        saved.avoid_rules.as_deref(),
        Some("Do not change production state during investigation")
    );
    assert_eq!(
        saved.steps.iter().map(|step| step.title.as_str()).collect::<Vec<_>>(),
        ["Discover", "Resolve"]
    );
    assert_eq!(
        saved.steps[0].bindings[0].binding_policy,
        WorkflowBindingPolicy::MetaOnDemand
    );
    assert_eq!(saved.steps[0].bindings[1].ref_id, ref_ids[1]);
    assert_eq!(saved.steps[0].bindings[1].binding_policy, WorkflowBindingPolicy::Direct);
    assert_eq!(saved.steps[1].bindings[0].binding_policy, WorkflowBindingPolicy::Direct);
    assert_eq!(saved.tool_binding_count, 3);
    let persisted_policies: Vec<String> = sqlx::query_scalar(
        "SELECT binding_policy FROM workflow_profile_step_bindings WHERE profile_id = ? ORDER BY step_index, binding_index",
    )
    .bind(&profile_id)
    .fetch_all(&pool)
    .await
    .expect("load persisted binding order");
    assert_eq!(persisted_policies, ["meta_on_demand", "direct", "direct"]);
    let persisted_guidance: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT validation_notes, avoid_rules FROM workflow_profile_specifications WHERE profile_id = ?",
    )
    .bind(&profile_id)
    .fetch_one(&pool)
    .await
    .expect("load persisted workflow guidance");
    assert_eq!(persisted_guidance.0, saved.validation_notes);
    assert_eq!(persisted_guidance.1, saved.avoid_rules);
    let publications_after_save: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM surface_publications")
        .fetch_one(&pool)
        .await
        .expect("recount surface publications");
    assert_eq!(publications_after_save, baseline_publications);

    let updated = service
        .save(WorkflowSpecificationSaveCommand {
            profile_id: profile_id.clone(),
            expected_specification_revision: Some(0),
            validation_notes: Some("Re-check the final incident evidence".to_string()),
            avoid_rules: Some("Avoid unreviewed production changes".to_string()),
            steps: workflow_steps(&ref_ids),
        })
        .await
        .expect("update workflow specification");
    assert_eq!(updated.specification_revision, Some(1));
    assert_eq!(
        updated.validation_notes.as_deref(),
        Some("Re-check the final incident evidence")
    );
    assert_eq!(
        updated.avoid_rules.as_deref(),
        Some("Avoid unreviewed production changes")
    );
    let stale = service
        .save(WorkflowSpecificationSaveCommand {
            profile_id: profile_id.clone(),
            expected_specification_revision: Some(0),
            validation_notes: Some("stale guidance".to_string()),
            avoid_rules: Some("stale avoidance".to_string()),
            steps: workflow_steps(&ref_ids),
        })
        .await
        .expect_err("stale workflow specification update must fail");
    assert!(matches!(
        stale,
        WorkflowSpecificationError::SpecificationChanged {
            current_specification_revision: 1
        }
    ));

    observe_server(&pool, "server-a", "Server A", "changed lookup").await;
    let Json(preview) = workflow_specification_preview(
        State(app_state(pool.clone())),
        Query(ProfileIdReq { id: profile_id.clone() }),
    )
    .await
    .expect("preview through Workflow API handler");
    let preview = preview.data.expect("Workflow API preview data").preview;
    assert!(!preview.valid);
    assert_eq!(preview.specification_revision, Some(1));
    assert_eq!(preview.validation_notes, updated.validation_notes);
    assert_eq!(preview.avoid_rules, updated.avoid_rules);
    assert_eq!(
        preview.steps[0].bindings[0].validation,
        WorkflowBindingValidation::Drifted
    );

    let invalid = service
        .save(WorkflowSpecificationSaveCommand {
            profile_id: profile_id.clone(),
            expected_specification_revision: Some(1),
            validation_notes: updated.validation_notes.clone(),
            avoid_rules: updated.avoid_rules.clone(),
            steps: vec![WorkflowStepCommand {
                step_id: None,
                title: "Invalid".to_string(),
                description: None,
                bindings: vec![WorkflowBindingCommand {
                    ref_id: "tool:server-a:missing".to_string(),
                    binding_policy: WorkflowBindingPolicy::MetaOnDemand,
                }],
            }],
        })
        .await
        .expect_err("binding outside the Profile capability boundary must fail");
    assert!(matches!(invalid, WorkflowSpecificationError::InvalidBinding { .. }));
    assert_eq!(service.view(&profile_id).await.unwrap().specification_revision, Some(1));
    sqlx::query("UPDATE capability_refs SET kind = 'resources' WHERE ref_id = ?")
        .bind(&ref_ids[1])
        .execute(&pool)
        .await
        .expect("change one fixture capability to a resource");
    let viewed = service
        .view(&profile_id)
        .await
        .expect("load workflow specification with mixed capability kinds");
    assert_eq!(viewed.tool_binding_count, 2);

    service
        .delete(&profile_id, 1)
        .await
        .expect("delete workflow specification with current revision");
    let empty_after_delete = service
        .view(&profile_id)
        .await
        .expect("reload empty specification after delete");
    assert_eq!(empty_after_delete.specification_revision, None);
    assert!(empty_after_delete.steps.is_empty());
}

#[tokio::test]
async fn workflow_specification_can_expand_while_retaining_and_reordering_steps() {
    let pool = pool().await;
    let ref_ids = add_server(&pool, "server-a", "Server A", 1).await;
    let profile = workflow_authoring_service(&pool)
        .save(workflow_profile_command(), "test")
        .await
        .expect("create workflow Profile");
    let profile_id = profile.profile.id.expect("created workflow Profile ID");
    let service = WorkflowSpecificationService::new(pool);
    let first = service
        .save(WorkflowSpecificationSaveCommand {
            profile_id: profile_id.clone(),
            expected_specification_revision: None,
            validation_notes: None,
            avoid_rules: None,
            steps: vec![WorkflowStepCommand {
                step_id: None,
                title: "Retained".to_string(),
                description: None,
                bindings: vec![WorkflowBindingCommand {
                    ref_id: ref_ids[0].clone(),
                    binding_policy: WorkflowBindingPolicy::MetaOnDemand,
                }],
            }],
        })
        .await
        .expect("save initial workflow step");

    let saved = service
        .save(WorkflowSpecificationSaveCommand {
            profile_id,
            expected_specification_revision: first.specification_revision,
            validation_notes: None,
            avoid_rules: None,
            steps: vec![
                WorkflowStepCommand {
                    step_id: None,
                    title: "Inserted first".to_string(),
                    description: None,
                    bindings: vec![WorkflowBindingCommand {
                        ref_id: ref_ids[0].clone(),
                        binding_policy: WorkflowBindingPolicy::MetaOnDemand,
                    }],
                },
                WorkflowStepCommand {
                    step_id: None,
                    title: "Inserted second".to_string(),
                    description: None,
                    bindings: vec![WorkflowBindingCommand {
                        ref_id: ref_ids[0].clone(),
                        binding_policy: WorkflowBindingPolicy::MetaOnDemand,
                    }],
                },
                WorkflowStepCommand {
                    step_id: Some(first.steps[0].step_id.clone()),
                    title: "Retained".to_string(),
                    description: None,
                    bindings: vec![WorkflowBindingCommand {
                        ref_id: ref_ids[0].clone(),
                        binding_policy: WorkflowBindingPolicy::MetaOnDemand,
                    }],
                },
            ],
        })
        .await
        .expect("expand and reorder workflow steps");

    assert_eq!(
        saved.steps.iter().map(|step| step.title.as_str()).collect::<Vec<_>>(),
        ["Inserted first", "Inserted second", "Retained"]
    );
    assert_eq!(saved.steps[2].step_id, first.steps[0].step_id);
}

#[tokio::test]
async fn workflow_binding_accepts_globally_enabled_servers_outside_the_profile() {
    let pool = pool().await;
    add_server(&pool, "server-a", "Server A", 1).await;
    let external_refs = add_server(&pool, "server-b", "Server B", 1).await;
    let profile = workflow_authoring_service(&pool)
        .save(workflow_profile_command(), "test")
        .await
        .expect("create workflow Profile");
    let profile_id = profile.profile.id.clone().expect("created workflow Profile ID");

    let service = WorkflowSpecificationService::new(pool.clone());
    let saved = service
        .save(WorkflowSpecificationSaveCommand {
            profile_id: profile_id.clone(),
            expected_specification_revision: None,
            validation_notes: None,
            avoid_rules: None,
            steps: vec![WorkflowStepCommand {
                step_id: None,
                title: "External evidence".to_string(),
                description: None,
                bindings: vec![WorkflowBindingCommand {
                    ref_id: external_refs[0].clone(),
                    binding_policy: WorkflowBindingPolicy::MetaOnDemand,
                }],
            }],
        })
        .await
        .expect("workflow may bind a globally enabled server capability outside the Profile");
    assert_eq!(saved.steps[0].bindings[0].ref_id, external_refs[0]);

    let Json(preview) = workflow_specification_preview(
        State(app_state(pool.clone())),
        Query(ProfileIdReq { id: profile_id.clone() }),
    )
    .await
    .expect("preview after global binding");
    assert!(preview.data.expect("preview data").preview.valid);

    observe_server(&pool, "server-b", "Server B", "changed external").await;
    let Json(preview) = workflow_specification_preview(
        State(app_state(pool.clone())),
        Query(ProfileIdReq { id: profile_id.clone() }),
    )
    .await
    .expect("preview after global capability drift");
    let preview = preview.data.expect("preview data").preview;
    assert!(!preview.valid);
    assert_eq!(
        preview.steps[0].bindings[0].validation,
        WorkflowBindingValidation::Drifted
    );
}

#[tokio::test]
async fn workflow_binding_rejects_globally_disabled_servers() {
    let pool = pool().await;
    add_server(&pool, "server-a", "Server A", 1).await;
    let disabled_refs = add_server(&pool, "server-c", "Server C", 0).await;
    let profile = workflow_authoring_service(&pool)
        .save(workflow_profile_command(), "test")
        .await
        .expect("create workflow Profile");
    let profile_id = profile.profile.id.clone().expect("created workflow Profile ID");

    let invalid = WorkflowSpecificationService::new(pool.clone())
        .save(WorkflowSpecificationSaveCommand {
            profile_id,
            expected_specification_revision: None,
            validation_notes: None,
            avoid_rules: None,
            steps: vec![WorkflowStepCommand {
                step_id: None,
                title: "Disabled".to_string(),
                description: None,
                bindings: vec![WorkflowBindingCommand {
                    ref_id: disabled_refs[0].clone(),
                    binding_policy: WorkflowBindingPolicy::MetaOnDemand,
                }],
            }],
        })
        .await
        .expect_err("globally disabled server capability must not be bindable");
    assert!(matches!(invalid, WorkflowSpecificationError::InvalidBinding { .. }));
}

#[tokio::test]
async fn deleting_workflow_specification_bumps_materials_revision_for_cascaded_links() {
    let pool = pool().await;
    let ref_ids = add_server(&pool, "server-a", "Materials Server", 1).await;
    let profile = workflow_authoring_service(&pool)
        .save(workflow_profile_command(), "test")
        .await
        .expect("create workflow Profile");
    let profile_id = profile.profile.id.expect("workflow Profile ID");
    let specification = WorkflowSpecificationService::new(pool.clone())
        .save(WorkflowSpecificationSaveCommand {
            profile_id: profile_id.clone(),
            expected_specification_revision: None,
            validation_notes: None,
            avoid_rules: None,
            steps: workflow_steps(&ref_ids),
        })
        .await
        .expect("save workflow specification");
    let temporary = tempfile::tempdir().expect("create managed Skills directory");
    let materials = WorkflowMaterialsService::new(pool.clone(), temporary.path().to_path_buf());
    let initial = materials.view(&profile_id).await.expect("initialize Materials library");
    let material = materials
        .save(WorkflowMaterialSaveCommand {
            profile_id: profile_id.clone(),
            material_id: None,
            expected_material_revision: None,
            expected_materials_revision: initial.materials_revision,
            title: "Evidence".to_string(),
            kind: WorkflowMaterialKind::ExternalUrl,
            external_url: Some("https://example.com/evidence".to_string()),
            markdown_content: None,
        })
        .await
        .expect("create Material");
    let step_id = specification.steps[0].step_id.clone();
    materials
        .save_step_materials(WorkflowStepMaterialsSaveCommand {
            profile_id: profile_id.clone(),
            step_id,
            material_ids: vec![material.material_id],
            expected_materials_revision: initial.materials_revision + 1,
        })
        .await
        .expect("attach Material to workflow step");
    let before_delete = materials.view(&profile_id).await.expect("load linked Materials");

    WorkflowSpecificationService::new(pool)
        .delete(
            &profile_id,
            specification
                .specification_revision
                .expect("saved specification revision"),
        )
        .await
        .expect("delete workflow specification");

    let after_delete = materials.view(&profile_id).await.expect("reload Materials library");
    assert_eq!(after_delete.materials_revision, before_delete.materials_revision + 1);
    assert!(after_delete.step_material_ids.is_empty());
}

#[tokio::test]
async fn workflow_materials_keep_ordered_step_references_and_managed_files() {
    let pool = pool().await;
    let ref_ids = add_server(&pool, "server-a", "Materials Server", 1).await;
    let profile = workflow_authoring_service(&pool)
        .save(workflow_profile_command(), "test")
        .await
        .expect("create workflow Profile");
    let profile_id = profile.profile.id.expect("workflow Profile ID");
    let specification = WorkflowSpecificationService::new(pool.clone())
        .save(WorkflowSpecificationSaveCommand {
            profile_id: profile_id.clone(),
            expected_specification_revision: None,
            validation_notes: None,
            avoid_rules: None,
            steps: workflow_steps(&ref_ids),
        })
        .await
        .expect("save workflow steps");
    let temporary = tempfile::tempdir().expect("create managed Skills directory");
    let materials = WorkflowMaterialsService::new(pool, temporary.path().to_path_buf());

    let initial = materials.view(&profile_id).await.expect("initialize Materials library");
    let external = materials
        .save(WorkflowMaterialSaveCommand {
            profile_id: profile_id.clone(),
            material_id: None,
            expected_material_revision: None,
            expected_materials_revision: initial.materials_revision,
            title: "External evidence".to_string(),
            kind: WorkflowMaterialKind::ExternalUrl,
            external_url: Some("https://example.com/evidence".to_string()),
            markdown_content: None,
        })
        .await
        .expect("create external Material");
    let markdown = materials
        .save(WorkflowMaterialSaveCommand {
            profile_id: profile_id.clone(),
            material_id: None,
            expected_material_revision: None,
            expected_materials_revision: 1,
            title: "Local evidence".to_string(),
            kind: WorkflowMaterialKind::MarkdownFile,
            external_url: None,
            markdown_content: Some("# Evidence".to_string()),
        })
        .await
        .expect("create Markdown Material");
    assert_eq!(markdown.relative_path.as_deref(), Some("references/local-evidence.md"));
    let markdown_path = temporary
        .path()
        .join(&initial.skill_name)
        .join(markdown.relative_path.as_deref().expect("Markdown path"));
    assert_eq!(tokio::fs::read_to_string(&markdown_path).await.unwrap(), "# Evidence");
    let uploaded = materials
        .upload(
            &profile_id,
            "Uploaded evidence".to_string(),
            "evidence.md".to_string(),
            b"# Uploaded evidence".to_vec(),
            None,
            None,
            2,
        )
        .await
        .expect("create uploaded Material");
    assert_eq!(uploaded.relative_path.as_deref(), Some("references/evidence.md"));
    let conflict = materials
        .upload(
            &profile_id,
            "Second evidence".to_string(),
            "evidence.md".to_string(),
            b"# Second evidence".to_vec(),
            None,
            None,
            3,
        )
        .await
        .expect("create conflicting uploaded Material with unique path");
    assert_eq!(conflict.relative_path.as_deref(), Some("references/evidence-2.md"));
    let renamed_uploaded = materials
        .save(WorkflowMaterialSaveCommand {
            profile_id: profile_id.clone(),
            material_id: Some(uploaded.material_id.clone()),
            expected_material_revision: Some(uploaded.material_revision),
            expected_materials_revision: 4,
            title: "Renamed uploaded evidence".to_string(),
            kind: WorkflowMaterialKind::UploadedFile,
            external_url: None,
            markdown_content: None,
        })
        .await
        .expect("rename uploaded Material without replacing its file");
    let uploaded_path = temporary
        .path()
        .join(&initial.skill_name)
        .join(uploaded.relative_path.as_deref().expect("uploaded file path"));
    assert_eq!(renamed_uploaded.title, "Renamed uploaded evidence");
    assert_eq!(renamed_uploaded.kind, WorkflowMaterialKind::UploadedFile);
    assert_eq!(renamed_uploaded.relative_path, uploaded.relative_path);
    assert_eq!(renamed_uploaded.original_filename, uploaded.original_filename);
    assert_eq!(renamed_uploaded.file_size, uploaded.file_size);
    assert_eq!(renamed_uploaded.checksum, uploaded.checksum);
    assert_eq!(
        tokio::fs::read_to_string(&uploaded_path).await.unwrap(),
        "# Uploaded evidence"
    );
    let replaced = materials
        .upload(
            &profile_id,
            "Replaced evidence".to_string(),
            "evidence-renamed-on-disk.md".to_string(),
            b"# Replaced evidence".to_vec(),
            Some(uploaded.material_id.as_str()),
            Some(renamed_uploaded.material_revision),
            5,
        )
        .await
        .expect("replace uploaded Material without renaming its storage path");
    assert_eq!(replaced.relative_path, uploaded.relative_path);
    assert_eq!(
        tokio::fs::read_to_string(&uploaded_path).await.unwrap(),
        "# Replaced evidence"
    );

    let step_id = specification.steps[0].step_id.clone();
    materials
        .save_step_materials(WorkflowStepMaterialsSaveCommand {
            profile_id: profile_id.clone(),
            step_id: step_id.clone(),
            material_ids: vec![
                external.material_id.clone(),
                markdown.material_id.clone(),
                renamed_uploaded.material_id.clone(),
            ],
            expected_materials_revision: 6,
        })
        .await
        .expect("attach ordered Materials to Step");
    materials
        .reorder(WorkflowMaterialsReorderCommand {
            profile_id: profile_id.clone(),
            material_ids: vec![
                markdown.material_id.clone(),
                external.material_id.clone(),
                renamed_uploaded.material_id.clone(),
                conflict.material_id.clone(),
            ],
            expected_materials_revision: 7,
        })
        .await
        .expect("reorder Materials library");
    let ordered = materials.view(&profile_id).await.expect("load Materials library");
    assert_eq!(
        ordered
            .materials
            .iter()
            .map(|material| &material.material_id)
            .collect::<Vec<_>>(),
        [
            &markdown.material_id,
            &external.material_id,
            &renamed_uploaded.material_id,
            &conflict.material_id,
        ]
    );
    assert_eq!(
        ordered.step_material_ids[&step_id],
        [
            external.material_id.clone(),
            markdown.material_id.clone(),
            renamed_uploaded.material_id.clone(),
        ]
    );

    materials
        .delete(
            &profile_id,
            &markdown.material_id,
            markdown.material_revision,
            ordered.materials_revision,
        )
        .await
        .expect("delete Markdown Material");
    let after_delete = materials.view(&profile_id).await.expect("reload Materials library");
    assert_eq!(after_delete.materials.len(), 3);
    assert_eq!(
        after_delete.step_material_ids[&step_id],
        [external.material_id, renamed_uploaded.material_id]
    );
    assert!(!markdown_path.exists());
    assert!(uploaded_path.exists());
    assert!(
        temporary
            .path()
            .join(&initial.skill_name)
            .join("references/evidence-2.md")
            .exists()
    );
}
