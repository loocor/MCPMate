use std::collections::{BTreeMap, BTreeSet};

use once_cell::sync::Lazy;
use regex::Regex;

use super::super::projection::{
    capability_item_body, capability_section_intro, external_reference_body, format_projected_skill_markdown,
    projection_config,
};
use super::super::workflow::WorkflowBindingPolicy;
use super::{
    RenderedWorkflowSkill, WorkflowGuide, WorkflowGuideCapability, WorkflowGuideError, WorkflowGuideExternalReference,
    WorkflowGuideHeading, WorkflowGuideParseError,
};

static CAPABILITY_START: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^:::capability\s+(\{.*\})\s*$").expect("valid Workflow Guide capability directive regex")
});
static EXTERNAL_START: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^:::external\s+(\{.*\})\s*$").expect("valid Workflow Guide external directive regex"));
static STANDALONE_EXTERNAL_REFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\s*\[([^\]\n]+)\]\((references/[^\s)#]+\.md)(#[^\s)]+)?\)\s*$")
        .expect("valid standalone external Markdown reference regex")
});
static DIRECTIVE_END: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^:::\s*$").expect("valid Workflow Guide directive end regex"));
pub(super) static PACKAGE_FILE_REFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"\[[^\]\n]+\]\(((?:references|scripts|assets)/[^\s)#]+)(?:#[^\s)]+)?\)")
        .expect("valid workflow Guide package file reference regex")
});
pub(super) static SIBLING_MARKDOWN_REFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?m)^\s*\[[^\]\n]+\]\(((?:\./)?[^/\s)#]+\.md)(?:#[^\s)]+)?\)\s*$")
        .expect("valid external Guide sibling Markdown reference regex")
});
pub(super) static UUID_REFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b")
        .expect("valid UUID reference regex")
});

