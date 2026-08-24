//! Workflow Guide projection templates and SKILL.md post-processing.
//!
//! Heading normalization here runs on projected SKILL.md output before staging.
//! Board authoring sanitizes trailing newlines only; preview and save projection
//! are authoritative on the backend.

use std::sync::OnceLock;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;

use crate::common::json5_config::{load_json5_object_from_path, parse_json5_object, resolve_env_config_override};

const BUNDLED_PROJECTION_CONFIG: &str = include_str!("../../../config/projection.json5");
const PROJECTION_CONFIG_LABEL: &str = "projection config";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProjectionConfig {
    pub workflow: WorkflowProjectionConfig,
    pub skills: SkillsProjectionConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WorkflowProjectionConfig {
    pub capability: CapabilityProjectionConfig,
    pub external: ExternalProjectionConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CapabilityProjectionConfig {
    pub list_threshold: usize,
    pub section_intro: CapabilitySectionIntroConfig,
    pub item: CapabilityItemTemplates,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CapabilitySectionIntroConfig {
    pub base: String,
    pub on_demand: String,
    pub direct: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CapabilityItemTemplates {
    pub direct_only: String,
    pub on_demand_only: String,
    pub direct_with_guide: String,
    pub on_demand_with_guide: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ExternalProjectionConfig {
    pub with_guide: String,
    pub link_only: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SkillsProjectionConfig {
    pub compatibility: String,
}

static GLUED_HEADING: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"([^\n#])(#{1,6}\s+)").expect("valid glued Markdown heading regex"));
static HEADING_NEEDS_BLANK_BEFORE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?m)([^\n])\n(#{1,6}\s+)").expect("valid Markdown heading blank-line regex"));

static PROJECTION_CONFIG: OnceLock<ProjectionConfig> = OnceLock::new();

pub fn projection_config() -> &'static ProjectionConfig {
    PROJECTION_CONFIG.get_or_init(|| {
        load_projection_config().unwrap_or_else(|error| {
            tracing::warn!("Failed to load projection config: {error}");
            default_projection_config()
        })
    })
}

pub fn interpolate(
    template: &str,
    values: &[(&str, &str)],
) -> String {
    let mut output = template.to_string();
    for (key, value) in values {
        output = output.replace(&format!("{{{key}}}"), value);
    }
    output
}

pub fn normalize_markdown_heading_boundaries(markdown: &str) -> String {
    let separated = GLUED_HEADING.replace_all(markdown, "$1\n$2");
    HEADING_NEEDS_BLANK_BEFORE
        .replace_all(&separated, "$1\n\n$2")
        .into_owned()
}

pub fn yaml_scalar(value: &str) -> String {
    serde_yaml::to_string(value)
        .expect("YAML scalar")
        .trim_end()
        .to_string()
}

pub fn format_skill_frontmatter(
    name: &str,
    description: &str,
    compatibility: Option<&str>,
) -> String {
    let mut front_matter = format!("name: {}\n", yaml_scalar(name));
    front_matter.push_str(&format!("description: {}\n", yaml_scalar(description)));
    if let Some(compatibility) = compatibility.filter(|value| !value.trim().is_empty()) {
        front_matter.push_str(&format!("compatibility: {}\n", yaml_scalar(compatibility)));
    }
    front_matter
}

pub fn format_projected_skill_markdown(
    name: &str,
    description: &str,
    compatibility: Option<&str>,
    body: &str,
) -> String {
    let body = normalize_markdown_heading_boundaries(body);
    let front_matter = format_skill_frontmatter(name, description, compatibility);
    format!("---\n{front_matter}---\n\n{body}\n")
}

pub fn capability_section_intro(
    has_meta_on_demand: bool,
    has_direct: bool,
) -> String {
    let config = &projection_config().workflow.capability.section_intro;
    let mut sentences = vec![config.base.clone()];
    if has_meta_on_demand {
        sentences.push(config.on_demand.clone());
    }
    if has_direct {
        sentences.push(config.direct.clone());
    }
    sentences.join(" ")
}

pub fn capability_item_body(
    name: &str,
    guide: &str,
    exposure: super::workflow::WorkflowBindingPolicy,
) -> String {
    let templates = &projection_config().workflow.capability.item;
    let guide = guide.trim();
    let template = match (guide.is_empty(), exposure) {
        (true, super::workflow::WorkflowBindingPolicy::Direct) => &templates.direct_only,
        (true, super::workflow::WorkflowBindingPolicy::MetaOnDemand) => &templates.on_demand_only,
        (false, super::workflow::WorkflowBindingPolicy::Direct) => &templates.direct_with_guide,
        (false, super::workflow::WorkflowBindingPolicy::MetaOnDemand) => &templates.on_demand_with_guide,
    };
    interpolate(template, &[("name", name), ("guide", guide)])
}

pub fn external_reference_body(
    title: &str,
    path: &str,
    guide: &str,
) -> String {
    let templates = &projection_config().workflow.external;
    let guide = guide.trim();
    let values = [("title", title), ("path", path), ("guide", guide)];
    if guide.is_empty() {
        interpolate(&templates.link_only, &values)
    } else {
        interpolate(&templates.with_guide, &values)
    }
}

pub fn skill_compatibility(server_names: &[String]) -> Option<String> {
    if server_names.is_empty() {
        return None;
    }
    let servers = server_names.join(", ");
    Some(interpolate(
        &projection_config().skills.compatibility,
        &[("servers", servers.as_str())],
    ))
}

fn default_projection_config() -> ProjectionConfig {
    parse_projection_config(BUNDLED_PROJECTION_CONFIG, "bundled projection.json5")
        .expect("bundled projection.json5 must match ProjectionConfig schema")
}

/// Load projection templates from override path or bundled JSON5.
///
/// Override failures return an error; callers should fall back to [`default_projection_config`].
fn load_projection_config() -> anyhow::Result<ProjectionConfig> {
    if let Some(path) = resolve_env_config_override("MCPMATE_PROJECTION_CONFIG", "Create PathService for projection config")? {
        return load_json5_object_from_path(&path, PROJECTION_CONFIG_LABEL);
    }
    parse_projection_config(BUNDLED_PROJECTION_CONFIG, "bundled projection.json5")
}

fn parse_projection_config(
    content: &str,
    source: &str,
) -> anyhow::Result<ProjectionConfig> {
    parse_json5_object(content, source, PROJECTION_CONFIG_LABEL)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use tempfile::tempdir;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn bundled_projection_config_is_loadable_and_valid() {
        let bundled = parse_projection_config(BUNDLED_PROJECTION_CONFIG, "bundled projection.json5")
            .expect("decode bundled projection config");
        assert!(bundled.workflow.capability.list_threshold >= 1);
        assert!(!bundled.skills.compatibility.is_empty());

        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe { std::env::remove_var("MCPMATE_PROJECTION_CONFIG") };
        let loaded = load_projection_config().expect("load bundled projection config");
        assert_eq!(loaded, bundled);
    }

    #[test]
    fn load_projection_config_requires_object_root() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("projection.json5");
        std::fs::write(&config_path, "[1, 2, 3]").expect("write invalid root");

        unsafe { std::env::set_var("MCPMATE_PROJECTION_CONFIG", &config_path) };
        let result = load_projection_config();
        assert!(result.is_err());
        let message = format!("{}", result.expect_err("load error"));
        assert!(message.contains("must be a JSON5 object"));
        unsafe { std::env::remove_var("MCPMATE_PROJECTION_CONFIG") };
    }

    #[test]
    fn projection_config_loader_falls_back_to_default_when_override_missing() {
        let _guard = ENV_LOCK.lock().expect("env lock");
        unsafe { std::env::set_var("MCPMATE_PROJECTION_CONFIG", "/tmp/mcpmate-missing-projection.json5") };
        let effective = load_projection_config().unwrap_or_else(|_| default_projection_config());
        assert_eq!(effective, default_projection_config());
        unsafe { std::env::remove_var("MCPMATE_PROJECTION_CONFIG") };
    }

    #[test]
    fn interpolates_capability_templates() {
        let body = capability_item_body(
            "playwright_browser_navigate",
            "Visit the target URL.",
            super::super::workflow::WorkflowBindingPolicy::MetaOnDemand,
        );
        assert_eq!(body, "`playwright_browser_navigate` (on-demand): Visit the target URL.");
    }

    #[test]
    fn normalizes_glued_markdown_headings() {
        assert_eq!(
            normalize_markdown_heading_boundaries("Intro line.## Goal\n\nBody"),
            "Intro line.\n\n## Goal\n\nBody"
        );
        assert_eq!(
            normalize_markdown_heading_boundaries(
                "Use Playwright MCP to take a screenshot of the specified URL.\n## Goal\n\n- item"
            ),
            "Use Playwright MCP to take a screenshot of the specified URL.\n\n## Goal\n\n- item"
        );
    }

    #[test]
    fn formats_skill_frontmatter_in_spec_field_order() {
        let front_matter = format_skill_frontmatter(
            "screenshot",
            "Capture pages as images.",
            Some("Requires Playwright via MCPMate."),
        );
        assert!(front_matter.starts_with("name: screenshot\n"));
        assert!(front_matter.contains("\ndescription: Capture pages as images.\n"));
        assert!(front_matter.contains("\ncompatibility: Requires Playwright via MCPMate.\n"));
    }

    #[test]
    fn formats_skill_compatibility_from_server_names() {
        assert_eq!(
            skill_compatibility(&["Playwright".to_string()]),
            Some("Requires Playwright via MCPMate.".to_string())
        );
        assert_eq!(skill_compatibility(&[]), None);
    }
}
