mod v0002_add_llm_provider_default;
mod v0004_upgrade_server_columns;
mod v0006_normalize_client_configuration;
mod v0008_validate_secure_store;
mod v0010_create_capability_catalog;
mod v0012_create_structured_server_transport;
mod v0013_canonicalize_unrecognized_server_transport;
mod v0014_create_workflow_profile_specification;
mod v0015_create_workflow_profile_materials;
mod v0016_create_workflow_guides;
mod v0017_add_skill_package_distribution;

use super::Migration;
use anyhow::Result;
use sqlx::{Pool, Sqlite};

pub(super) const LLM_PROVIDER_SCHEMA: &str = include_str!("v0001_create_llm_provider.sql");
pub(super) const SERVER_SCHEMA: &str = include_str!("v0003_create_server_configuration.sql");
pub(super) const CLIENT_SCHEMA: &str = include_str!("v0005_create_client_configuration.sql");
pub(super) const SECURE_STORE_SCHEMA: &str = include_str!("v0007_create_secure_store.sql");
pub(super) const PROFILE_SCHEMA: &str = include_str!("v0009_create_profile_authoring.sql");
const PROFILE_AUTHORING_GENERATION: &str = include_str!("v0011_add_profile_authoring_generation.sql");

pub(crate) fn all() -> Vec<Migration> {
    vec![
        Migration::sql(1, "create llm provider", LLM_PROVIDER_SCHEMA),
        v0002_add_llm_provider_default::migration(),
        Migration::sql(3, "create server configuration", SERVER_SCHEMA),
        v0004_upgrade_server_columns::migration(),
        Migration::sql(5, "create client configuration", CLIENT_SCHEMA),
        v0006_normalize_client_configuration::migration(),
        Migration::sql(7, "create secure store storage", SECURE_STORE_SCHEMA),
        v0008_validate_secure_store::migration(),
        Migration::sql(9, "create profile authoring storage", PROFILE_SCHEMA),
        v0010_create_capability_catalog::migration(),
        Migration::sql(11, "add profile authoring generation", PROFILE_AUTHORING_GENERATION),
        v0012_create_structured_server_transport::migration(),
        v0013_canonicalize_unrecognized_server_transport::migration(),
        v0014_create_workflow_profile_specification::migration(),
        v0015_create_workflow_profile_materials::migration(),
        v0016_create_workflow_guides::migration(),
        v0017_add_skill_package_distribution::migration(),
    ]
}

pub(crate) async fn verify_capability_catalog(pool: &Pool<Sqlite>) -> Result<()> {
    v0010_create_capability_catalog::verify(pool).await
}