/// Parses the canonical, document-first Workflow Guide format.
///
/// This parser intentionally owns only document syntax. Database lookups for
/// canonical capability names and package paths are performed by the authoring service
/// so a missing or ambiguous reference fails at the same transaction boundary
/// as persistence and projection.
pub fn parse_workflow_guide(markdown: &str) -> Result<WorkflowGuide, Vec<WorkflowGuideParseError>> {
    let mut headings = Vec::new();
    let mut capabilities = Vec::new();
    let mut external_references = Vec::new();
    let mut package_paths = BTreeSet::new();
    let mut errors = Vec::new();
    let mut fence = None;
    let mut active_capability: Option<ActiveCapability> = None;
    let mut active_external: Option<ActiveExternal> = None;

    for (index, line) in markdown.lines().enumerate() {
        let line_number = index + 1;
        if let Some(active_fence) = fence {
            if closes_fence(line, active_fence) {
                fence = None;
                if let Some(active) = active_capability.as_mut() {
                    active.lines.push(line.to_string());
                } else if let Some(active) = active_external.as_mut() {
                    active.lines.push(line.to_string());
                }
                continue;
            }
            if contains_reserved_workflow_guide_syntax(line) {
                errors.push(WorkflowGuideParseError {
                    line: line_number,
                    message: "Workflow Guide directives and references are not allowed in fenced code".to_string(),
                });
            }
            if let Some(active) = active_capability.as_mut() {
                active.lines.push(line.to_string());
            } else if let Some(active) = active_external.as_mut() {
                active.lines.push(line.to_string());
            }
            continue;
        }
        if let Some(opening_fence) = opening_fence(line) {
            fence = Some(opening_fence);
            if let Some(active) = active_capability.as_mut() {
                active.lines.push(line.to_string());
            } else if let Some(active) = active_external.as_mut() {
                active.lines.push(line.to_string());
            }
            continue;
        }

        if active_capability.is_some() {
            if DIRECTIVE_END.is_match(line) {
                let closed = active_capability
                    .take()
                    .expect("active Workflow Guide capability exists");
                let guide = closed.lines.join("\n").trim().to_string();
                collect_package_references(&guide, &mut package_paths);
                capabilities.push(WorkflowGuideCapability {
                    name: closed.name,
                    exposure: closed.exposure,
                    guide,
                    start_line: closed.start_line,
                    end_line: line_number,
                });
            } else if let Some(open) = active_capability.as_mut() {
                open.lines.push(line.to_string());
            }
            continue;
        }
        if active_external.is_some() {
            if DIRECTIVE_END.is_match(line) {
                let closed = active_external
                    .take()
                    .expect("active Workflow Guide external reference exists");
                let guide = closed.lines.join("\n").trim().to_string();
                collect_package_references(&guide, &mut package_paths);
                package_paths.insert(closed.path.clone());
                external_references.push(WorkflowGuideExternalReference {
                    title: closed.title,
                    path: closed.path,
                    guide,
                    start_line: closed.start_line,
                    end_line: line_number,
                });
            } else if let Some(open) = active_external.as_mut() {
                open.lines.push(line.to_string());
            }
            continue;
        }

        if let Some(captures) = CAPABILITY_START.captures(line) {
            match serde_json::from_str::<CapabilityDirectiveHeader>(&captures[1]) {
                Ok(header) if !header.name.trim().is_empty() && !header.name.contains(['\n', '\r']) => {
                    active_capability = Some(ActiveCapability {
                        name: header.name,
                        exposure: header.exposure,
                        start_line: line_number,
                        lines: Vec::new(),
                    });
                }
                Ok(_) => errors.push(WorkflowGuideParseError {
                    line: line_number,
                    message: "Capability name must not be empty".to_string(),
                }),
                Err(error) => errors.push(WorkflowGuideParseError {
                    line: line_number,
                    message: format!("invalid Capability directive: {error}"),
                }),
            }
            continue;
        }
        if line.trim_start().starts_with(":::capability") {
            errors.push(WorkflowGuideParseError {
                line: line_number,
                message: "invalid Capability directive; expected JSON name and exposure".to_string(),
            });
            continue;
        }
        if let Some(captures) = EXTERNAL_START.captures(line) {
            match serde_json::from_str::<ExternalDirectiveHeader>(&captures[1]) {
                Ok(header)
                    if !header.title.trim().is_empty()
                        && !header.title.contains(['\n', '\r'])
                        && header.path.starts_with("references/")
                        && header.path.ends_with(".md")
                        && !header.path.contains(['\n', '\r']) =>
                {
                    active_external = Some(ActiveExternal {
                        title: header.title,
                        path: header.path,
                        start_line: line_number,
                        lines: Vec::new(),
                    });
                }
                Ok(_) => errors.push(WorkflowGuideParseError {
                    line: line_number,
                    message: "External reference title and references/*.md path must not be empty".to_string(),
                }),
                Err(error) => errors.push(WorkflowGuideParseError {
                    line: line_number,
                    message: format!("invalid External directive: {error}"),
                }),
            }
            continue;
        }
        if line.trim_start().starts_with(":::external") {
            errors.push(WorkflowGuideParseError {
                line: line_number,
                message: "invalid External directive; expected JSON title and path".to_string(),
            });
            continue;
        }
        if DIRECTIVE_END.is_match(line) {
            errors.push(WorkflowGuideParseError {
                line: line_number,
                message: "Workflow Guide directive end has no matching start".to_string(),
            });
            continue;
        }
        if let Some((level, text)) = heading(line) {
            headings.push(WorkflowGuideHeading {
                level,
                text: text.to_string(),
                line: line_number,
            });
        }
        collect_package_references(line, &mut package_paths);
    }

    if let Some(active_capability) = active_capability {
        errors.push(WorkflowGuideParseError {
            line: active_capability.start_line,
            message: format!("Capability '{}' directive is not closed", active_capability.name),
        });
    }
    if let Some(active_external) = active_external {
        errors.push(WorkflowGuideParseError {
            line: active_external.start_line,
            message: format!("External reference '{}' directive is not closed", active_external.title),
        });
    }
    for (index, line) in markdown.lines().enumerate() {
        if UUID_REFERENCE.is_match(line) {
            errors.push(WorkflowGuideParseError {
                line: index + 1,
                message: "opaque identifiers are not allowed in a Workflow Guide".to_string(),
            });
        }
        if line.contains("skill://") {
            errors.push(WorkflowGuideParseError {
                line: index + 1,
                message: "skill:// references are not allowed in a Workflow Guide".to_string(),
            });
        }
    }

    if errors.is_empty() {
        Ok(WorkflowGuide {
            headings,
            capabilities,
            external_references,
            package_paths,
        })
    } else {
        Err(errors)
    }
}

