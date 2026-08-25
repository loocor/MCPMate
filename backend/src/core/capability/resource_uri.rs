use std::collections::{BTreeMap, BTreeSet, HashSet};

use anyhow::{Context, Result, bail};
use iri_string::template::UriTemplateStr;
use regex::Regex;
use sha2::{Digest, Sha256};
use url::Url;

const CANONICAL_SCHEME: &str = "mcpmate";
const RESOURCE_AUTHORITY: &str = "resources";
const TEMPLATE_SEGMENT: &str = "template";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResourceAddressKind {
    Static,
    Template,
}

impl ResourceAddressKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Static => "resource",
            Self::Template => "resource_template",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResourceAliasCandidates {
    pub(crate) preferred: String,
    pub(crate) expanded: String,
    pub(crate) digested: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ParsedUpstreamAddress {
    scheme: String,
    authority: Option<String>,
    path: String,
    query: Option<String>,
    fragment: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct MalformedCanonicalResourceUriError(#[source] anyhow::Error);

pub(crate) fn is_malformed_canonical_resource_uri_error(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|source| source.downcast_ref::<MalformedCanonicalResourceUriError>().is_some())
}

fn validate_namespace(namespace: &str) -> Result<()> {
    crate::config::server::validate_server_namespace(namespace)
        .map(|_| ())
        .with_context(|| format!("Invalid resource namespace '{namespace}'"))
}

fn validate_upstream_uri(upstream_uri: &str) -> Result<()> {
    if upstream_uri.is_empty() {
        bail!("Upstream resource URI cannot be empty");
    }
    Url::parse(upstream_uri).with_context(|| format!("Invalid upstream resource URI '{upstream_uri}'"))?;
    Ok(())
}

fn find_outside_expression(
    value: &str,
    targets: &[char],
) -> Option<usize> {
    let mut depth = 0_u32;
    for (index, character) in value.char_indices() {
        match character {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            _ if depth == 0 && targets.contains(&character) => return Some(index),
            _ => {}
        }
    }
    None
}

fn find_query_boundary(value: &str) -> Option<usize> {
    let mut depth = 0_u32;
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' if depth == 0
                && bytes
                    .get(index + 1)
                    .is_some_and(|operator| matches!(operator, b'?' | b'&')) =>
            {
                return Some(index);
            }
            b'{' => depth += 1,
            b'}' => depth = depth.saturating_sub(1),
            b'?' if depth == 0 => return Some(index),
            _ => {}
        }
        index += 1;
    }
    None
}

fn parse_upstream_address(value: &str) -> Result<ParsedUpstreamAddress> {
    let scheme_end =
        find_outside_expression(value, &[':']).context("Upstream resource URI or template must include a scheme")?;
    let scheme = &value[..scheme_end];
    let mut scheme_chars = scheme.chars();
    if !scheme_chars
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        || !scheme_chars.all(|character| character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.'))
    {
        bail!("Invalid upstream resource scheme '{scheme}'");
    }

    let mut remainder = &value[scheme_end + 1..];
    let authority = if let Some(after_prefix) = remainder.strip_prefix("//") {
        let end = [
            find_outside_expression(after_prefix, &['/', '#']),
            find_query_boundary(after_prefix),
        ]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(after_prefix.len());
        let authority = after_prefix[..end].to_string();
        remainder = &after_prefix[end..];
        Some(authority)
    } else {
        None
    };

    let fragment_start = find_outside_expression(remainder, &['#']);
    let (before_fragment, fragment) = match fragment_start {
        Some(index) => (&remainder[..index], Some(remainder[index + 1..].to_string())),
        None => (remainder, None),
    };
    let query_start = find_query_boundary(before_fragment);
    let (path, query) = match query_start {
        Some(index) => (
            before_fragment[..index].to_string(),
            Some(before_fragment[index..].to_string()),
        ),
        None => (before_fragment.to_string(), None),
    };

    Ok(ParsedUpstreamAddress {
        scheme: scheme.to_ascii_lowercase(),
        authority,
        path,
        query,
        fragment,
    })
}

fn is_generic_authority(authority: &str) -> bool {
    authority.is_empty() || authority.eq_ignore_ascii_case("resource") || authority.eq_ignore_ascii_case("resources")
}

fn encode_path_segment(segment: &str) -> String {
    let normalized_dots = segment.to_ascii_lowercase().replace("%2e", ".");
    if normalized_dots == "." {
        return "~dot".to_string();
    }
    if normalized_dots == ".." {
        return "~dotdot".to_string();
    }
    let bytes = segment.as_bytes();
    let mut encoded = String::with_capacity(segment.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            encoded.push('%');
            encoded.push((bytes[index + 1] as char).to_ascii_uppercase());
            encoded.push((bytes[index + 2] as char).to_ascii_uppercase());
            index += 3;
            continue;
        }
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to a String cannot fail");
        }
        index += 1;
    }
    encoded
}

fn expression_variables(expression: &str) -> Result<Vec<String>> {
    let template = UriTemplateStr::new(expression)
        .with_context(|| format!("Invalid resource template expression '{expression}'"))?;
    Ok(template_variables(template))
}

