use mcpmate_secrets::store::SecretOriginInput;
use sqlx::{Pool, Sqlite};

pub async fn prepare_config_database(pool: &Pool<Sqlite>) {
    mcpmate_migrations::prepare_config_database(pool, mcpmate_migrations::DatabaseSource::InMemory)
        .await
        .expect("prepare test config database");
}

pub async fn prepare_audit_database(pool: &Pool<Sqlite>) {
    mcpmate_migrations::prepare_audit_database(pool, mcpmate_migrations::DatabaseSource::InMemory)
        .await
        .expect("prepare test audit database");
}

pub async fn insert_profile(
    pool: &Pool<Sqlite>,
    profile: &crate::config::models::Profile,
) -> String {
    let profile_id = profile.id.clone().unwrap_or_else(|| crate::generate_id!("prof"));
    sqlx::query(
        r#"
        INSERT INTO profile (
            id, name, description, type, role,
            priority, is_active, is_default, authoring_generation
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&profile_id)
    .bind(&profile.name)
    .bind(&profile.description)
    .bind(profile.profile_type)
    .bind(profile.role)
    .bind(profile.priority)
    .bind(profile.is_active)
    .bind(profile.is_default)
    .bind(profile.authoring_generation)
    .execute(pool)
    .await
    .expect("insert test Profile");
    profile_id
}

/// Build a `SecretOriginInput` for an OAuth-managed secret slot.
pub fn oauth_secret_origin(
    server_id: &str,
    server_name: &str,
    field_key: &str,
) -> SecretOriginInput {
    SecretOriginInput {
        server_id: Some(server_id.to_string()),
        server_name: Some(server_name.to_string()),
        server_kind: Some("streamable_http".to_string()),
        source: Some("oauth".to_string()),
        field_group: Some("oauth".to_string()),
        field_key: Some(field_key.to_string()),
        field_index: None,
        field_path: Some(format!("oauth.{field_key}")),
    }
}
