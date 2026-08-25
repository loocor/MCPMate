//! Workflow Guide projection templates and SKILL.md post-processing.
//!
//! Heading normalization here runs on projected SKILL.md output before staging.
//! Board authoring sanitizes trailing newlines only; preview and save projection
//! are authoritative on the backend.

use std::sync::OnceLock;

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

static PROJECTION_CONFIG: OnceLock<ProjectionConfig> = OnceLock::new();
const SKILL_COMPATIBILITY_MAX_CHARS: usize = 500;

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
    let mut output = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        output.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        if let Some(end) = rest.find('}') {
            let key = &rest[..end];
            if let Some((_, value)) = values.iter().find(|(candidate, _)| *candidate == key) {
                output.push_str(value);
                rest = &rest[end + 1..];
                continue;
            }
        }
        output.push('{');
    }
    output.push_str(rest);
    output
}

pub fn normalize_markdown_heading_boundaries(markdown: &str) -> String {
    let mut output = String::new();
    let mut fence: Option<(char, usize)> = None;
    let mut previous_was_content = false;

    for line in markdown.split_inclusive('\n') {
        let (content, newline) = match line.strip_suffix('\n') {
            Some(content) => (content, "\n"),
            None => (line, ""),
        };

        if let Some((delimiter, length)) = fence {
            output.push_str(content);
            output.push_str(newline);
            if closes_markdown_fence(content, delimiter, length) {
                fence = None;
            }
            previous_was_content = true;
            continue;
        }

        if let Some(opener) = markdown_fence_opener(content) {
            output.push_str(content);
            output.push_str(newline);
            fence = Some(opener);
            previous_was_content = true;
            continue;
        }

        if let Some((before, heading)) = split_glued_atx_heading(content) {
            if !before.is_empty() {
                output.push_str(before);
                output.push_str("\n\n");
            } else if previous_was_content {
                ensure_blank_line_before_heading(&mut output);
            }
            output.push_str(heading);
            output.push_str(newline);
            previous_was_content = false;
            continue;
        }

        if previous_was_content && is_atx_heading_line(content) {
            ensure_blank_line_before_heading(&mut output);
            output.push_str(content);
            output.push_str(newline);
            previous_was_content = false;
            continue;
        }

        output.push_str(content);
        output.push_str(newline);
        previous_was_content = !content.trim().is_empty() && !is_atx_heading_line(content);
    }

    output
}

fn ensure_blank_line_before_heading(output: &mut String) {
    if output.ends_with("\n\n") || output.is_empty() {
        return;
    }
    if output.ends_with('\n') {
        output.push('\n');
        return;
    }
    output.push_str("\n\n");
}

fn markdown_fence_opener(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let delimiter = trimmed.chars().next()?;
    if delimiter != '`' && delimiter != '~' {
        return None;
    }
    let length = trimmed.chars().take_while(|character| *character == delimiter).count();
    (length >= 3).then_some((delimiter, length))
}

fn closes_markdown_fence(
    line: &str,
    delimiter: char,
    length: usize,
) -> bool {
    let trimmed = line.trim_start();
    let close_length = trimmed.chars().take_while(|character| *character == delimiter).count();
    close_length >= length && trimmed[close_length..].trim().is_empty()
}

fn is_atx_heading_line(line: &str) -> bool {
    atx_heading_level(line.trim_start()).is_some()
}

fn atx_heading_level(trimmed: &str) -> Option<usize> {
    let level = trimmed.chars().take_while(|character| *character == '#').count();
    if !(1..=6).contains(&level) || trimmed.as_bytes().get(level) != Some(&b' ') {
        return None;
    }
    let text = trimmed[level..].trim();
    (!text.is_empty()).then_some(level)
}

fn split_glued_atx_heading(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    let mut index = 1;
    while index < bytes.len() {
        if bytes[index] != b'#' {
            index += 1;
            continue;
        }
        let predecessor = line[..index].chars().next_back()?;
        if predecessor.is_whitespace()
            || predecessor == '#'
            || predecessor.is_ascii_alphanumeric()
            || predecessor == '_'
        {
            index += 1;
            continue;
        }
        if atx_heading_level(&line[index..]).is_none() {
            index += 1;
            continue;
        }
        return Some((&line[..index], &line[index..]));
    }
    None
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
    Some(bound_compatibility_text(interpolate(
        &projection_config().skills.compatibility,
        &[("servers", servers.as_str())],
    )))
}

fn bound_compatibility_text(value: String) -> String {
    if value.chars().count() <= SKILL_COMPATIBILITY_MAX_CHARS {
        return value;
    }
    let keep = SKILL_COMPATIBILITY_MAX_CHARS.saturating_sub(3);
    let mut truncated: String = value.chars().take(keep).collect();
    truncated.push_str("...");
    truncated
}

fn default_projection_config() -> ProjectionConfig {
    parse_projection_config(BUNDLED_PROJECTION_CONFIG, "bundled projection.json5")
        .expect("bundled projection.json5 must match ProjectionConfig schema")
}

/// Load projection templates from override path or bundled JSON5.
///
/// Override failures return an error; callers should fall back to [`default_projection_config`].
fn load_projection_config() -> anyhow::Result<ProjectionConfig> {
    if let Some(path) =
        resolve_env_config_override("MCPMATE_PROJECTION_CONFIG", "Create PathService for projection config")?
    {
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
    fn interpolate_does_not_rewrite_placeholders_inside_values() {
        let body = capability_item_body(
            "server://docs/{guide}",
            "Then capture.",
            super::super::workflow::WorkflowBindingPolicy::Direct,
        );
        assert_eq!(body, "`server://docs/{guide}` (direct): Then capture.");
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
        assert_eq!(
            normalize_markdown_heading_boundaries("C# stays prose\n"),
            "C# stays prose\n"
        );
        assert_eq!(
            normalize_markdown_heading_boundaries("```\n# stays a comment\n```\n"),
            "```\n# stays a comment\n```\n"
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

    #[test]
    fn truncates_skill_compatibility_to_five_hundred_characters() {
        let servers = vec!["Alpha".repeat(80), "Beta".repeat(80), "Gamma".repeat(80)];
        let compatibility = skill_compatibility(&servers).expect("compatibility");
        assert_eq!(compatibility.chars().count(), 500);
        assert!(compatibility.ends_with("..."));
    }
}