fn template_expression_operator(body: &str) -> Option<u8> {
    body.as_bytes()
        .first()
        .copied()
        .filter(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_' && *byte != b'%')
}

fn validate_routeable_resource_template(
    template: &str,
    address: &ParsedUpstreamAddress,
) -> Result<()> {
    if address.fragment.is_some() {
        bail!("Resource templates with literal fragments cannot enter the canonical address space");
    }
    if address
        .authority
        .as_deref()
        .is_some_and(|authority| authority.contains('{'))
    {
        bail!("Resource template variables in the authority cannot be reverse-routed reliably");
    }
    let (literals, expressions) = template_parts(template)?;
    for (index, expression) in expressions.iter().enumerate() {
        let operator = template_expression_operator(expression);
        if matches!(operator, Some(b'+') | Some(b'#')) {
            bail!("Resource template operator in '{{{expression}}}' cannot be reverse-routed reliably");
        }
        let variables = if operator.is_some() {
            &expression[1..]
        } else {
            expression
        };
        let specifications = variables.split(',').collect::<Vec<_>>();
        if specifications.is_empty() || specifications.iter().any(|specification| specification.is_empty()) {
            bail!("Resource template expression '{{{expression}}}' has no variables");
        }
        if specifications.iter().any(|specification| specification.contains(':')) {
            bail!("Resource template prefix modifiers cannot be reverse-routed reliably");
        }
        match operator {
            None => {
                if specifications.len() != 1 || specifications[0].ends_with('*') {
                    bail!("Simple Resource Template expressions must contain one scalar variable");
                }
            }
            Some(b'.' | b'/' | b';') => {
                if specifications.len() != 1 {
                    bail!("Path-like Resource Template expressions must contain one variable");
                }
            }
            Some(b'?' | b'&') => {
                if specifications.iter().any(|specification| specification.ends_with('*')) {
                    bail!("Exploded query variables cannot be reverse-routed reliably");
                }
            }
            Some(operator) => bail!(
                "Unsupported canonical resource template operator '{}'",
                operator as char
            ),
        }
        if index > 0 && literals[index].is_empty() {
            let previous_operator = template_expression_operator(expressions[index - 1]);
            let has_explicit_boundary = operator == Some(b'?')
                || (operator == Some(b'&') && matches!(previous_operator, Some(b'?') | Some(b'&')));
            if !has_explicit_boundary {
                bail!("Adjacent Resource Template expressions cannot be reverse-routed reliably");
            }
        }
    }
    Ok(())
}

fn transform_template_segment(
    segment: &str,
    include_variable_position: bool,
) -> Result<Option<String>> {
    let mut transformed = String::new();
    let mut cursor = 0;
    while let Some(relative_start) = segment[cursor..].find('{') {
        let start = cursor + relative_start;
        transformed.push_str(&encode_path_segment(&segment[cursor..start]));
        let end = segment[start..]
            .find('}')
            .map(|relative| start + relative + 1)
            .context("Invalid resource template expression")?;
        let variables = expression_variables(&segment[start..end])?;
        if include_variable_position {
            transformed.push_str("by-");
            transformed.push_str(&variables.join("-"));
            transformed.push('-');
        }
        transformed.push_str(&segment[start..end]);
        cursor = end;
    }
    transformed.push_str(&encode_path_segment(&segment[cursor..]));
    Ok((!transformed.is_empty()).then_some(transformed))
}

fn route_segments(
    kind: ResourceAddressKind,
    address: &ParsedUpstreamAddress,
    include_generic_authority: bool,
    include_variable_position: bool,
) -> Result<Vec<String>> {
    let mut segments = vec![encode_path_segment(&address.scheme)];
    if let Some(authority) = address
        .authority
        .as_deref()
        .filter(|authority| include_generic_authority || !is_generic_authority(authority))
    {
        if !authority.is_empty() {
            segments.push(encode_path_segment(&authority.to_ascii_lowercase()));
        }
    }
    for segment in split_path_segments(&address.path) {
        let segment = match kind {
            ResourceAddressKind::Static => Some(encode_path_segment(segment)),
            ResourceAddressKind::Template => transform_template_segment(segment, include_variable_position)?,
        };
        if let Some(segment) = segment.filter(|segment| !segment.is_empty()) {
            segments.push(segment);
        }
    }
    if include_variable_position {
        if let Some(fragment) = address.fragment.as_deref().filter(|fragment| !fragment.is_empty()) {
            segments.push(format!("fragment-{}", encode_path_segment(fragment)));
        }
    }
    Ok(segments)
}

fn split_path_segments(path: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut depth = 0_u32;
    let mut start = 0;
    for (index, character) in path.char_indices() {
        match character {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            '/' if depth == 0 => {
                if start < index {
                    segments.push(&path[start..index]);
                }
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < path.len() {
        segments.push(&path[start..]);
    }
    segments
}

fn build_alias(
    kind: ResourceAddressKind,
    namespace: &str,
    segments: &[String],
    query: Option<&str>,
) -> Result<String> {
    let prefix = match kind {
        ResourceAddressKind::Static => format!("{CANONICAL_SCHEME}://{RESOURCE_AUTHORITY}/{namespace}"),
        ResourceAddressKind::Template => {
            format!("{CANONICAL_SCHEME}://{RESOURCE_AUTHORITY}/{TEMPLATE_SEGMENT}/{namespace}")
        }
    };
    let mut alias = format!("{prefix}/{}", segments.join("/"));
    match kind {
        ResourceAddressKind::Static => {
            if let Some(query) = query.filter(|query| !query.is_empty()) {
                alias.push_str(query);
            }
            let parsed = Url::parse(&alias).with_context(|| format!("Invalid canonical resource URI '{alias}'"))?;
            alias = parsed.to_string();
        }
        ResourceAddressKind::Template => {
            if let Some(query) = query.filter(|query| !query.is_empty()) {
                alias.push_str(query);
            }
            UriTemplateStr::new(&alias).with_context(|| format!("Invalid canonical resource template '{alias}'"))?;
        }
    }
    Ok(alias)
}

fn append_digest(
    alias: &str,
    digest: &str,
) -> String {
    if let Some(expression_start) = alias.rfind("{?") {
        format!(
            "{}~{}{}",
            &alias[..expression_start],
            digest,
            &alias[expression_start..]
        )
    } else if let Some(query_start) = alias.find('?') {
        format!("{}~{}{}", &alias[..query_start], digest, &alias[query_start..])
    } else {
        format!("{alias}~{digest}")
    }
}

pub(crate) fn resource_alias_candidates(
    kind: ResourceAddressKind,
    namespace: &str,
    upstream_value: &str,
) -> Result<ResourceAliasCandidates> {
    validate_namespace(namespace)?;
    match kind {
        ResourceAddressKind::Static => validate_upstream_uri(upstream_value)?,
        ResourceAddressKind::Template => {
            UriTemplateStr::new(upstream_value)
                .with_context(|| format!("Invalid upstream resource template '{upstream_value}'"))?;
        }
    }
    let address = parse_upstream_address(upstream_value)?;
    if kind == ResourceAddressKind::Template {
        validate_routeable_resource_template(upstream_value, &address)?;
    }
    let preferred = build_alias(
        kind,
        namespace,
        &route_segments(kind, &address, false, false)?,
        address.query.as_deref(),
    )?;
    let expanded = build_alias(
        kind,
        namespace,
        &route_segments(kind, &address, true, true)?,
        address.query.as_deref(),
    )?;
    let mut hasher = Sha256::new();
    hasher.update(kind.as_str());
    hasher.update([0]);
    hasher.update(namespace);
    hasher.update([0]);
    hasher.update(upstream_value);
    let digest = format!("{:x}", hasher.finalize());
    let digested = append_digest(&expanded, &digest[..8]);

    Ok(ResourceAliasCandidates {
        preferred,
        expanded,
        digested,
    })
}

pub(crate) fn resource_template_is_projectable(
    namespace: &str,
    upstream_template: &str,
) -> Result<bool> {
    validate_namespace(namespace)?;
    UriTemplateStr::new(upstream_template)
        .with_context(|| format!("Invalid upstream resource template '{upstream_template}'"))?;
    Ok(resource_alias_candidates(ResourceAddressKind::Template, namespace, upstream_template).is_ok())
}

pub(crate) fn plan_resource_addresses(
    kind: ResourceAddressKind,
    namespace: &str,
    upstream_values: &[String],
    retained: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    plan_resource_addresses_with_reserved(kind, namespace, upstream_values, retained, &BTreeSet::new())
}

pub(crate) fn plan_resource_addresses_with_reserved(
    kind: ResourceAddressKind,
    namespace: &str,
    upstream_values: &[String],
    retained: &BTreeMap<String, String>,
    reserved_routes: &BTreeSet<String>,
) -> Result<BTreeMap<String, String>> {
    validate_namespace(namespace)?;
    let inventory = upstream_values.iter().cloned().collect::<BTreeSet<_>>();
    if inventory.len() != upstream_values.len() {
        bail!("Cannot plan resource addresses for server '{namespace}': duplicate upstream value");
    }

    let candidates = inventory
        .iter()
        .map(|upstream| {
            resource_alias_candidates(kind, namespace, upstream).map(|candidate| (upstream.clone(), candidate))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let routing_key = |external: &str| -> Result<String> {
        match kind {
            ResourceAddressKind::Static => Ok(external.to_string()),
            ResourceAddressKind::Template => template_match_key(external),
        }
    };
    let mut result = BTreeMap::new();
    let mut used = reserved_routes.clone();
    for upstream in &inventory {
        let Some(external) = retained.get(upstream) else {
            continue;
        };
        let candidate = &candidates[upstream];
        if external != &candidate.preferred && external != &candidate.expanded && external != &candidate.digested {
            continue;
        }
        let key = routing_key(external)?;
        if !used.insert(key) {
            bail!("Retained resource routing key for '{external}' is not unique");
        }
        result.insert(upstream.clone(), external.clone());
    }

    let mut preferred_groups = BTreeMap::<String, Vec<String>>::new();
    for upstream in inventory.iter().filter(|upstream| !result.contains_key(*upstream)) {
        preferred_groups
            .entry(routing_key(&candidates[upstream].preferred)?)
            .or_default()
            .push(upstream.clone());
    }
    for (preferred_key, group) in preferred_groups {
        if group.len() == 1 && !used.contains(&preferred_key) {
            let preferred = candidates[&group[0]].preferred.clone();
            used.insert(preferred_key);
            result.insert(group[0].clone(), preferred);
            continue;
        }

        let mut expanded_groups = BTreeMap::<String, Vec<String>>::new();
        for upstream in group {
            expanded_groups
                .entry(routing_key(&candidates[&upstream].expanded)?)
                .or_default()
                .push(upstream);
        }
        for (expanded_key, expanded_group) in expanded_groups {
            if expanded_group.len() == 1 && !used.contains(&expanded_key) {
                let expanded = candidates[&expanded_group[0]].expanded.clone();
                used.insert(expanded_key);
                result.insert(expanded_group[0].clone(), expanded);
                continue;
            }
            for upstream in expanded_group {
                let digested = candidates[&upstream].digested.clone();
                if !used.insert(routing_key(&digested)?) {
                    bail!("Deterministic resource address collision for upstream '{upstream}'");
                }
                result.insert(upstream, digested);
            }
        }
    }
    if kind == ResourceAddressKind::Template {
        resolve_template_route_overlaps(&mut result, &candidates, retained, reserved_routes)?;
    }
    Ok(result)
}

fn template_routes_may_overlap(
    first: &str,
    second: &str,
) -> Result<bool> {
    let (first_literals, _) = template_parts(first)?;
    let (second_literals, _) = template_parts(second)?;
    let first_prefix = first_literals.first().copied().unwrap_or_default();
    let second_prefix = second_literals.first().copied().unwrap_or_default();
    if !first_prefix.starts_with(second_prefix) && !second_prefix.starts_with(first_prefix) {
        return Ok(false);
    }
    let first_suffix = first_literals.last().copied().unwrap_or_default();
    let second_suffix = second_literals.last().copied().unwrap_or_default();
    Ok(first_suffix.ends_with(second_suffix) || second_suffix.ends_with(first_suffix))
}

fn resolve_template_route_overlaps(
    result: &mut BTreeMap<String, String>,
    candidates: &BTreeMap<String, ResourceAliasCandidates>,
    retained: &BTreeMap<String, String>,
    reserved_routes: &BTreeSet<String>,
) -> Result<()> {
    loop {
        let entries = result
            .iter()
            .map(|(upstream, external)| (upstream.clone(), external.clone()))
            .collect::<Vec<_>>();
        let mut overlap = None;
        'outer: for (index, (first_upstream, first_external)) in entries.iter().enumerate() {
            for (second_upstream, second_external) in entries.iter().skip(index + 1) {
                if template_routes_may_overlap(first_external, second_external)? {
                    overlap = Some((first_upstream.clone(), second_upstream.clone()));
                    break 'outer;
                }
            }
        }
        let Some((first, second)) = overlap else {
            return Ok(());
        };
        let first_retained = retained
            .get(&first)
            .is_some_and(|external| result.get(&first) == Some(external));
        let second_retained = retained
            .get(&second)
            .is_some_and(|external| result.get(&second) == Some(external));
        let victim = match (first_retained, second_retained) {
            (true, true) => bail!("Retained Resource Template routes overlap and cannot be routed unambiguously"),
            (true, false) => second,
            (false, true) => first,
            (false, false) => second,
        };
        let current = result[&victim].clone();
        let mut replacement = None;
        for candidate in [
            candidates[&victim].expanded.as_str(),
            candidates[&victim].digested.as_str(),
        ] {
            if candidate == current {
                continue;
            }
            let key = template_match_key(candidate)?;
            if reserved_routes.contains(&key) {
                continue;
            }
            let mut overlaps = false;
            for (_, external) in result.iter().filter(|(upstream, _)| upstream.as_str() != victim) {
                if template_routes_may_overlap(candidate, external)? {
                    overlaps = true;
                    break;
                }
            }
            if !overlaps {
                replacement = Some(candidate.to_string());
                break;
            }
        }
        let replacement = replacement
            .with_context(|| format!("Cannot allocate an unambiguous Resource Template route for '{victim}'"))?;
        result.insert(victim, replacement);
    }
}

pub(crate) fn template_match_key(external_template: &str) -> Result<String> {
    UriTemplateStr::new(external_template)
        .with_context(|| format!("Invalid canonical resource template '{external_template}'"))?;
    let mut key = String::with_capacity(external_template.len());
    let mut cursor = 0;
    while let Some(relative_start) = external_template[cursor..].find('{') {
        let start = cursor + relative_start;
        key.push_str(&external_template[cursor..start]);
        let end = external_template[start..]
            .find('}')
            .map(|relative| start + relative + 1)
            .context("Invalid canonical resource template expression")?;
        let body = &external_template[start + 1..end - 1];
        let operator = body
            .as_bytes()
            .first()
            .copied()
            .filter(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_' && *byte != b'%');
        if matches!(operator, Some(b'?') | Some(b'&')) {
            let names = expression_variable_names(body);
            let mut names = names.into_iter().map(|name| name.to_string()).collect::<Vec<_>>();
            names.sort();
            key.push('{');
            key.push(operator.expect("query operator") as char);
            key.push_str(&names.join(","));
            key.push('}');
        } else {
            key.push_str("{}");
        }
        cursor = end;
    }
    key.push_str(&external_template[cursor..]);
    Ok(key)
}

fn expression_variable_names(body: &str) -> Vec<&str> {
    let variables = if body
        .as_bytes()
        .first()
        .is_some_and(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_' && *byte != b'%')
    {
        &body[1..]
    } else {
        body
    };
    variables
        .split(',')
        .map(|variable| {
            variable
                .trim_end_matches('*')
                .split_once(':')
                .map_or(variable, |(name, _)| name)
        })
        .collect()
}

fn insert_captured_argument(
    arguments: &mut BTreeMap<String, String>,
    name: &str,
    value: String,
) -> Result<bool> {
    if let Some(existing) = arguments.get(name) {
        return Ok(existing == &value);
    }
    arguments.insert(name.to_string(), value);
    Ok(true)
}

fn decode_path_value(value: &str) -> Result<String> {
    validate_percent_encoding(value)?;
    percent_encoding::percent_decode_str(value)
        .decode_utf8()
        .map(|value| value.into_owned())
        .context("Canonical resource template argument is not valid UTF-8")
        .map_err(|source| MalformedCanonicalResourceUriError(source).into())
}

fn validate_percent_encoding(value: &str) -> Result<()> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                bail!("Canonical resource URI contains invalid percent encoding");
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn template_parts(template: &str) -> Result<(Vec<&str>, Vec<&str>)> {
    let mut literals = Vec::new();
    let mut expressions = Vec::new();
    let mut cursor = 0;
    while let Some(relative_start) = template[cursor..].find('{') {
        let start = cursor + relative_start;
        literals.push(&template[cursor..start]);
        let end = template[start..]
            .find('}')
            .map(|relative| start + relative + 1)
            .context("Invalid canonical resource template expression")?;
        expressions.push(&template[start + 1..end - 1]);
        cursor = end;
    }
    literals.push(&template[cursor..]);
    Ok((literals, expressions))
}

fn expression_expansion_pattern(body: &str) -> Result<&'static str> {
    let operator = template_expression_operator(body);
    let exploded = body.ends_with('*');
    match operator {
        None => Ok("[^/?#]*"),
        Some(b'.') if exploded => Ok("(?:\\.[^./?#]*)*"),
        Some(b'.') => Ok("(?:\\.[^./?#]*)?"),
        Some(b'/') if exploded => Ok("(?:/[^/?#]*)*"),
        Some(b'/') => Ok("(?:/[^/?#]*)?"),
        Some(b';') => Ok("(?:;[^/?#]*)*"),
        Some(b'?') => Ok("(?:\\?[^#]*)?"),
        Some(b'&') => Ok("(?:&[^#]*)?"),
        Some(operator) => bail!(
            "Unsupported canonical resource template operator '{}'",
            operator as char
        ),
    }
}

fn query_expression_run_end(
    expressions: &[&str],
    literals: &[&str],
    start: usize,
) -> usize {
    if template_expression_operator(expressions[start]) != Some(b'?') {
        return start + 1;
    }

    let mut end = start + 1;
    while end < expressions.len()
        && literals[end].is_empty()
        && template_expression_operator(expressions[end]) == Some(b'&')
    {
        end += 1;
    }
    end
}

type TemplateExpressionCaptures<'a> = (Vec<&'a str>, Vec<&'a str>, Vec<String>);

fn capture_template_expansions<'a>(
    template: &'a str,
    concrete_uri: &str,
) -> Result<Option<TemplateExpressionCaptures<'a>>> {
    let (literals, expressions) = template_parts(template)?;
    let mut pattern = String::from("^");
    let mut capture_groups = Vec::new();
    let mut index = 0;
    while index < expressions.len() {
        pattern.push_str(&regex::escape(literals[index]));
        pattern.push('(');
        let end = query_expression_run_end(&expressions, &literals, index);
        pattern.push_str(expression_expansion_pattern(expressions[index])?);
        pattern.push(')');
        capture_groups.push((index, end));
        index = end;
    }
    pattern.push_str(&regex::escape(literals.last().copied().unwrap_or_default()));
    pattern.push('$');
    let matcher = Regex::new(&pattern).context("Failed to compile canonical resource template matcher")?;
    let Some(captures) = matcher.captures(concrete_uri) else {
        return Ok(None);
    };
    let mut expansions = vec![String::new(); expressions.len()];
    for (capture_index, (start, _)) in capture_groups.into_iter().enumerate() {
        expansions[start] = captures
            .get(capture_index + 1)
            .map(|capture| capture.as_str().to_string())
            .unwrap_or_default();
    }
    Ok(Some((literals, expressions, expansions)))
}

fn decode_query_value(value: &str) -> Result<String> {
    validate_percent_encoding(value)?;
    percent_encoding::percent_decode_str(&value.replace('+', " "))
        .decode_utf8()
        .map(|value| value.into_owned())
        .context("Canonical resource template query argument is not valid UTF-8")
        .map_err(|source| MalformedCanonicalResourceUriError(source).into())
}

fn captured_query_arguments(query: &str) -> Result<Vec<(String, String)>> {
    query
        .split('&')
        .filter(|parameter| !parameter.is_empty())
        .map(|parameter| {
            let (name, value) = parameter.split_once('=').unwrap_or((parameter, ""));
            Ok((decode_query_value(name)?, decode_query_value(value)?))
        })
        .collect()
}

fn captured_template_arguments(
    literals: &[&str],
    expressions: &[&str],
    expansions: &[String],
) -> Result<Option<BTreeMap<String, String>>> {
    let mut arguments = BTreeMap::new();
    let mut index = 0;
    while index < expressions.len() {
        let expression = expressions[index];
        let expansion = &expansions[index];
        let operator = expression
            .as_bytes()
            .first()
            .copied()
            .filter(|byte| !byte.is_ascii_alphanumeric() && *byte != b'_' && *byte != b'%');
        if matches!(operator, Some(b'?') | Some(b'&')) {
            let end = query_expression_run_end(expressions, literals, index);
            let variables = expressions[index..end]
                .iter()
                .flat_map(|expression| expression_variable_names(expression))
                .collect::<Vec<_>>();
            let accepts_dynamic_keys = variables
                .iter()
                .any(|variable| expression.contains(&format!("{variable}*")));
            let allowed = variables.into_iter().collect::<HashSet<_>>();
            let query = expansion.trim_start_matches(['?', '&']);
            let mut seen = HashSet::new();
            for (name, value) in captured_query_arguments(query)? {
                if !seen.insert(name.clone()) || (!accepts_dynamic_keys && !allowed.contains(name.as_str())) {
                    return Ok(None);
                }
                if allowed.contains(name.as_str()) && !insert_captured_argument(&mut arguments, &name, value)? {
                    return Ok(None);
                }
            }
            index = end;
            continue;
        }

        let variables = expression_variable_names(expression);
        if variables.len() == 1 {
            let value = expansion
                .trim_start_matches(['/', '.', ';', '#'])
                .strip_prefix(&format!("{}=", variables[0]))
                .unwrap_or_else(|| expansion.trim_start_matches(['/', '.', ';', '#']));
            if !value.is_empty() && !insert_captured_argument(&mut arguments, variables[0], decode_path_value(value)?)?
            {
                return Ok(None);
            }
        }
        index += 1;
    }
    Ok(Some(arguments))
}

pub(crate) fn match_resource_template(
    external_template: &str,
    external_uri: &str,
) -> Result<Option<BTreeMap<String, String>>> {
    UriTemplateStr::new(external_template)
        .with_context(|| format!("Invalid canonical resource template '{external_template}'"))?;
    let parsed_template = parse_upstream_address(external_template)?;
    validate_routeable_resource_template(external_template, &parsed_template)?;
    validate_percent_encoding(external_uri)?;
    url::Url::parse(external_uri).with_context(|| format!("Invalid canonical resource URI '{external_uri}'"))?;
    let Some((literals, expressions, expansions)) = capture_template_expansions(external_template, external_uri)?
    else {
        return Ok(None);
    };
    captured_template_arguments(&literals, &expressions, &expansions)
}

pub(crate) fn expand_upstream_resource_template(
    external_template: &str,
    upstream_template: &str,
    external_uri: &str,
) -> Result<Option<(String, BTreeMap<String, String>)>> {
    UriTemplateStr::new(upstream_template)
        .with_context(|| format!("Invalid persisted upstream resource template '{upstream_template}'"))?;
    validate_percent_encoding(external_uri)?;
    let Some(arguments) = match_resource_template(external_template, external_uri)? else {
        return Ok(None);
    };
    let Some((_, external_expressions, expansions)) = capture_template_expansions(external_template, external_uri)?
    else {
        return Ok(None);
    };
    let (upstream_literals, upstream_expressions) = template_parts(upstream_template)?;
    if external_expressions != upstream_expressions {
        bail!("Canonical resource template does not preserve its upstream expression structure");
    }
    let mut upstream_uri = String::new();
    for (index, expansion) in expansions.iter().enumerate() {
        upstream_uri.push_str(upstream_literals[index]);
        upstream_uri.push_str(expansion);
    }
    upstream_uri.push_str(upstream_literals.last().copied().unwrap_or_default());
    Url::parse(&upstream_uri)
        .with_context(|| format!("Expanded upstream resource URI is invalid: '{upstream_uri}'"))?;
    Ok(Some((upstream_uri, arguments)))
}

pub(crate) fn project_upstream_resource_uri_through_template(
    external_template: &str,
    upstream_template: &str,
    upstream_uri: &str,
) -> Result<Option<String>> {
    UriTemplateStr::new(external_template)
        .with_context(|| format!("Invalid canonical resource template '{external_template}'"))?;
    UriTemplateStr::new(upstream_template)
        .with_context(|| format!("Invalid persisted upstream resource template '{upstream_template}'"))?;
    validate_percent_encoding(upstream_uri)?;
    Url::parse(upstream_uri).with_context(|| format!("Invalid upstream resource URI '{upstream_uri}'"))?;

    let Some((_, upstream_expressions, expansions)) = capture_template_expansions(upstream_template, upstream_uri)?
    else {
        return Ok(None);
    };
    let (external_literals, external_expressions) = template_parts(external_template)?;
    if external_expressions != upstream_expressions {
        bail!("Canonical resource template does not preserve its upstream expression structure");
    }

    let mut external_uri = String::new();
    for (index, expansion) in expansions.iter().enumerate() {
        external_uri.push_str(external_literals[index]);
        external_uri.push_str(expansion);
    }
    external_uri.push_str(external_literals.last().copied().unwrap_or_default());
    match_resource_template(external_template, &external_uri)?
        .context("Canonical resource template projection did not preserve a valid route")?;
    Ok(Some(external_uri))
}

fn template_variables(template: &UriTemplateStr) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut variables = Vec::new();
    for variable in template.variables() {
        let name = variable.as_str().to_string();
        if seen.insert(name.clone()) {
            variables.push(name);
        }
    }
    variables
}

pub(crate) fn encode_resource_uri(
    namespace: &str,
    upstream_uri: &str,
) -> Result<String> {
    Ok(resource_alias_candidates(ResourceAddressKind::Static, namespace, upstream_uri)?.preferred)
}

pub(crate) fn encode_resource_template(
    namespace: &str,
    upstream_template: &str,
) -> Result<String> {
    Ok(resource_alias_candidates(ResourceAddressKind::Template, namespace, upstream_template)?.preferred)
}

async fn rewrite_resource_contents(
    registry_pool: &sqlx::Pool<sqlx::Sqlite>,
    server_id: &str,
    namespace: &str,
    content: &mut rmcp::model::ResourceContents,
) -> Result<()> {
    match content {
        rmcp::model::ResourceContents::TextResourceContents { uri, .. }
        | rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => {
            *uri = crate::core::capability::resource_registry::issue_resource_route(
                registry_pool,
                server_id,
                namespace,
                uri,
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

pub(crate) async fn rewrite_call_tool_result(
    registry_pool: &sqlx::Pool<sqlx::Sqlite>,
    server_id: &str,
    namespace: &str,
    result: &mut rmcp::model::CallToolResult,
) -> Result<()> {
    for content in &mut result.content {
        match content {
            rmcp::model::ContentBlock::Resource(resource) => {
                rewrite_resource_contents(registry_pool, server_id, namespace, &mut resource.resource).await?;
            }
            rmcp::model::ContentBlock::ResourceLink(resource) => {
                resource.uri = crate::core::capability::resource_registry::issue_resource_route(
                    registry_pool,
                    server_id,
                    namespace,
                    &resource.uri,
                )
                .await?;
            }
            rmcp::model::ContentBlock::Text(_)
            | rmcp::model::ContentBlock::Image(_)
            | rmcp::model::ContentBlock::Audio(_) => {}
            _ => {}
        }
    }
    Ok(())
}

pub(crate) async fn rewrite_get_prompt_result(
    registry_pool: &sqlx::Pool<sqlx::Sqlite>,
    server_id: &str,
    namespace: &str,
    result: &mut rmcp::model::GetPromptResult,
) -> Result<()> {
    for message in &mut result.messages {
        match &mut message.content {
            rmcp::model::ContentBlock::Resource(resource) => {
                rewrite_resource_contents(registry_pool, server_id, namespace, &mut resource.resource).await?;
            }
            rmcp::model::ContentBlock::ResourceLink(link) => {
                link.uri = crate::core::capability::resource_registry::issue_resource_route(
                    registry_pool,
                    server_id,
                    namespace,
                    &link.uri,
                )
                .await?;
            }
            rmcp::model::ContentBlock::Text(_)
            | rmcp::model::ContentBlock::Image(_)
            | rmcp::model::ContentBlock::Audio(_) => {}
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn projection_pool() -> sqlx::Pool<sqlx::Sqlite> {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect registry database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .expect("initialize profile tables");
        sqlx::query("INSERT INTO server_config (id, name, server_type) VALUES ('server-a', 'docs', 'stdio')")
            .execute(&pool)
            .await
            .expect("insert server");
        pool
    }

    async fn template_projection_pool(templates: &[(&str, &str)]) -> sqlx::Pool<sqlx::Sqlite> {
        let pool = projection_pool().await;
        sqlx::query("UPDATE server_config SET name = 'everything' WHERE id = 'server-a'")
            .execute(&pool)
            .await
            .expect("align fixture namespace");
        for (template, name) in templates {
            crate::config::server::capabilities::upsert_shadow_resource_template(
                &pool,
                "server-a",
                "everything",
                template,
                Some(name),
                None,
            )
            .await
            .expect("register resource template");
        }
        pool
    }

    #[test]
    fn everything_resource_uses_readable_canonical_uri() {
        assert_eq!(
            encode_resource_uri("everything", "demo://resource/static/document/architecture.md",)
                .expect("encode readable static resource"),
            "mcpmate://resources/everything/demo/static/document/architecture.md"
        );
    }

    #[test]
    fn everything_template_uses_readable_canonical_uri() {
        assert_eq!(
            encode_resource_template("everything", "demo://resource/dynamic/text/{resourceId}",)
                .expect("encode readable resource template"),
            "mcpmate://resources/template/everything/demo/dynamic/text/{resourceId}"
        );
    }

    #[test]
    fn canonical_template_keeps_namespace_and_upstream_scheme_even_when_equal() {
        assert_eq!(
            encode_resource_template("search", "search://items{?q,lang}")
                .expect("encode template with repeated namespace and scheme"),
            "mcpmate://resources/template/search/search/items{?q,lang}"
        );
    }

    #[test]
    fn canonical_template_preserves_rfc6570_path_operators() {
        assert_eq!(
            encode_resource_template("docs", "file:///files{/segments*}{?revision}")
                .expect("encode path operator template"),
            "mcpmate://resources/template/docs/file/files{/segments*}{?revision}"
        );
    }

    #[test]
    fn rejects_templates_that_cannot_be_reverse_routed_unambiguously() {
        for template in [
            "file:///{+path}",
            "file:///{id:3}",
            "file:///{first}{second}",
            "https://{host}/document",
            "search://items{?tag*}",
        ] {
            assert!(
                encode_resource_template("docs", template).is_err(),
                "unsupported reverse-routing template must fail: {template}"
            );
        }
    }

    #[test]
    fn valid_upstream_template_can_be_unprojectable() {
        assert!(UriTemplateStr::new("file:///{+path}").is_ok());
        assert!(
            !resource_template_is_projectable("docs", "file:///{+path}")
                .expect("validate upstream template independently from canonical projection")
        );
    }

    #[test]
    fn matches_structural_path_and_query_arguments() {
        let arguments = match_resource_template(
            "mcpmate://resources/template/docs/demo/report-{year}.md/{resourceId}{?lang}",
            "mcpmate://resources/template/docs/demo/report-2026.md/42?lang=zh-CN",
        )
        .expect("match canonical template")
        .expect("canonical route matches");

        assert_eq!(
            arguments,
            BTreeMap::from([
                ("lang".to_string(), "zh-CN".to_string()),
                ("resourceId".to_string(), "42".to_string()),
                ("year".to_string(), "2026".to_string()),
            ])
        );
    }

    #[test]
    fn matches_adjacent_query_expressions_in_declared_order() {
        let arguments = match_resource_template(
            "mcpmate://resources/template/docs/demo/items{?a}{&b}",
            "mcpmate://resources/template/docs/demo/items?a=one&b=two",
        )
        .expect("match canonical template")
        .expect("canonical route matches");

        assert_eq!(
            arguments,
            BTreeMap::from([
                ("a".to_string(), "one".to_string()),
                ("b".to_string(), "two".to_string()),
            ])
        );
    }

    #[test]
    fn matches_adjacent_query_expressions_regardless_of_parameter_order() {
        let arguments = match_resource_template(
            "mcpmate://resources/template/docs/demo/items{?a}{&b}",
            "mcpmate://resources/template/docs/demo/items?b=two&a=one",
        )
        .expect("match canonical template")
        .expect("canonical route matches");

        assert_eq!(
            arguments,
            BTreeMap::from([
                ("a".to_string(), "one".to_string()),
                ("b".to_string(), "two".to_string()),
            ])
        );
    }

    #[test]
    fn adjacent_query_expressions_allow_missing_optional_values() {
        let arguments = match_resource_template(
            "mcpmate://resources/template/docs/demo/items{?a}{&b}",
            "mcpmate://resources/template/docs/demo/items?b=two",
        )
        .expect("match canonical template")
        .expect("canonical route matches");

        assert_eq!(arguments, BTreeMap::from([("b".to_string(), "two".to_string())]));
    }

    #[test]
    fn adjacent_query_expressions_reject_unknown_or_duplicate_parameters() {
        let template = "mcpmate://resources/template/docs/demo/items{?a}{&b}";

        assert!(
            match_resource_template(
                template,
                "mcpmate://resources/template/docs/demo/items?a=one&extra=three"
            )
            .expect("unknown parameter is a valid URI")
            .is_none()
        );
        assert!(
            match_resource_template(template, "mcpmate://resources/template/docs/demo/items?a=one&a=two")
                .expect("duplicate parameter is a valid URI")
                .is_none()
        );
    }

    #[test]
    fn adjacent_query_expressions_reject_invalid_percent_encoding() {
        assert!(
            match_resource_template(
                "mcpmate://resources/template/docs/demo/items{?a}{&b}",
                "mcpmate://resources/template/docs/demo/items?a=%ZZ",
            )
            .is_err()
        );
    }

    #[test]
    fn adjacent_query_expressions_reject_query_values_that_are_not_utf8() {
        assert!(
            match_resource_template(
                "mcpmate://resources/template/docs/demo/items{?a}{&b}",
                "mcpmate://resources/template/docs/demo/items?a=%FF",
            )
            .is_err()
        );
    }

    #[test]
    fn adjacent_query_expressions_preserve_concrete_query_during_upstream_reconstruction() {
        let (upstream_uri, arguments) = expand_upstream_resource_template(
            "mcpmate://resources/template/docs/demo/items{?a}{&b}",
            "demo://resource/items{?a}{&b}",
            "mcpmate://resources/template/docs/demo/items?b=two&a=one",
        )
        .expect("expand canonical template")
        .expect("canonical route matches");

        assert_eq!(upstream_uri, "demo://resource/items?b=two&a=one");
        assert_eq!(
            arguments,
            BTreeMap::from([
                ("a".to_string(), "one".to_string()),
                ("b".to_string(), "two".to_string()),
            ])
        );
    }

    #[test]
    fn readable_alias_preserves_meaningful_authority_and_omits_generic_authority() {
        let meaningful = resource_alias_candidates(
            ResourceAddressKind::Static,
            "docs",
            "https://api.example.com/docs/readme.md",
        )
        .expect("plan meaningful authority");
        let generic =
            resource_alias_candidates(ResourceAddressKind::Static, "docs", "demo://resources/static/readme.md")
                .expect("plan generic authority");
        let empty = resource_alias_candidates(ResourceAddressKind::Static, "docs", "file:///guide.md")
            .expect("plan empty authority");

        assert_eq!(
            meaningful.preferred,
            "mcpmate://resources/docs/https/api.example.com/docs/readme.md"
        );
        assert_eq!(generic.preferred, "mcpmate://resources/docs/demo/static/readme.md");
        assert_eq!(empty.preferred, "mcpmate://resources/docs/file/guide.md");
    }

    #[test]
    fn readable_alias_preserves_static_query_and_encodes_fragment_without_wrapper_fragment() {
        let candidates = resource_alias_candidates(
            ResourceAddressKind::Static,
            "docs",
            "demo://resource/item/指南?lang=zh-CN#overview",
        )
        .expect("plan static query and fragment");

        assert_eq!(
            candidates.preferred,
            "mcpmate://resources/docs/demo/item/%E6%8C%87%E5%8D%97?lang=zh-CN"
        );
        assert!(!candidates.expanded.contains('#'));
        assert!(candidates.expanded.contains("overview"));
    }

    #[test]
    fn readable_template_preserves_path_and_query_variable_roles() {
        let candidates = resource_alias_candidates(
            ResourceAddressKind::Template,
            "docs",
            "demo://resource/report-{year}.md/{resourceId}/{resourceId}{?lang}",
        )
        .expect("plan mixed resource template");

        assert_eq!(
            candidates.preferred,
            "mcpmate://resources/template/docs/demo/report-{year}.md/{resourceId}/{resourceId}{?lang}"
        );
    }

    #[test]
    fn reserved_template_namespace_cannot_enter_the_static_address_space() {
        assert!(
            resource_alias_candidates(ResourceAddressKind::Static, "template", "demo://resource/static.txt").is_err()
        );
    }

    #[test]
    fn collision_groups_expand_deterministically_before_using_digest() {
        let authority_collision = vec![
            "demo://resource/dynamic/{id}".to_string(),
            "demo://resources/dynamic/{id}".to_string(),
        ];
        let forward = plan_resource_addresses(
            ResourceAddressKind::Template,
            "everything",
            &authority_collision,
            &BTreeMap::new(),
        )
        .expect("plan authority collision");
        let reverse = plan_resource_addresses(
            ResourceAddressKind::Template,
            "everything",
            &authority_collision.iter().cloned().rev().collect::<Vec<_>>(),
            &BTreeMap::new(),
        )
        .expect("plan reversed authority collision");

        assert_eq!(forward, reverse);
        assert_ne!(forward[&authority_collision[0]], forward[&authority_collision[1]]);
        assert!(forward[&authority_collision[0]].contains("/resource/"));
        assert!(forward[&authority_collision[1]].contains("/resources/"));

        let final_collision = vec![
            "DEMO://resource/static.txt".to_string(),
            "demo://resource/static.txt".to_string(),
        ];
        let digested = plan_resource_addresses(
            ResourceAddressKind::Static,
            "everything",
            &final_collision,
            &BTreeMap::new(),
        )
        .expect("plan final collision");
        assert_ne!(digested[&final_collision[0]], digested[&final_collision[1]]);
        assert!(digested.values().all(|value| value.contains('~')));
    }

    #[test]
    fn retained_readable_alias_is_sticky_and_deleted_upstream_is_removed() {
        let retained_uri = "demo://resource/status".to_string();
        let new_uri = "demo://resources/status".to_string();
        let deleted_uri = "demo://resource/deleted".to_string();
        let retained_alias = "mcpmate://resources/everything/demo/status".to_string();
        let retained = BTreeMap::from([
            (retained_uri.clone(), retained_alias.clone()),
            (deleted_uri, "mcpmate://resources/everything/demo/deleted".to_string()),
        ]);

        let plan = plan_resource_addresses(
            ResourceAddressKind::Static,
            "everything",
            &[retained_uri.clone(), new_uri.clone()],
            &retained,
        )
        .expect("plan sticky mappings");

        assert_eq!(plan.len(), 2);
        assert_eq!(plan[&retained_uri], retained_alias);
        assert_ne!(plan[&new_uri], retained_alias);
    }

    #[test]
    fn template_planning_keeps_match_spaces_unique() {
        let first = "demo://resource/dynamic/{id}".to_string();
        let second = "demo://resource/dynamic/{name}".to_string();
        let plan = plan_resource_addresses(
            ResourceAddressKind::Template,
            "everything",
            &[first.clone(), second.clone()],
            &BTreeMap::new(),
        )
        .expect("plan templates with the same preferred route base");

        assert_ne!(plan[&first], plan[&second]);
    }

    #[test]
    fn template_preserves_supported_path_and_query_expressions() {
        let upstream = "file:///files{/segments*}{?revision}";
        let encoded = encode_resource_template("docs", upstream).expect("encode resource template");

        assert_eq!(
            encoded,
            "mcpmate://resources/template/docs/file/files{/segments*}{?revision}"
        );
    }

    #[test]
    fn potentially_overlapping_template_routes_receive_distinct_address_spaces() {
        let first = "demo://resource/{value}/fixed".to_string();
        let second = "demo://resource/literal/{value}".to_string();
        let plan = plan_resource_addresses(
            ResourceAddressKind::Template,
            "everything",
            &[first.clone(), second.clone()],
            &BTreeMap::new(),
        )
        .expect("plan overlapping templates");

        assert!(!template_routes_may_overlap(&plan[&first], &plan[&second]).expect("compare planned routes"));
    }

    #[test]
    fn static_resource_preserves_unicode_custom_scheme_and_percent_encoding() {
        let upstream = "workspace+docs://项目/指南?key=%E5%80%BC#章节";
        let encoded = encode_resource_uri("knowledge", upstream).expect("encode unicode resource");

        assert_eq!(
            encoded,
            "mcpmate://resources/knowledge/workspace%2Bdocs/%E9%A1%B9%E7%9B%AE/%E6%8C%87%E5%8D%97?key=%E5%80%BC"
        );
    }

    #[test]
    fn fragment_expansion_is_rejected_before_a_template_is_exposed() {
        assert!(encode_resource_template("docs", "file:///document{#section}").is_err());
    }

    #[test]
    fn root_and_dot_segments_keep_a_stable_canonical_structure() {
        assert_eq!(
            encode_resource_uri("docs", "file:///").expect("encode root resource"),
            "mcpmate://resources/docs/file"
        );
        assert_eq!(
            encode_resource_uri("docs", "file:///../secret").expect("encode parent segment"),
            "mcpmate://resources/docs/file/~dotdot/secret"
        );
        assert_eq!(
            encode_resource_uri("docs", "file:///%2E%2E/secret").expect("encode encoded parent segment"),
            "mcpmate://resources/docs/file/~dotdot/secret"
        );
    }

    #[test]
    fn template_variables_are_deduplicated_in_first_seen_order() {
        let encoded =
            encode_resource_template("docs", "file:///{first}/{second}{?first}").expect("encode repeated variables");

        assert_eq!(
            encoded,
            "mcpmate://resources/template/docs/file/{first}/{second}{?first}"
        );
    }

    #[test]
    fn percent_encoded_template_variable_name_remains_distinct() {
        let encoded =
            encode_resource_template("docs", "file:///{%70ath}").expect("encode percent-encoded variable name");
        assert_eq!(encoded, "mcpmate://resources/template/docs/file/{%70ath}");
    }

    #[test]
    fn rejects_invalid_namespace() {
        assert!(encode_resource_uri("Invalid Namespace", "file:///a").is_err());
    }

    #[test]
    fn rejects_invalid_upstream_template() {
        assert!(encode_resource_template("docs", "file:///{unclosed").is_err());
    }

    #[tokio::test]
    async fn rewrites_only_typed_embedded_resource_content_in_tool_results() {
        use rmcp::model::{
            Annotations, AudioContent, CallToolResult, ContentBlock, Icon, MetaObject, Resource, ResourceContents,
            Role, TextContent,
        };

        let annotations = Annotations::default()
            .with_audience(vec![Role::User])
            .with_priority(0.75);
        let mut meta = MetaObject::new();
        meta.0.insert("source".to_string(), serde_json::json!("upstream"));

        let mut result = CallToolResult::structured(serde_json::json!({
            "url": "file:///structured.json"
        }));
        result.content = vec![
            ContentBlock::resource_link(
                Resource::new("file:///linked.md", "linked")
                    .with_description("Linked resource at file:///description.md")
                    .with_icons(vec![Icon::new("https://example.com/icon.png")])
                    .with_meta(meta.clone())
                    .with_annotations(annotations.clone()),
            ),
            ContentBlock::resource(ResourceContents::text("embedded", "file:///embedded.md")),
            ContentBlock::Text(
                TextContent::new("file:///plain-text.md")
                    .with_meta(meta.clone())
                    .with_annotations(annotations.clone()),
            ),
            ContentBlock::Audio(
                AudioContent::new("file:///audio-data.wav", "audio/wav")
                    .with_meta(meta.clone())
                    .with_annotations(annotations.clone()),
            ),
        ];

        let pool = projection_pool().await;
        rewrite_call_tool_result(&pool, "server-a", "docs", &mut result)
            .await
            .expect("rewrite tool result");

        assert_eq!(
            result.content[0].as_resource_link().expect("resource link").uri,
            encode_resource_uri("docs", "file:///linked.md").expect("encode link")
        );
        assert_eq!(
            result.content[0].as_resource_link().expect("resource link").icons,
            Some(vec![Icon::new("https://example.com/icon.png")])
        );
        assert_eq!(
            result.content[0]
                .as_resource_link()
                .expect("resource link")
                .description
                .as_deref(),
            Some("Linked resource at file:///description.md")
        );
        assert_eq!(
            result.content[0].as_resource_link().expect("resource link").meta,
            Some(meta.clone())
        );
        assert_eq!(
            result.content[0].as_resource_link().expect("resource link").annotations,
            Some(annotations.clone())
        );
        match &result.content[1].as_resource().expect("embedded resource").resource {
            ResourceContents::TextResourceContents { uri, text, .. } => {
                assert_eq!(
                    uri,
                    &encode_resource_uri("docs", "file:///embedded.md").expect("encode embedded")
                );
                assert_eq!(text, "embedded");
            }
            ResourceContents::BlobResourceContents { .. } => panic!("expected text resource"),
            _ => panic!("expected known resource contents"),
        }
        assert_eq!(result.content[2].as_text().expect("text").text, "file:///plain-text.md");
        assert_eq!(result.content[2].as_text().expect("text").meta, Some(meta.clone()));
        assert_eq!(
            result.content[2].as_text().expect("text").annotations,
            Some(annotations.clone())
        );
        assert_eq!(
            result.content[3].as_audio().expect("audio").data,
            "file:///audio-data.wav"
        );
        assert_eq!(result.content[3].as_audio().expect("audio").meta, Some(meta));
        assert_eq!(
            result.content[3].as_audio().expect("audio").annotations,
            Some(annotations)
        );
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({"url": "file:///structured.json"}))
        );
    }

    #[tokio::test]
    async fn rewrites_only_typed_embedded_resource_content_in_prompt_results() {
        use rmcp::model::{ContentBlock, GetPromptResult, PromptMessage, Resource, ResourceContents, Role};

        let link = Resource::new("file:///linked.md", "linked");
        let mut result = GetPromptResult::new(vec![
            PromptMessage::new_resource_link(Role::User, link),
            PromptMessage::new_resource(
                Role::Assistant,
                "file:///embedded.md".to_string(),
                Some("text/plain".to_string()),
                Some("embedded".to_string()),
                None,
                None,
                None,
            ),
            PromptMessage::new_text(Role::User, "file:///plain-text.md"),
        ]);

        let pool = projection_pool().await;
        rewrite_get_prompt_result(&pool, "server-a", "docs", &mut result)
            .await
            .expect("rewrite prompt result");

        match &result.messages[0].content {
            ContentBlock::ResourceLink(link) => assert_eq!(
                link.uri,
                encode_resource_uri("docs", "file:///linked.md").expect("encode link")
            ),
            _ => panic!("expected resource link"),
        }
        match &result.messages[1].content {
            ContentBlock::Resource(resource) => match &resource.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    assert_eq!(
                        uri,
                        &encode_resource_uri("docs", "file:///embedded.md").expect("encode embedded")
                    );
                    assert_eq!(text, "embedded");
                }
                ResourceContents::BlobResourceContents { .. } => panic!("expected text resource"),
                _ => panic!("expected known resource contents"),
            },
            _ => panic!("expected embedded resource"),
        }
        match &result.messages[2].content {
            ContentBlock::Text(text) => assert_eq!(text.text, "file:///plain-text.md"),
            _ => panic!("expected text"),
        }
    }

    #[tokio::test]
    async fn registry_projection_reuses_listed_and_issues_unlisted_tool_resources() {
        use rmcp::model::{CallToolResult, ContentBlock, Resource, ResourceContents};

        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("connect registry database");
        crate::helpers::prepare_config_database(&pool).await;
        crate::config::server::init::initialize_server_tables(&pool)
            .await
            .expect("initialize server tables");
        crate::config::profile::init::initialize_profile_tables(&pool)
            .await
            .expect("initialize profile tables");
        sqlx::query("INSERT INTO server_config (id, name, server_type) VALUES ('server-a', 'everything', 'stdio')")
            .execute(&pool)
            .await
            .expect("insert server");
        crate::config::server::capabilities::upsert_shadow_resource(
            &pool,
            "server-a",
            "everything",
            "demo://resource/static/document/architecture.md",
            None,
            None,
            None,
        )
        .await
        .expect("insert listed resource");

        let mut result = CallToolResult::structured(serde_json::json!({
            "url": "demo://resource/structured.json"
        }));
        result.content = vec![
            ContentBlock::resource_link(Resource::new(
                "demo://resource/static/document/architecture.md",
                "listed",
            )),
            ContentBlock::resource(ResourceContents::text(
                "generated",
                "demo://resource/generated/report.md",
            )),
            ContentBlock::text("demo://resource/plain-text.md"),
        ];

        rewrite_call_tool_result(&pool, "server-a", "everything", &mut result)
            .await
            .expect("rewrite tool result through registry");

        assert_eq!(
            result.content[0].as_resource_link().expect("resource link").uri,
            "mcpmate://resources/everything/demo/static/document/architecture.md"
        );
        match &result.content[1].as_resource().expect("embedded resource").resource {
            ResourceContents::TextResourceContents { uri, .. } => {
                assert_eq!(uri, "mcpmate://resources/everything/demo/generated/report.md");
            }
            ResourceContents::BlobResourceContents { .. } => panic!("expected text resource"),
            _ => panic!("expected known resource contents"),
        }
        assert_eq!(
            result.content[2].as_text().expect("plain text").text,
            "demo://resource/plain-text.md"
        );
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({"url": "demo://resource/structured.json"}))
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_issued_resources")
                .fetch_one(&pool)
                .await
                .expect("count issued routes"),
            1
        );
    }

    #[tokio::test]
    async fn template_derived_resource_link_reuses_registered_canonical_template_route() {
        use rmcp::model::{CallToolResult, ContentBlock, Resource};

        let pool =
            template_projection_pool(&[("demo://resource/dynamic/text/{resourceId}", "Dynamic Text Resource")]).await;

        let mut result = CallToolResult::success(vec![ContentBlock::resource_link(Resource::new(
            "demo://resource/dynamic/text/2",
            "Dynamic text resource",
        ))]);

        rewrite_call_tool_result(&pool, "server-a", "everything", &mut result)
            .await
            .expect("rewrite template-derived resource link");

        assert_eq!(
            result.content[0].as_resource_link().expect("resource link").uri,
            "mcpmate://resources/template/everything/demo/dynamic/text/2"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_issued_resources")
                .fetch_one(&pool)
                .await
                .expect("count issued resource routes"),
            0
        );
    }

    #[tokio::test]
    async fn template_derived_resource_link_falls_back_to_issued_route_without_template_match() {
        use rmcp::model::{CallToolResult, ContentBlock, Resource};

        let pool =
            template_projection_pool(&[("demo://resource/dynamic/text/{resourceId}", "Dynamic Text Resource")]).await;
        let mut result = CallToolResult::success(vec![ContentBlock::resource_link(Resource::new(
            "demo://resource/generated/report.md",
            "Generated report",
        ))]);

        rewrite_call_tool_result(&pool, "server-a", "everything", &mut result)
            .await
            .expect("rewrite unmatched resource link");

        assert_eq!(
            result.content[0].as_resource_link().expect("resource link").uri,
            "mcpmate://resources/everything/demo/generated/report.md"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_issued_resources")
                .fetch_one(&pool)
                .await
                .expect("count issued resource routes"),
            1
        );
    }

    #[tokio::test]
    async fn template_derived_resource_link_rejects_ambiguous_template_matches_without_issuing() {
        use rmcp::model::{CallToolResult, ContentBlock, Resource};

        let pool = template_projection_pool(&[
            ("demo://resource/dynamic/text/{resourceId}", "Dynamic Text Resource"),
            ("demo://resource/dynamic/{kind}/{resourceId}", "Dynamic Resource"),
        ])
        .await;
        let mut result = CallToolResult::success(vec![ContentBlock::resource_link(Resource::new(
            "demo://resource/dynamic/text/2",
            "Dynamic text resource",
        ))]);

        let error = rewrite_call_tool_result(&pool, "server-a", "everything", &mut result)
            .await
            .expect_err("ambiguous template-derived resource link must fail");

        assert!(error.to_string().contains("ambiguous"));
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM server_issued_resources")
                .fetch_one(&pool)
                .await
                .expect("count issued resource routes"),
            0
        );
    }
}
