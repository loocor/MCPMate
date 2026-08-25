use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

use crate::system::paths::PathService;

pub fn parse_json5_object<T: DeserializeOwned>(
    content: &str,
    source: &str,
    label: &str,
) -> Result<T> {
    let value: serde_json::Value = json5::from_str(content).with_context(|| format!("Parse {label} from {source}"))?;
    if !value.is_object() {
        bail!("{label} at {source} must be a JSON5 object");
    }
    serde_json::from_value(value).with_context(|| format!("Decode {label} from {source}"))
}

pub fn load_json5_object_from_path<T: DeserializeOwned>(
    path: &Path,
    label: &str,
) -> Result<T> {
    let content = std::fs::read_to_string(path).with_context(|| format!("Read {label} from {}", path.display()))?;
    parse_json5_object(&content, &path.display().to_string(), label)
}

pub fn resolve_env_config_override(
    env_var: &str,
    path_service_context: &str,
) -> Result<Option<PathBuf>> {
    let Some(path_hint) = std::env::var(env_var).ok().filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let path_service = PathService::new().with_context(|| path_service_context.to_string())?;
    path_service
        .resolve_user_path(&path_hint)
        .map(Some)
        .with_context(|| format!("Resolve {env_var} path"))
}
