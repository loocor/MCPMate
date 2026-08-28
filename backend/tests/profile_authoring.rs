use mcpmate::core::capability::materializer::bootstrap_managed_surfaces;
use mcpmate::core::events::{Event, EventBus};
use mcpmate::core::profile::authoring::{ProfileAuthoringCommand, ProfileAuthoringError, ProfileAuthoringService};
use mcpmate_capability_store::{
    CapabilityCatalog, CapabilityKind, CapabilityObservation, CapabilityPayload, CatalogRecord, DeclarationState,
    InventoryState, KindObservation, SqliteCapabilityCatalog, SqliteSurfaceStore,
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
        .unwrap();
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
            std::sync::Arc::new(json!({"type": "object"}).as_object().unwrap().clone()),
        )),
    )
    .unwrap()
}

async fn add_server(
    pool: &sqlx::SqlitePool,
    server_id: &str,
    records: Vec<CatalogRecord>,
) {
    sqlx::query(
        "INSERT INTO server_config (id, name, server_type, command, enabled)
         VALUES (?, ?, 'stdio', '', 1)",
    )
    .bind(server_id)
    .bind(server_id)
    .execute(pool)
    .await
    .unwrap();
    let initialize: InitializeResult = serde_json::from_value(json!({
        "protocolVersion": "2025-11-25",
        "capabilities": {"tools": {"listChanged": true}},
        "serverInfo": {"name": server_id, "version": "1.0.0"}
    }))
    .unwrap();
    SqliteCapabilityCatalog::new(pool.clone())
        .commit_observation(CapabilityObservation::new(
            server_id,
            server_id,
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
        .unwrap();
}

async fn add_profile(
    pool: &sqlx::SqlitePool,
    profile_id: &str,
    name: &str,
    server_ids: &[&str],
) {
    sqlx::query(
        "INSERT INTO profile (id, name, description, type, role, is_active)
         VALUES (?, ?, '', 'shared', 'user', 1)",
    )
    .bind(profile_id)
    .bind(name)
    .execute(pool)
    .await
    .unwrap();
    for server_id in server_ids {
        sqlx::query(
            "INSERT INTO profile_server_relationships
                (profile_id, server_id, enabled, new_ref_policy)
             VALUES (?, ?, 1, 'follow')",
        )
        .bind(profile_id)
        .bind(server_id)
        .execute(pool)
        .await
        .unwrap();
    }
}

async fn add_hosted_consumer(
    pool: &sqlx::SqlitePool,
    profile_id: &str,
) {
    sqlx::query(
        r#"
        INSERT INTO client (
            id, identifier, name, config_mode, approval_status,
            capability_source, selected_profile_ids
        ) VALUES (
            'consumer-a', 'client-a', 'Client A', 'hosted', 'approved',
            'profiles', json_array(?)
        )
        "#,
    )
    .bind(profile_id)
    .execute(pool)
    .await
    .unwrap();
}

fn update_command(
    profile_id: &str,
    generation: i64,
    name: &str,
    server_ids: &[&str],
) -> ProfileAuthoringCommand {
    ProfileAuthoringCommand {
        id: Some(profile_id.to_string()),
        expected_authoring_generation: Some(generation),
        name: name.to_string(),
        description: Some(format!("{name} description")),
        profile_type: "shared".to_string(),
        priority: 5,
        is_active: true,
        is_default: false,
        server_ids: server_ids.iter().map(|id| (*id).to_string()).collect(),
        clone_from_id: None,
        profile_mode: None,
        skill_name: None,
        package_distribution: None,
        workflow_guidance: None,
    }
}

#[tokio::test]
async fn stale_profile_generation_rejects_without_partial_writes() {
    let pool = pool().await;
    add_server(&pool, "server-a", vec![tool_record("server-a", "analyze")]).await;
    add_server(&pool, "server-b", vec![tool_record("server-b", "summarize")]).await;
    add_profile(&pool, "profile-a", "Profile A", &["server-a"]).await;
    add_hosted_consumer(&pool, "profile-a").await;
    bootstrap_managed_surfaces(&pool).await.unwrap();
    let service = ProfileAuthoringService::new(pool.clone());

    let saved = service
        .save(update_command("profile-a", 0, "First Save", &["server-a"]), "test")
        .await
        .unwrap();
    assert_eq!(saved.profile.authoring_generation, 1);
    let binding_after_first = SqliteSurfaceStore::new(pool.clone())
        .load_binding("client-a")
        .await
        .unwrap()
        .unwrap();

    let stale = service
        .save(update_command("profile-a", 0, "Stale Save", &["server-b"]), "test")
        .await
        .unwrap_err();
    assert!(matches!(
        stale,
        ProfileAuthoringError::ProfileAuthoringChanged {
            current_authoring_generation: 1
        }
    ));
    let view = service.view("profile-a").await.unwrap();
    assert_eq!(view.profile.name, "First Save");
    assert_eq!(view.profile.description.as_deref(), Some("First Save description"));
    assert_eq!(view.server_ids, vec!["server-a"]);
    let binding_after_stale = SqliteSurfaceStore::new(pool.clone())
        .load_binding("client-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(binding_after_stale, binding_after_first);
}

#[tokio::test]
async fn unrelated_server_revision_does_not_reject_profile_authoring() {
    let pool = pool().await;
    let record_a = tool_record("server-a", "analyze");
    let record_b = tool_record("server-b", "summarize");
    add_server(&pool, "server-a", vec![record_a]).await;
    add_server(&pool, "server-b", vec![record_b]).await;
    add_server(&pool, "server-z", Vec::new()).await;
    add_profile(&pool, "profile-a", "Profile A", &["server-a", "server-b"]).await;
    add_hosted_consumer(&pool, "profile-a").await;
    bootstrap_managed_surfaces(&pool).await.unwrap();
    let service = ProfileAuthoringService::new(pool.clone());
    let current = service.view("profile-a").await.unwrap();

    let initialize: InitializeResult = serde_json::from_value(json!({
        "protocolVersion": "2025-11-25",
        "capabilities": {"tools": {"listChanged": true}},
        "serverInfo": {"name": "server-z", "version": "2.0.0"}
    }))
    .unwrap();
    SqliteCapabilityCatalog::new(pool.clone())
        .commit_observation(CapabilityObservation::new(
            "server-z",
            "server-z",
            "config-v2",
            initialize,
            vec![KindObservation::new(
                CapabilityKind::Tools,
                DeclarationState::Supported,
                InventoryState::Complete,
            )],
            vec![tool_record("server-z", "unrelated")],
        ))
        .await
        .unwrap();

    let saved = service
        .save(
            update_command(
                "profile-a",
                current.profile.authoring_generation,
                "Updated Profile",
                &["server-a", "server-b"],
            ),
            "test",
        )
        .await
        .unwrap();
    assert_eq!(
        saved.profile.authoring_generation,
        current.profile.authoring_generation + 1
    );
}

#[tokio::test]
async fn create_profile_metadata_servers_activation_and_default_are_atomic() {
    let pool = pool().await;
    add_server(&pool, "server-a", Vec::new()).await;
    let service = ProfileAuthoringService::new(pool.clone());

    let error = service
        .save(
            ProfileAuthoringCommand {
                id: None,
                expected_authoring_generation: None,
                name: "Atomic Create".to_string(),
                description: Some("Must not be partially visible".to_string()),
                profile_type: "scenario".to_string(),
                priority: 9,
                is_active: true,
                is_default: false,
                server_ids: vec!["server-a".to_string(), "missing".to_string()],
                clone_from_id: None,
                profile_mode: None,
                skill_name: None,
                package_distribution: None,
                workflow_guidance: None,
            },
            "test",
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ProfileAuthoringError::InvalidTarget { ref dependency_server_ids }
            if dependency_server_ids == &["missing".to_string()]
    ));
    let profile_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM profile WHERE name = 'Atomic Create'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let relationship_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM profile_server_relationships")
        .fetch_one(&pool)
        .await
        .unwrap();
    let publication_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM surface_publications")
        .fetch_one(&pool)
        .await
        .unwrap();
    let binding_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM consumer_surface_bindings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (profile_count, relationship_count, publication_count, binding_count),
        (0, 0, 0, 0)
    );
}

#[tokio::test]
async fn update_profile_and_surface_materialization_commit_together() {
    let pool = pool().await;
    add_server(&pool, "server-a", vec![tool_record("server-a", "analyze")]).await;
    add_server(&pool, "server-b", vec![tool_record("server-b", "summarize")]).await;
    add_profile(&pool, "profile-a", "Profile A", &["server-a"]).await;
    add_hosted_consumer(&pool, "profile-a").await;
    bootstrap_managed_surfaces(&pool).await.unwrap();
    let baseline_binding = SqliteSurfaceStore::new(pool.clone())
        .load_binding("client-a")
        .await
        .unwrap()
        .unwrap();
    sqlx::query(
        r#"
        CREATE TRIGGER fail_profile_authoring_publication
        BEFORE INSERT ON surface_publications
        WHEN NEW.consumer_id = 'client-a'
        BEGIN
            SELECT RAISE(FAIL, 'injected materialization fault');
        END
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    let mut receiver = EventBus::global().subscribe_async();

    let error = ProfileAuthoringService::new(pool.clone())
        .save(update_command("profile-a", 0, "Rolled Back", &["server-b"]), "test")
        .await
        .unwrap_err();
    assert!(matches!(error, ProfileAuthoringError::Persistence(_)));
    let profile: (String, i64) =
        sqlx::query_as("SELECT name, authoring_generation FROM profile WHERE id = 'profile-a'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(profile, ("Profile A".to_string(), 0));
    let server_ids: Vec<String> = sqlx::query_scalar(
        "SELECT server_id FROM profile_server_relationships WHERE profile_id = 'profile-a' ORDER BY server_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(server_ids, vec!["server-a"]);
    let current_binding = SqliteSurfaceStore::new(pool.clone())
        .load_binding("client-a")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current_binding, baseline_binding);
    let mut rollback_effects = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        match event {
            Event::ProfileStatusChanged { profile_id, enabled } if profile_id == "profile-a" => {
                rollback_effects.push(format!("profile:{profile_id}:{enabled}"));
            }
            Event::ServerEnabledInProfileChanged {
                server_id,
                profile_id,
                enabled,
                ..
            } if profile_id == "profile-a" => {
                rollback_effects.push(format!("server:{profile_id}:{server_id}:{enabled}"));
            }
            _ => {}
        }
    }
    assert!(
        rollback_effects.is_empty(),
        "rolled-back authoring must not publish runtime effects: {rollback_effects:?}"
    );
}

#[tokio::test]
async fn clone_and_explicit_server_selection_commit_as_one_authoring_operation() {
    let pool = pool().await;
    let record_a = tool_record("server-a", "analyze");
    let record_b = tool_record("server-b", "summarize");
    add_server(&pool, "server-a", vec![record_a.clone()]).await;
    add_server(&pool, "server-b", vec![record_b.clone()]).await;
    add_profile(&pool, "profile-source", "Source", &["server-a", "server-b"]).await;
    for record in [&record_a, &record_b] {
        sqlx::query(
            "INSERT INTO profile_capability_refs (profile_id, ref_id, enabled) VALUES ('profile-source', ?, 1)",
        )
        .bind(record.ref_id.as_str())
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "DELETE FROM profile_server_relationships WHERE profile_id = 'profile-source' AND server_id = 'server-b'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let saved = ProfileAuthoringService::new(pool.clone())
        .save(
            ProfileAuthoringCommand {
                id: None,
                expected_authoring_generation: None,
                name: "Clone".to_string(),
                description: None,
                profile_type: "shared".to_string(),
                priority: 0,
                is_active: false,
                is_default: false,
                server_ids: vec!["server-a".to_string(), "server-a".to_string()],
                clone_from_id: Some("profile-source".to_string()),
                profile_mode: None,
                skill_name: None,
                package_distribution: None,
                workflow_guidance: None,
            },
            "test",
        )
        .await
        .unwrap();

    assert_eq!(saved.profile.authoring_generation, 0);
    assert_eq!(saved.server_ids, vec!["server-a"]);
    let cloned_refs: Vec<String> =
        sqlx::query_scalar("SELECT ref_id FROM profile_capability_refs WHERE profile_id = ? ORDER BY ref_id")
            .bind(saved.profile.id.as_deref())
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(cloned_refs, vec![record_a.ref_id.to_string()]);
}

#[tokio::test]
async fn create_rolls_back_profile_and_relationships_when_materialization_fails() {
    let pool = pool().await;
    add_server(&pool, "server-a", vec![tool_record("server-a", "analyze")]).await;
    sqlx::query(
        r#"
        INSERT INTO client (
            id, identifier, name, config_mode, approval_status,
            capability_source, selected_profile_ids
        ) VALUES (
            'consumer-activated', 'client-activated', 'Activated Client', 'hosted', 'approved',
            'activated', '[]'
        )
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        r#"
        CREATE TRIGGER fail_profile_create_publication
        BEFORE INSERT ON surface_publications
        WHEN NEW.consumer_id = 'client-activated'
        BEGIN
            SELECT RAISE(FAIL, 'injected create materialization fault');
        END
        "#,
    )
    .execute(&pool)
    .await
    .unwrap();

    let error = ProfileAuthoringService::new(pool.clone())
        .save(
            ProfileAuthoringCommand {
                id: None,
                expected_authoring_generation: None,
                name: "Rolled Back Create".to_string(),
                description: None,
                profile_type: "shared".to_string(),
                priority: 0,
                is_active: true,
                is_default: false,
                server_ids: vec!["server-a".to_string()],
                clone_from_id: None,
                profile_mode: None,
                skill_name: None,
                package_distribution: None,
                workflow_guidance: None,
            },
            "test",
        )
        .await
        .unwrap_err();

    assert!(matches!(error, ProfileAuthoringError::Persistence(_)));
    let profile_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM profile WHERE name = 'Rolled Back Create'")
        .fetch_one(&pool)
        .await
        .unwrap();
    let relationship_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM profile_server_relationships")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!((profile_count, relationship_count), (0, 0));
}

#[tokio::test]
async fn save_returns_the_committed_runtime_effect_deltas() {
    let pool = pool().await;
    add_server(&pool, "server-a", Vec::new()).await;
    add_server(&pool, "server-b", Vec::new()).await;
    add_profile(&pool, "profile-a", "Profile A", &[]).await;
    add_profile(&pool, "profile-b", "Profile B", &["server-a"]).await;
    sqlx::query("UPDATE profile SET is_active = 0 WHERE id = 'profile-b'")
        .execute(&pool)
        .await
        .unwrap();

    let saved = ProfileAuthoringService::new(pool.clone())
        .save(
            ProfileAuthoringCommand {
                id: Some("profile-b".to_string()),
                expected_authoring_generation: Some(0),
                name: "Profile B".to_string(),
                description: None,
                profile_type: "shared".to_string(),
                priority: 0,
                is_active: true,
                is_default: false,
                server_ids: vec!["server-b".to_string()],
                clone_from_id: None,
                profile_mode: None,
                skill_name: None,
                package_distribution: None,
                workflow_guidance: None,
            },
            "test",
        )
        .await
        .unwrap();

    assert_eq!(saved.activation_delta, Some(true));
    let active_profile_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM profile WHERE is_active = 1 ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(active_profile_ids, vec!["profile-a", "profile-b"]);
    assert_eq!(saved.server_relationship_deltas.len(), 2);
    assert_eq!(saved.server_relationship_deltas[0].server_id, "server-a");
    assert!(!saved.server_relationship_deltas[0].enabled);
    assert_eq!(saved.server_relationship_deltas[1].server_id, "server-b");
    assert!(saved.server_relationship_deltas[1].enabled);
}