pub fn effective_exposure_by_name(capabilities: &[WorkflowGuideCapability]) -> BTreeMap<String, WorkflowBindingPolicy> {
    let mut effective = BTreeMap::new();
    for capability in capabilities {
        match effective.get(&capability.name) {
            Some(WorkflowBindingPolicy::Direct) => {}
            _ => {
                effective.insert(capability.name.clone(), capability.exposure);
            }
        }
    }
    effective
}

pub fn render_workflow_skill(
    markdown: &str,
    guide: &WorkflowGuide,
    effective_exposure: &BTreeMap<String, WorkflowBindingPolicy>,
) -> RenderedWorkflowSkill {
    let capabilities_by_start = guide
        .capabilities
        .iter()
        .map(|capability| (capability.start_line, capability))
        .collect::<BTreeMap<_, _>>();
    let externals_by_start = guide
        .external_references
        .iter()
        .map(|external| (external.start_line, external))
        .collect::<BTreeMap<_, _>>();
    let mut output: Vec<String> = Vec::new();
    let lines = markdown.lines().collect::<Vec<_>>();
    let mut index = 0;
    let mut capability_section_shown = false;
    let use_capability_list_style = guide.capabilities.len() >= projection_config().workflow.capability.list_threshold;
    let (has_meta_on_demand, has_direct) = capability_exposure_summary(&guide.capabilities, effective_exposure);

    while index < lines.len() {
        let line_number = index + 1;
        if let Some(capability) = capabilities_by_start.get(&line_number) {
            let exposure = *effective_exposure.get(&capability.name).unwrap_or(&capability.exposure);
            if !capability_section_shown {
                push_blank_line_if_needed(&mut output);
                output.push(capability_section_intro(has_meta_on_demand, has_direct));
                capability_section_shown = true;
            }
            let previous_is_list_item = output.last().is_some_and(|line| line.starts_with("- "));
            if !use_capability_list_style || !previous_is_list_item {
                push_blank_line_if_needed(&mut output);
            }
            let rendered = render_capability_occurrence(capability, exposure, use_capability_list_style);
            output.push(rendered);
            let followed_by_capability = next_capability_after(&lines, capability.end_line, &capabilities_by_start);
            if (!use_capability_list_style || !followed_by_capability)
                && lines
                    .get(capability.end_line)
                    .is_some_and(|line| !line.trim().is_empty())
            {
                output.push(String::new());
            }
            index = capability.end_line;
            continue;
        }
        if let Some(external) = externals_by_start.get(&line_number) {
            push_blank_line_if_needed(&mut output);
            output.push(external_reference_body(
                &external.title,
                &external.path,
                &external.guide,
            ));
            if lines.get(external.end_line).is_some_and(|line| !line.trim().is_empty()) {
                output.push(String::new());
            }
            index = external.end_line;
            continue;
        }
        if let Some(captures) = STANDALONE_EXTERNAL_REFERENCE.captures(lines[index]) {
            push_blank_line_if_needed(&mut output);
            let path = match captures.get(3) {
                Some(fragment) => format!("{}{}", &captures[2], fragment.as_str()),
                None => captures[2].to_string(),
            };
            output.push(external_reference_body(&captures[1], &path, ""));
            index += 1;
            continue;
        }
        if use_capability_list_style
            && lines[index].trim().is_empty()
            && is_blank_between_consecutive_capabilities(
                line_number,
                &lines,
                &guide.capabilities,
                &capabilities_by_start,
            )
        {
            index += 1;
            continue;
        }
        output.push(lines[index].to_string());
        index += 1;
    }

    RenderedWorkflowSkill {
        markdown: collapse_blank_lines(&output.join("\n")),
    }
}

