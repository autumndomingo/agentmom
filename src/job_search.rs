use std::{collections::HashSet, time::Duration};

use anyhow::{Context, Result};
use reqwest::Url;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobSearchRequest {
    #[serde(default)]
    pub(crate) sources: Vec<JobSource>,
    pub(crate) profile: SearchProfile,
    #[serde(default)]
    pub(crate) resume: Option<ResumeSummary>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct JobSource {
    pub(crate) label: String,
    pub(crate) url: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SearchProfile {
    #[serde(default)]
    pub(crate) opportunity_type: String,
    #[serde(default)]
    pub(crate) work_style: String,
    #[serde(default)]
    pub(crate) location: String,
    #[serde(default)]
    pub(crate) industries: String,
    #[serde(default)]
    pub(crate) roles: String,
    #[serde(default)]
    pub(crate) experience_level: String,
    #[serde(default)]
    pub(crate) education_status: String,
    #[serde(default)]
    pub(crate) personal_description: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ResumeSummary {
    #[serde(default)]
    pub(crate) name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobSearchResponse {
    pub(crate) matches: Vec<JobMatch>,
    pub(crate) source_notes: Vec<SourceNote>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobMatch {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) company: String,
    pub(crate) source: String,
    pub(crate) url: String,
    pub(crate) match_percent: u8,
    pub(crate) recency: String,
    pub(crate) location: String,
    pub(crate) snippet: String,
    pub(crate) reasons: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SourceNote {
    pub(crate) source: String,
    pub(crate) status: String,
    pub(crate) message: String,
}

#[derive(Debug, Clone)]
struct Candidate {
    title: String,
    source: String,
    url: String,
}

pub(crate) async fn search_jobs(request: JobSearchRequest) -> Result<JobSearchResponse> {
    let client = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; AgentMomJobSearch/0.1; +https://localhost)")
        .redirect(reqwest::redirect::Policy::limited(6))
        .connect_timeout(Duration::from_secs(6))
        .timeout(Duration::from_secs(16))
        .build()
        .context("build job search HTTP client")?;

    let query = SearchQuery::from_profile(&request.profile);
    let mut notes = Vec::new();
    let mut candidates = Vec::new();

    for source in request.sources.iter().take(8) {
        match search_source(&client, source, &query).await {
            Ok((mut found, message)) => {
                let count = found.len();
                candidates.append(&mut found);
                notes.push(SourceNote {
                    source: source.label.clone(),
                    status: if count > 0 { "searched" } else { "empty" }.to_string(),
                    message,
                });
            }
            Err(error) => notes.push(SourceNote {
                source: source.label.clone(),
                status: "blocked".to_string(),
                message: format!("Could not read listings from this source: {error:#}"),
            }),
        }
    }

    let mut seen = HashSet::new();
    let mut matches = Vec::new();
    for candidate in candidates {
        if !seen.insert(normalize_url_key(&candidate.url)) {
            continue;
        }
        if matches.len() >= 30 {
            break;
        }
        if let Ok(job_match) = inspect_candidate(&client, candidate, &query, &request).await {
            if job_match.match_percent >= 18 {
                matches.push(job_match);
            }
        }
    }

    matches.sort_by(|a, b| b.match_percent.cmp(&a.match_percent));
    matches.truncate(20);

    Ok(JobSearchResponse {
        matches,
        source_notes: notes,
    })
}

async fn search_source(
    client: &reqwest::Client,
    source: &JobSource,
    query: &SearchQuery,
) -> Result<(Vec<Candidate>, String)> {
    let search_urls = source_search_urls(source, query);
    let mut candidates = Vec::new();
    let mut last_error = None;

    for url in search_urls {
        match fetch_text(client, &url).await {
            Ok(body) => {
                let base = Url::parse(&url).ok();
                candidates.extend(extract_candidates(&body, base.as_ref(), &source.label));
                if !candidates.is_empty() {
                    candidates.truncate(12);
                    return Ok((
                        candidates,
                        "Found accessible listing links and filtered them for likely recent jobs."
                            .to_string(),
                    ));
                }
            }
            Err(error) => last_error = Some(error),
        }
    }

    if let Some(error) = last_error {
        return Err(error);
    }
    Ok((
        Vec::new(),
        "The source loaded, but no readable job listing links were found.".to_string(),
    ))
}

async fn inspect_candidate(
    client: &reqwest::Client,
    candidate: Candidate,
    query: &SearchQuery,
    request: &JobSearchRequest,
) -> Result<JobMatch> {
    let body = fetch_text(client, &candidate.url).await?;
    let text = html_to_text(&body);
    let title = pick_title(&body).unwrap_or(candidate.title);
    let company = pick_company(&body, &text).unwrap_or_else(|| "Company not detected".to_string());
    let recency = detect_recency(&text);
    if matches!(recency.as_deref(), Some("older than 2 weeks")) {
        anyhow::bail!("listing appears older than two weeks");
    }
    let location = detect_location(&text, &request.profile.location);
    let (match_percent, reasons) = score_match(&title, &company, &location, &text, query, request);

    Ok(JobMatch {
        id: stable_id(&candidate.url),
        title,
        company,
        source: candidate.source,
        url: candidate.url,
        match_percent,
        recency: recency.unwrap_or_else(|| "recent date not detected".to_string()),
        location,
        snippet: make_snippet(&text, query),
        reasons,
    })
}

fn source_search_urls(source: &JobSource, query: &SearchQuery) -> Vec<String> {
    let label = source.label.to_lowercase();
    let source_url = source.url.trim_end_matches('/');
    let q = encode_component(&query.primary_query);
    let location = encode_component(&query.location);

    if label.contains("indeed") || source_url.contains("indeed.") {
        return vec![format!(
            "https://www.indeed.com/jobs?q={q}&l={location}&fromage=14&sort=date"
        )];
    }
    if label.contains("linkedin") || source_url.contains("linkedin.") {
        return vec![format!(
            "https://www.linkedin.com/jobs/search/?keywords={q}&location={location}&f_TPR=r1209600&sortBy=DD"
        )];
    }
    if label.contains("wellfound") || source_url.contains("wellfound.") {
        return vec![format!("https://wellfound.com/jobs?keywords={q}&location={location}")];
    }
    if label.contains("greenhouse") || source_url.contains("boards.greenhouse.io") {
        if source_url.contains("boards.greenhouse.io/") && source_url != "https://boards.greenhouse.io" {
            return vec![source_url.to_string()];
        }
        return vec![format!(
            "https://www.google.com/search?q={}%20site%3Aboards.greenhouse.io",
            encode_component(&format!("{} {}", query.primary_query, query.location))
        )];
    }
    if label.contains("google") {
        return vec![format!(
            "https://www.google.com/search?q={}%20jobs%20posted%20last%202%20weeks",
            encode_component(&format!("{} {}", query.primary_query, query.location))
        )];
    }
    if label.contains("handshake") || source_url.contains("handshake") {
        return vec![format!(
            "https://www.google.com/search?q={}%20site%3Ajoinhandshake.com",
            encode_component(&format!("{} {}", query.primary_query, query.location))
        )];
    }

    vec![source.url.clone()]
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = client.get(url).send().await?.error_for_status()?;
    let text = response.text().await?;
    if text.len() < 300 {
        anyhow::bail!("response was too small to contain listings");
    }
    Ok(text)
}

fn extract_candidates(body: &str, base: Option<&Url>, source: &str) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();
    let mut rest = body;

    while let Some(index) = rest.find("href=") {
        rest = &rest[index + 5..];
        let Some(quote) = rest.chars().next() else {
            break;
        };
        if quote != '"' && quote != '\'' {
            continue;
        }
        rest = &rest[quote.len_utf8()..];
        let Some(end) = rest.find(quote) else {
            break;
        };
        let raw_href = &rest[..end];
        let after = &rest[end + quote.len_utf8()..];
        rest = after;

        let Some(url) = resolve_href(raw_href, base) else {
            continue;
        };
        if !is_likely_job_url(&url) || !seen.insert(normalize_url_key(&url)) {
            continue;
        }
        let title = extract_anchor_text(after).unwrap_or_else(|| "Job listing".to_string());
        candidates.push(Candidate {
            title,
            source: source.to_string(),
            url,
        });
        if candidates.len() >= 20 {
            break;
        }
    }

    candidates
}

fn resolve_href(raw_href: &str, base: Option<&Url>) -> Option<String> {
    let href = decode_basic_entities(raw_href).trim().to_string();
    if href.starts_with("javascript:") || href.starts_with('#') || href.starts_with("mailto:") {
        return None;
    }
    if href.starts_with("/url?q=") {
        let target = href
            .trim_start_matches("/url?q=")
            .split('&')
            .next()
            .map(percent_decode)?;
        return Url::parse(&target).ok().map(|url| url.to_string());
    }
    if let Ok(url) = Url::parse(&href) {
        return Some(url.to_string());
    }
    base.and_then(|base| base.join(&href).ok()).map(|url| url.to_string())
}

fn is_likely_job_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    if lower.contains("google.com/search") {
        return false;
    }
    if lower.contains("linkedin.com") {
        return lower.contains("/jobs/view/");
    }
    if lower.contains("indeed.") {
        return lower.contains("viewjob") || lower.contains("jk=");
    }
    [
        "/job", "/jobs", "viewjob", "jk=", "gh_jid", "jobid", "position", "opening", "career",
        "boards.greenhouse.io", "lever.co",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn extract_anchor_text(after_href: &str) -> Option<String> {
    let end = after_href.find("</a>")?;
    let tag_end = after_href.find('>')?;
    if tag_end >= end {
        return None;
    }
    let text = html_to_text(&after_href[tag_end + 1..end]);
    let cleaned = compact_whitespace(&text);
    if cleaned.len() >= 3 && cleaned.len() <= 140 {
        Some(cleaned)
    } else {
        None
    }
}

fn pick_title(body: &str) -> Option<String> {
    extract_tag_text(body, "h1")
        .or_else(|| extract_title_tag(body))
        .map(|title| title.replace(" | LinkedIn", "").replace(" - Indeed", ""))
        .map(|title| compact_whitespace(&title))
        .filter(|title| title.len() >= 3)
}

fn pick_company(body: &str, text: &str) -> Option<String> {
    if let Some(company) = extract_json_string_after(body, r#""hiringOrganization""#, r#""name":""#)
        .or_else(|| extract_json_string_after(body, r#""companyName""#, r#""companyName":""#))
    {
        return Some(company);
    }
    for marker in [
        r#""hiringOrganization""#,
        r#""companyName""#,
        r#"data-company-name=""#,
        "Company:",
    ] {
        if let Some(index) = body.find(marker) {
            let chunk = &body[index..body.len().min(index + 500)];
            let cleaned = compact_whitespace(&html_to_text(chunk));
            if cleaned.len() > 8 {
                return Some(cleaned.chars().take(80).collect());
            }
        }
    }
    text.lines()
        .map(compact_whitespace)
        .find(|line| line.len() >= 3 && line.len() <= 70 && !line.to_lowercase().contains("job"))
}

fn detect_recency(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    if lower.contains("today") || lower.contains("just posted") || lower.contains("new") {
        return Some("posted today".to_string());
    }
    if lower.contains("yesterday") {
        return Some("posted yesterday".to_string());
    }
    for days in 1..=31 {
        let singular = format!("{days} day ago");
        let plural = format!("{days} days ago");
        if lower.contains(&singular) || lower.contains(&plural) {
            return Some(if days <= 14 {
                format!("posted {days} days ago")
            } else {
                "older than 2 weeks".to_string()
            });
        }
    }
    Some("recent date not detected".to_string())
}

fn detect_location(text: &str, requested: &str) -> String {
    let lower_text = text.to_lowercase();
    let requested = requested.trim();
    if !requested.is_empty() && lower_text.contains(&requested.to_lowercase()) {
        return requested.to_string();
    }
    for marker in ["remote", "hybrid", "united states", "new york", "san francisco", "los angeles"] {
        if lower_text.contains(marker) {
            return capitalize_words(marker);
        }
    }
    "Location not detected".to_string()
}

fn score_match(
    title: &str,
    company: &str,
    location: &str,
    text: &str,
    query: &SearchQuery,
    request: &JobSearchRequest,
) -> (u8, Vec<String>) {
    let title_lower = title.to_lowercase();
    let text_lower = text.to_lowercase();
    let location_lower = location.to_lowercase();
    let mut score = 0u32;
    let mut reasons = Vec::new();

    let role_hits = query
        .role_terms
        .iter()
        .filter(|term| title_lower.contains(term.as_str()) || text_lower.contains(term.as_str()))
        .count();
    if role_hits > 0 {
        score += (role_hits as u32 * 10).min(35);
        reasons.push(format!("Matches {role_hits} role/title keyword{}", plural(role_hits)));
    }

    let field_hits = query
        .field_terms
        .iter()
        .filter(|term| text_lower.contains(term.as_str()) || title_lower.contains(term.as_str()))
        .count();
    if field_hits > 0 {
        score += (field_hits as u32 * 7).min(22);
        reasons.push(format!("Mentions {field_hits} preferred field keyword{}", plural(field_hits)));
    }

    let personal_hits = query
        .personal_terms
        .iter()
        .filter(|term| text_lower.contains(term.as_str()))
        .count();
    if personal_hits > 0 {
        score += (personal_hits as u32 * 4).min(18);
        reasons.push("Job description overlaps with the tailored description".to_string());
    }

    if !query.location.is_empty()
        && (location_lower.contains(&query.location.to_lowercase())
            || text_lower.contains(&query.location.to_lowercase()))
    {
        score += 12;
        reasons.push("Fits the requested location".to_string());
    }

    let work_style = request.profile.work_style.to_lowercase();
    if work_style.contains("remote") && text_lower.contains("remote")
        || work_style.contains("hybrid") && text_lower.contains("hybrid")
        || work_style.contains("in-person") && !text_lower.contains("remote")
        || work_style.contains("flexible")
    {
        score += 8;
        reasons.push("Work style looks compatible".to_string());
    }

    let experience = request.profile.experience_level.to_lowercase();
    if !experience.is_empty() && text_lower.contains(&experience.replace(" / student", "")) {
        score += 7;
        reasons.push("Experience level appears aligned".to_string());
    }

    if !request.profile.education_status.trim().is_empty()
        && contains_any_token(&text_lower, &tokenize(&request.profile.education_status))
    {
        score += 5;
        reasons.push("Education/student context appears relevant".to_string());
    }

    if request.resume.as_ref().is_some_and(|resume| !resume.name.is_empty()) {
        score += 3;
    }

    if reasons.is_empty() {
        reasons.push(format!(
            "Limited keyword overlap detected; review before applying to {}",
            company
        ));
    }

    (score.min(100) as u8, reasons)
}

#[derive(Debug)]
struct SearchQuery {
    primary_query: String,
    location: String,
    role_terms: Vec<String>,
    field_terms: Vec<String>,
    personal_terms: Vec<String>,
}

impl SearchQuery {
    fn from_profile(profile: &SearchProfile) -> Self {
        let role_terms = tokenize(&profile.roles);
        let field_terms = tokenize(&profile.industries);
        let personal_terms = tokenize(&profile.personal_description);
        let primary_query = compact_whitespace(&format!(
            "{} {} {}",
            profile.roles, profile.industries, profile.opportunity_type
        ));
        Self {
            primary_query: if primary_query.is_empty() {
                "jobs".to_string()
            } else {
                primary_query
            },
            location: profile.location.clone(),
            role_terms,
            field_terms,
            personal_terms,
        }
    }
}

fn tokenize(value: &str) -> Vec<String> {
    let stop_words = [
        "and", "the", "for", "with", "that", "this", "from", "into", "want", "learn", "how",
        "about", "your", "you", "are", "job", "jobs", "role", "work",
    ];
    value
        .split(|ch: char| !ch.is_alphanumeric())
        .map(|word| word.trim().to_lowercase())
        .filter(|word| word.len() >= 3 && !stop_words.contains(&word.as_str()))
        .collect::<HashSet<_>>()
        .into_iter()
        .take(25)
        .collect()
}

fn contains_any_token(text: &str, tokens: &[String]) -> bool {
    tokens.iter().any(|token| text.contains(token))
}

fn make_snippet(text: &str, query: &SearchQuery) -> String {
    let lower = text.to_lowercase();
    let first_hit = query
        .role_terms
        .iter()
        .chain(query.field_terms.iter())
        .filter_map(|term| lower.find(term))
        .min()
        .unwrap_or(0);
    let start = first_hit.saturating_sub(120);
    let snippet: String = text.chars().skip(start).take(360).collect();
    compact_whitespace(&snippet)
}

fn html_to_text(value: &str) -> String {
    let value = remove_tag_blocks(value, &["script", "style", "svg", "noscript"]);
    let mut output = String::with_capacity(value.len().min(4096));
    let mut in_tag = false;
    for ch in value.chars() {
        match ch {
            '<' => {
                in_tag = true;
                output.push(' ');
            }
            '>' => {
                in_tag = false;
                output.push(' ');
            }
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    compact_whitespace(&decode_basic_entities(&output))
}

fn remove_tag_blocks(value: &str, tags: &[&str]) -> String {
    let mut output = value.to_string();
    for tag in tags {
        loop {
            let lower = output.to_lowercase();
            let Some(start) = lower.find(&format!("<{tag}")) else {
                break;
            };
            let Some(relative_end) = lower[start..].find(&format!("</{tag}>")) else {
                break;
            };
            let end = start + relative_end + tag.len() + 3;
            output.replace_range(start..end, " ");
        }
    }
    output
}

fn extract_tag_text(body: &str, tag: &str) -> Option<String> {
    let lower = body.to_lowercase();
    let start = lower.find(&format!("<{tag}"))?;
    let content_start = body[start..].find('>')? + start + 1;
    let end = lower[content_start..].find(&format!("</{tag}>"))? + content_start;
    Some(html_to_text(&body[content_start..end]))
}

fn extract_title_tag(body: &str) -> Option<String> {
    extract_tag_text(body, "title")
}

fn compact_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_basic_entities(value: &str) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("\\u0026", "&")
        .replace("\\/", "/")
}

fn extract_json_string_after(body: &str, marker: &str, key: &str) -> Option<String> {
    let marker_index = body.find(marker)?;
    let chunk = &body[marker_index..body.len().min(marker_index + 800)];
    let key_index = chunk.find(key)?;
    let after_key = &chunk[key_index + key.len()..];
    let after_quote = after_key.strip_prefix('"').unwrap_or(after_key);
    let end = after_quote.find('"')?;
    let value = decode_basic_entities(&after_quote[..end]);
    let cleaned = compact_whitespace(&html_to_text(&value));
    if cleaned.len() >= 2 && cleaned.len() <= 100 {
        Some(cleaned)
    } else {
        None
    }
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                if let Ok(hex) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                    output.push(hex);
                    index += 3;
                } else {
                    output.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).to_string()
}

fn encode_component(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            b' ' => vec!['+'],
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn normalize_url_key(url: &str) -> String {
    url.split('#').next().unwrap_or(url).trim_end_matches('/').to_lowercase()
}

fn stable_id(url: &str) -> String {
    format!("{:x}", md5ish(url.as_bytes()))
}

fn md5ish(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn capitalize_words(value: &str) -> String {
    value
        .split_whitespace()
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}
