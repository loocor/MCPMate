// Basic query operations for Profile
// Contains read-only operations for profile

use anyhow::{Context, Result};
use sqlx::{Pool, Sqlite};

use crate::{
    common::{
        database::{fetch_all_ordered, fetch_optional},
        profile::{ProfileRole, ProfileType},
    },
    config::models::Profile,
};

/// Get all profile from the database
pub async fn get_all_profile(pool: &Pool<Sqlite>) -> Result<Vec<Profile>> {
    fetch_all_ordered(pool, "profile", Some("name")).await
}

/// Capability working-set profiles.
/// Workflow `is_active` means published, not part of Hosted/Transparent Active.
pub async fn get_active_profile(pool: &Pool<Sqlite>) -> Result<Vec<Profile>> {
    tracing::trace!("Executing SQL query to get all active profile");

    let profile = sqlx::query_as::<_, Profile>(
        r#"
        SELECT * FROM profile
        WHERE is_active = 1
          AND profile_mode != 'workflow'
        ORDER BY priority DESC, created_at ASC
        "#,
    )
    .fetch_all(pool)
    .await
    .context("Failed to fetch active profile")?;

    tracing::trace!("Successfully fetched {} active profile from database", profile.len());
    Ok(profile)
}

/// Get the default profile from the database
pub async fn get_default_profile(pool: &Pool<Sqlite>) -> Result<Option<Profile>> {
    tracing::trace!("Retrieving default anchor profile");

    if let Some(profile) = get_profile_by_role(pool, ProfileRole::DefaultAnchor).await? {
        return Ok(Some(profile));
    }

    tracing::trace!("Default anchor profile not found via role, falling back to is_default flag");

    let profile = sqlx::query_as::<_, Profile>(
        r#"
        SELECT * FROM profile
        WHERE is_default = 1
        ORDER BY priority DESC, created_at ASC
        LIMIT 1
        "#,
    )
    .fetch_optional(pool)
    .await
    .context("Failed to fetch default profile")?;

    if let Some(ref s) = profile {
        tracing::trace!(
            "Found fallback default profile '{}' with ID {}",
            s.name,
            s.id.as_ref().unwrap_or(&"unknown".to_string())
        );
    } else {
        tracing::trace!("No default profile found");
    }

    Ok(profile)
}

/// Get all profiles marked as default and active from the database
pub async fn get_default_profiles(pool: &Pool<Sqlite>) -> Result<Vec<Profile>> {
    tracing::trace!("Executing SQL query to get all default profiles");

    let profile = sqlx::query_as::<_, Profile>(
        r#"
        SELECT * FROM profile
        WHERE is_default = 1
        ORDER BY priority DESC, created_at ASC
        "#,
    )
    .fetch_all(pool)
    .await
    .context("Failed to fetch default profiles")?;

    tracing::trace!("Successfully fetched {} default profiles from database", profile.len());
    Ok(profile)
}

/// Get profile by type from the database
pub async fn get_profile_by_type(
    pool: &Pool<Sqlite>,
    profile_type: ProfileType,
) -> Result<Vec<Profile>> {
    tracing::trace!("Executing SQL query to get profile of type '{}'", profile_type.as_str());

    let profile = sqlx::query_as::<_, Profile>(
        r#"
        SELECT * FROM profile
        WHERE type = ?
        ORDER BY name
        "#,
    )
    .bind(profile_type.as_str())
    .fetch_all(pool)
    .await
    .context("Failed to fetch profile by type")?;

    tracing::trace!(
        "Successfully fetched {} profile of type '{}'",
        profile.len(),
        profile_type.as_str()
    );
    Ok(profile)
}

/// Get a specific profile from the database
pub async fn get_profile(
    pool: &Pool<Sqlite>,
    id: &str,
) -> Result<Option<Profile>> {
    let profile: Option<Profile> = fetch_optional(pool, "profile", "id", id).await?;

    if let Some(ref p) = profile {
        tracing::debug!("Found profile '{}' with ID {}, type: {}", p.name, id, p.profile_type);
    } else {
        tracing::debug!("No profile found with ID {}", id);
    }

    Ok(profile)
}

/// Get a specific profile by role from the database
pub async fn get_profile_by_role(
    pool: &Pool<Sqlite>,
    role: ProfileRole,
) -> Result<Option<Profile>> {
    tracing::debug!("Executing SQL query to get profile with role '{}'", role.as_str());

    let profile = sqlx::query_as::<_, Profile>(
        r#"
        SELECT * FROM profile
        WHERE role = ?
        LIMIT 1
        "#,
    )
    .bind(role)
    .fetch_optional(pool)
    .await
    .context("Failed to fetch profile by role")?;

    if let Some(ref p) = profile {
        tracing::debug!(
            "Found profile '{}' with ID {} for role '{}'",
            p.name,
            p.id.as_ref().unwrap_or(&"unknown".to_string()),
            role.as_str()
        );
    } else {
        tracing::debug!("No profile found for role '{}'", role.as_str());
    }

    Ok(profile)
}

/// Get a specific profile by name from the database
pub async fn get_profile_by_name(
    pool: &Pool<Sqlite>,
    name: &str,
) -> Result<Option<Profile>> {
    tracing::debug!("Executing SQL query to get profile '{}'", name);

    let profile = sqlx::query_as::<_, Profile>(
        r#"
        SELECT * FROM profile
        WHERE name = ?
        "#,
    )
    .bind(name)
    .fetch_optional(pool)
    .await
    .context("Failed to fetch profile by name")?;

    if let Some(ref p) = profile {
        tracing::debug!(
            "Found profile '{}' with ID {}, type: {}",
            name,
            p.id.as_ref().unwrap_or(&"unknown".to_string()),
            p.profile_type
        );
    } else {
        tracing::debug!("No profile found with name '{}'", name);
    }

    Ok(profile)
}