struct ActiveExternal {
    title: String,
    path: String,
    start_line: usize,
    lines: Vec<String>,
}

struct ActiveCapability {
    name: String,
    exposure: WorkflowBindingPolicy,
    start_line: usize,
    lines: Vec<String>,
}

#[derive(serde::Deserialize)]
struct ExternalDirectiveHeader {
    title: String,
    path: String,
}

#[derive(serde::Deserialize)]
struct CapabilityDirectiveHeader {
    name: String,
    exposure: WorkflowBindingPolicy,
}

#[derive(Clone, Copy)]
struct MarkdownFence {
    delimiter: char,
    length: usize,
}

fn opening_fence(line: &str) -> Option<MarkdownFence> {
    let trimmed = line.trim_start();
    let delimiter = trimmed.chars().next()?;
    if delimiter != '`' && delimiter != '~' {
        return None;
    }
    let length = trimmed.chars().take_while(|character| *character == delimiter).count();
    (length >= 3).then_some(MarkdownFence { delimiter, length })
}

fn closes_fence(
    line: &str,
    fence: MarkdownFence,
) -> bool {
    let trimmed = line.trim_start();
    let length = trimmed
        .chars()
        .take_while(|character| *character == fence.delimiter)
        .count();
    length >= fence.length && trimmed[length..].trim().is_empty()
}

fn contains_reserved_workflow_guide_syntax(line: &str) -> bool {
    line.trim_start().starts_with(":::capability")
        || line.trim_start().starts_with(":::external")
        || DIRECTIVE_END.is_match(line)
        || PACKAGE_FILE_REFERENCE.is_match(line)
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let trimmed = line.trim_start();
    let level = trimmed.chars().take_while(|character| *character == '#').count();
    if !(1..=6).contains(&level) || trimmed.as_bytes().get(level) != Some(&b' ') {
        return None;
    }
    let text = trimmed[level..].trim();
    (!text.is_empty()).then_some((level as u8, text))
}

fn collect_package_references(
    line: &str,
    package_paths: &mut BTreeSet<String>,
) {
    package_paths.extend(
        PACKAGE_FILE_REFERENCE
            .captures_iter(line)
            .map(|captures| captures[1].to_string()),
    );
}

fn capability_exposure_summary(
    capabilities: &[WorkflowGuideCapability],
    effective_exposure: &BTreeMap<String, WorkflowBindingPolicy>,
) -> (bool, bool) {
    let mut has_meta_on_demand = false;
    let mut has_direct = false;
    for capability in capabilities {
        let exposure = *effective_exposure.get(&capability.name).unwrap_or(&capability.exposure);
        match exposure {
            WorkflowBindingPolicy::MetaOnDemand => has_meta_on_demand = true,
            WorkflowBindingPolicy::Direct => has_direct = true,
        }
    }
    (has_meta_on_demand, has_direct)
}

fn push_blank_line_if_needed(output: &mut Vec<String>) {
    if output.last().is_some_and(|line| !line.trim().is_empty()) {
        output.push(String::new());
    }
}

fn next_capability_after(
    lines: &[&str],
    after_line: usize,
    capabilities_by_start: &BTreeMap<usize, &WorkflowGuideCapability>,
) -> bool {
    next_non_blank_line(lines, after_line + 1)
        .is_some_and(|line_number| capabilities_by_start.contains_key(&line_number))
}

fn next_non_blank_line(
    lines: &[&str],
    from_line: usize,
) -> Option<usize> {
    for line_number in from_line..=lines.len() {
        if lines[line_number - 1].trim().is_empty() {
            continue;
        }
        return Some(line_number);
    }
    None
}

fn previous_non_blank_line(
    lines: &[&str],
    before_line: usize,
) -> Option<usize> {
    for line_number in (1..=before_line).rev() {
        if lines[line_number - 1].trim().is_empty() {
            continue;
        }
        return Some(line_number);
    }
    None
}

fn line_in_capability(
    line_number: usize,
    capabilities: &[WorkflowGuideCapability],
) -> bool {
    capabilities
        .iter()
        .any(|capability| (capability.start_line..=capability.end_line).contains(&line_number))
}

fn is_blank_between_consecutive_capabilities(
    blank_line_number: usize,
    lines: &[&str],
    capabilities: &[WorkflowGuideCapability],
    capabilities_by_start: &BTreeMap<usize, &WorkflowGuideCapability>,
) -> bool {
    let Some(previous_line) = previous_non_blank_line(lines, blank_line_number - 1) else {
        return false;
    };
    let Some(next_line) = next_non_blank_line(lines, blank_line_number + 1) else {
        return false;
    };
    line_in_capability(previous_line, capabilities) && capabilities_by_start.contains_key(&next_line)
}

fn format_as_markdown_list_item(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "-".to_string();
    }
    let mut lines = trimmed.lines();
    let first = lines.next().expect("non-empty capability list item");
    let mut output = format!("- {first}");
    for line in lines {
        if line.trim().is_empty() {
            output.push('\n');
        } else {
            output.push_str("\n  ");
            output.push_str(line);
        }
    }
    output
}

fn render_capability_occurrence(
    capability: &WorkflowGuideCapability,
    exposure: WorkflowBindingPolicy,
    list_style: bool,
) -> String {
    let body = capability_item_body(&capability.name, &capability.guide, exposure);
    if list_style {
        format_as_markdown_list_item(&body)
    } else {
        body
    }
}

fn collapse_blank_lines(value: &str) -> String {
    value
        .lines()
        .fold((String::new(), 0_usize), |(mut output, blanks), line| {
            let next_blanks = usize::from(line.trim().is_empty()) * (blanks + 1);
            if next_blanks <= 2 {
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(line);
            }
            (output, next_blanks)
        })
        .0
        .trim()
        .to_string()
}

/// The Profile record owns a Skill's identity.  Imported standard Skills may
/// carry their own front matter, but that is source metadata rather than Guide
/// body content and must never become a second front-matter block on projection.
#[doc(hidden)]
pub fn normalize_main_guide_markdown(markdown: &str) -> Result<String, WorkflowGuideError> {
    let normalized = markdown.replace("\r\n", "\n");
    let Some(remainder) = normalized.strip_prefix("---\n") else {
        return Ok(normalized);
    };
    let Some(closing_offset) = remainder.find("\n---\n") else {
        return Err(WorkflowGuideError::InvalidStorage(
            "Skill front matter must be closed before the Guide body".to_string(),
        ));
    };
    let front_matter = &remainder[..closing_offset];
    let values: BTreeMap<String, serde_yaml::Value> = serde_yaml::from_str(front_matter)
        .map_err(|error| WorkflowGuideError::InvalidStorage(format!("Skill front matter is invalid YAML: {error}")))?;
    for key in ["name", "description"] {
        if !values.contains_key(key) {
            return Err(WorkflowGuideError::InvalidStorage(format!(
                "Skill front matter must include '{key}'"
            )));
        }
    }
    Ok(remainder[closing_offset + "\n---\n".len()..].to_string())
}

pub(super) fn format_parse_errors(errors: &[WorkflowGuideParseError]) -> String {
    errors
        .iter()
        .map(|error| format!("line {}: {}", error.line, error.message))
        .collect::<Vec<_>>()
        .join("; ")
}

#[doc(hidden)]
pub fn format_skill_definition(
    skill_name: &str,
    profile_name: &str,
    description: &str,
    compatibility: Option<&str>,
    body: &str,
) -> String {
    let description = if description.trim().is_empty() {
        format!("Workflow Guide for {}.", profile_name.trim())
    } else {
        description.trim().to_string()
    };
    format_projected_skill_markdown(skill_name, &description, compatibility, body)
}

pub(super) fn display_skill_name(profile_name: &str) -> String {
    let mut output = String::new();
    let mut pending_separator = false;
    for character in profile_name.trim().chars() {
        if character.is_ascii_alphanumeric() {
            if pending_separator && !output.is_empty() {
                output.push('-');
            }
            output.push(character.to_ascii_lowercase());
            pending_separator = false;
        } else {
            pending_separator = true;
        }
    }
    if output.is_empty() {
        "workflow-guide".to_string()
    } else {
        output
    }
}
