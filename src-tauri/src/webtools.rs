//! Built-in, key-free web tools (Hermes-style names): `web_search`,
//! `web_extract`, `web_research`, `wikipedia`, `fetch_json`.
//!
//! Search order: Brave / Tavily / Serper (only when a key is configured) →
//! SearXNG (if configured) → DuckDuckGo HTML → Wikipedia. HTML is converted to
//! Markdown with `htmd` (Apache-2.0); when a direct fetch fails the extract
//! tools fall back to `https://r.jina.ai/<url>`.
//!
//! Results (and extracts) are cached in-process for 10 minutes. Every network
//! call has a short timeout so a slow site cannot stall an agent turn.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde_json::Value;

use crate::events::ToolResult;
use crate::tools::Tool;

const CACHE_TTL: Duration = Duration::from_secs(600);
const MAX_RESULT_CHARS: usize = 6000;
const EXTRACT_MAX_CHARS: usize = 12_000;
const EXTRACT_MAX_BYTES: usize = 2 * 1024 * 1024;

/// Where the search keys / SearXNG endpoint come from. Built once per agent
/// from the config + Keychain, then cloned into each web tool.
#[derive(Clone, Default)]
pub struct WebConfig {
    pub enabled: bool,
    pub searxng_url: String,
    pub brave_key: Option<String>,
    pub tavily_key: Option<String>,
    pub serper_key: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

fn cache() -> &'static Mutex<HashMap<String, (Instant, String)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, String)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_get(key: &str) -> Option<String> {
    let guard = cache().lock().unwrap();
    match guard.get(key) {
        Some((at, v)) if at.elapsed() < CACHE_TTL => Some(v.clone()),
        Some(_) => None,
        None => None,
    }
}

fn cache_put(key: String, value: String) {
    let mut guard = cache().lock().unwrap();
    guard.insert(key, (Instant::now(), value));
    // Bound memory: drop expired entries when the map grows large.
    if guard.len() > 256 {
        guard.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
    }
}

fn client(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(timeout)
        .connect_timeout(Duration::from_secs(4))
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Xenon/0.3")
        .build()
        .map_err(|e| anyhow!("http client: {e}"))
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// Percent-decode a URL component (DuckDuckGo wraps result URLs in `uddg=`).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (hi, lo) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---- search --------------------------------------------------------------

async fn brave_search(cfg: &WebConfig, query: &str) -> Result<Vec<SearchHit>> {
    let key = cfg.brave_key.as_deref().ok_or_else(|| anyhow!("no key"))?;
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}",
        urlencode(query)
    );
    let resp = client(Duration::from_secs(8))?
        .get(&url)
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| anyhow!("brave: {e}"))?;
    let json: Value = resp.json().await.map_err(|e| anyhow!("brave json: {e}"))?;
    let mut out = Vec::new();
    if let Some(results) = json["web"]["results"].as_array() {
        for r in results {
            out.push(SearchHit {
                title: json_str(r, &["title"]),
                url: json_str(r, &["url"]),
                snippet: json_str(r, &["description"]),
            });
        }
    }
    Ok(out)
}

async fn tavily_search(cfg: &WebConfig, query: &str) -> Result<Vec<SearchHit>> {
    let key = cfg.tavily_key.as_deref().ok_or_else(|| anyhow!("no key"))?;
    let resp = client(Duration::from_secs(8))?
        .post("https://api.tavily.com/search")
        .json(&serde_json::json!({ "api_key": key, "query": query, "max_results": 5 }))
        .send()
        .await
        .map_err(|e| anyhow!("tavily: {e}"))?;
    let json: Value = resp.json().await.map_err(|e| anyhow!("tavily json: {e}"))?;
    let mut out = Vec::new();
    for r in json["results"].as_array().cloned().unwrap_or_default() {
        out.push(SearchHit {
            title: json_str(&r, &["title"]),
            url: json_str(&r, &["url"]),
            snippet: json_str(&r, &["content"]),
        });
    }
    Ok(out)
}

async fn serper_search(cfg: &WebConfig, query: &str) -> Result<Vec<SearchHit>> {
    let key = cfg.serper_key.as_deref().ok_or_else(|| anyhow!("no key"))?;
    let resp = client(Duration::from_secs(8))?
        .post("https://google.serper.dev/search")
        .header("X-API-KEY", key)
        .json(&serde_json::json!({ "q": query }))
        .send()
        .await
        .map_err(|e| anyhow!("serper: {e}"))?;
    let json: Value = resp.json().await.map_err(|e| anyhow!("serper json: {e}"))?;
    let mut out = Vec::new();
    for r in json["organic"].as_array().cloned().unwrap_or_default() {
        out.push(SearchHit {
            title: json_str(&r, &["title"]),
            url: json_str(&r, &["link"]),
            snippet: json_str(&r, &["snippet"]),
        });
    }
    Ok(out)
}

async fn searxng_search(cfg: &WebConfig, query: &str) -> Result<Vec<SearchHit>> {
    let base = cfg.searxng_url.trim();
    if base.is_empty() {
        return Err(anyhow!("no searxng url"));
    }
    let url = format!(
        "{}/search?q={}&format=json",
        base.trim_end_matches('/'),
        urlencode(query)
    );
    let resp = client(Duration::from_secs(8))?
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("searxng: {e}"))?;
    let json: Value = resp.json().await.map_err(|e| anyhow!("searxng json: {e}"))?;
    let mut out = Vec::new();
    for r in json["results"].as_array().cloned().unwrap_or_default() {
        out.push(SearchHit {
            title: json_str(&r, &["title"]),
            url: json_str(&r, &["url"]),
            snippet: json_str(&r, &["content"]),
        });
    }
    Ok(out)
}

async fn duckduckgo_search(query: &str) -> Result<Vec<SearchHit>> {
    let url = format!("https://html.duckduckgo.com/html/?q={}", urlencode(query));
    let resp = client(Duration::from_secs(8))?
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("ddg: {e}"))?;
    let html = resp
        .text()
        .await
        .map_err(|e| anyhow!("ddg body: {e}"))?;
    Ok(parse_ddg_html(&html))
}

/// Extract result links + titles from a DuckDuckGo HTML page. Kept small and
/// dependency-free: find each `result__a` anchor, pull its href and text.
fn parse_ddg_html(html: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find("result__a") {
        let after = &rest[pos..];
        // href="…" of this result anchor.
        let Some(href_begin_rel) = after.find("href=\"").map(|i| i + 6) else {
            break;
        };
        let Some(href_end_rel) = after[href_begin_rel..].find('"') else {
            break;
        };
        let href_end = href_begin_rel + href_end_rel;
        let raw_href = &after[href_begin_rel..href_end];
        let url = raw_href
            .split("uddg=")
            .nth(1)
            .map(|u| percent_decode(u.split('&').next().unwrap_or(u)))
            .unwrap_or_else(|| raw_href.to_string());
        // Anchor text sits between the end of the opening tag and </a>.
        let tail = &after[href_end..];
        let text_begin = tail.find('>').map(|i| i + 1).unwrap_or(0);
        let text_end = tail.find("</a>").unwrap_or(tail.len());
        let title = strip_tags(&tail[text_begin..text_end]);

        // Advance past this anchor to find the next one.
        rest = &rest[pos + 1..];
        if !url.is_empty() {
            hits.push(SearchHit {
                title,
                url,
                snippet: String::new(),
            });
        }
    }
    hits
}

fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth: i32 = 0;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn json_str(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        if let Some(s) = v.get(*k).and_then(|x| x.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Search the web, trying the configured providers in order. `Ok(vec![])` means
/// every backend returned nothing (not an error).
pub async fn search(cfg: &WebConfig, query: &str) -> Result<Vec<SearchHit>> {
    let q = query.trim();
    if q.is_empty() {
        return Err(anyhow!("empty query"));
    }
    let key = format!("search:{q}");
    if let Some(hit) = cache_get(&key) {
        if let Ok(v) = serde_json::from_str::<Vec<SearchHit>>(&hit) {
            return Ok(v);
        }
    }

    let mut tried = 0;
    macro_rules! try_backend {
        ($e:expr) => {{
            tried += 1;
            match $e.await {
                Ok(hits) if !hits.is_empty() => {
                    let hits = hits.into_iter().take(8).collect::<Vec<_>>();
                    cache_put(key, serde_json::to_string(&hits).unwrap_or_default());
                    return Ok(hits);
                }
                Ok(_) => {}
                Err(e) => log::debug!("search backend {tried} failed: {e}"),
            }
        }};
    }

    if cfg.brave_key.is_some() {
        try_backend!(brave_search(cfg, q));
    }
    if cfg.tavily_key.is_some() {
        try_backend!(tavily_search(cfg, q));
    }
    if cfg.serper_key.is_some() {
        try_backend!(serper_search(cfg, q));
    }
    if !cfg.searxng_url.trim().is_empty() {
        try_backend!(searxng_search(cfg, q));
    }
    try_backend!(duckduckgo_search(q));

    Ok(Vec::new())
}

// ---- extract -------------------------------------------------------------

fn html_to_markdown(html: &str) -> String {
    match htmd::convert(html) {
        Ok(md) => md,
        Err(_) => strip_tags(html),
    }
}

/// Fetch one URL and return Markdown (via htmd), falling back to r.jina.ai.
async fn extract_one(url: &str) -> Result<String> {
    let cache_key = format!("extract:{url}");
    if let Some(hit) = cache_get(&cache_key) {
        return Ok(hit);
    }

    let direct = async {
        let resp = client(Duration::from_secs(10))?
            .get(url)
            .send()
            .await
            .map_err(|e| anyhow!("fetch: {e}"))?;
        let status = resp.status();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = resp.bytes().await.map_err(|e| anyhow!("body: {e}"))?;
        if bytes.len() > EXTRACT_MAX_BYTES {
            return Err(anyhow!("body too large ({} bytes)", bytes.len()));
        }
        if !status.is_success() {
            return Err(anyhow!("HTTP {status}"));
        }
        let text = String::from_utf8_lossy(&bytes).to_string();
        let md = if ctype.contains("html") || text.trim_start().starts_with('<') {
            html_to_markdown(&text)
        } else {
            text
        };
        let md = md.trim().to_string();
        if md.len() < 40 {
            return Err(anyhow!("content too short"));
        }
        Ok(md)
    };

    let md = match direct.await {
        Ok(md) => md,
        Err(e) => {
            log::debug!("direct extract failed ({e}); trying r.jina.ai");
            let jina = format!("https://r.jina.ai/{url}");
            let resp = client(Duration::from_secs(12))?
                .get(&jina)
                .send()
                .await
                .map_err(|e| anyhow!("jina: {e}"))?;
            let text = resp.text().await.map_err(|e| anyhow!("jina body: {e}"))?;
            let text = text.trim().to_string();
            if text.len() < 40 {
                return Err(anyhow!("extract produced no usable text"));
            }
            text
        }
    };
    let md = truncate_chars(&md, EXTRACT_MAX_CHARS);
    cache_put(cache_key, md.clone());
    Ok(md)
}

/// Extract several URLs concurrently (bounded). Never more than 5 at a time.
pub async fn extract(urls: &[String]) -> Vec<(String, Result<String>)> {
    let urls: Vec<String> = urls.iter().take(5).cloned().collect();
    let futs = urls.into_iter().map(|u| async move {
        let r = extract_one(&u).await;
        (u, r)
    });
    futures::future::join_all(futs).await
}

// ---- tools ---------------------------------------------------------------

fn tool_result(result: Value) -> ToolResult {
    ToolResult {
        call_id: String::new(),
        result,
        is_error: false,
        error_message: None,
        image: None,
    }
}

fn require(cfg: &WebConfig) -> Result<()> {
    if cfg.enabled {
        Ok(())
    } else {
        Err(anyhow!("web tools are disabled in Settings"))
    }
}

pub struct WebSearchTool {
    cfg: WebConfig,
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web and return ranked results (title, url, snippet). Use for \
         facts, news and anything current. Prefer web_research for a synthesised answer."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query." }
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, args: Value) -> Result<ToolResult> {
        require(&self.cfg)?;
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'query'"))?;
        let hits = search(&self.cfg, query).await?;
        let json = serde_json::to_string(&hits)?;
        Ok(tool_result(serde_json::json!({
            "query": query,
            "results": serde_json::from_str::<Value>(&json).unwrap_or(json.into()),
            "summary": format!("{} result(s).", hits.len()),
        })))
    }
}

pub struct WebExtractTool {
    cfg: WebConfig,
}

#[async_trait]
impl Tool for WebExtractTool {
    fn name(&self) -> &str {
        "web_extract"
    }
    fn description(&self) -> &str {
        "Fetch one or more URLs and return each page as Markdown text (max 5). \
         Use to read an article, docs page or search result."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "urls": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Up to 5 http(s) URLs to extract."
                }
            },
            "required": ["urls"]
        })
    }
    async fn execute(&self, args: Value) -> Result<ToolResult> {
        require(&self.cfg)?;
        let urls: Vec<String> = args["urls"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if urls.is_empty() {
            return Err(anyhow!("missing 'urls'"));
        }
        let results = extract(&urls).await;
        let mut pages = Vec::new();
        for (url, r) in results {
            match r {
                Ok(md) => pages.push(serde_json::json!({ "url": url, "markdown": md })),
                Err(e) => pages.push(serde_json::json!({ "url": url, "error": e.to_string() })),
            }
        }
        Ok(tool_result(serde_json::json!({
            "pages": pages,
            "summary": format!("Extracted {} page(s).", pages.len()),
        })))
    }
}

pub struct WebResearchTool {
    cfg: WebConfig,
}

#[async_trait]
impl Tool for WebResearchTool {
    fn name(&self) -> &str {
        "web_research"
    }
    fn description(&self) -> &str {
        "Search the web, read the top 3-5 results and return a compact summary with \
         source URLs. The best tool for factual or current questions. Always cite sources."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Question to research." }
            },
            "required": ["query"]
        })
    }
    async fn execute(&self, args: Value) -> Result<ToolResult> {
        require(&self.cfg)?;
        let query = args["query"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'query'"))?;
        let hits = search(&self.cfg, query).await?;
        if hits.is_empty() {
            return Ok(tool_result(serde_json::json!({
                "query": query,
                "findings": "No results.",
                "sources": [],
                "summary": "No results.",
            })));
        }
        let urls: Vec<String> = hits.iter().take(5).map(|h| h.url.clone()).collect();
        let extracted = extract(&urls).await;
        let mut findings = String::new();
        let mut sources = Vec::new();
        for (hit, (_, r)) in hits.iter().zip(extracted) {
            sources.push(serde_json::json!({ "title": hit.title, "url": hit.url }));
            if let Ok(md) = r {
                findings.push_str(&format!("## {}\n{}\n\n", hit.title, truncate_chars(&md, 2000)));
            } else if !hit.snippet.is_empty() {
                findings.push_str(&format!("## {}\n{}\n\n", hit.title, hit.snippet));
            }
        }
        let findings = truncate_chars(&findings, EXTRACT_MAX_CHARS);
        Ok(tool_result(serde_json::json!({
            "query": query,
            "findings": findings,
            "sources": sources,
            "summary": format!("Researched {} source(s).", sources.len()),
        })))
    }
}

pub struct WikipediaTool {
    cfg: WebConfig,
}

#[async_trait]
impl Tool for WikipediaTool {
    fn name(&self) -> &str {
        "wikipedia"
    }
    fn description(&self) -> &str {
        "Look up a topic on Wikipedia and return the summary. Good for definitions \
         and background."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "Article title or topic." }
            },
            "required": ["title"]
        })
    }
    async fn execute(&self, args: Value) -> Result<ToolResult> {
        require(&self.cfg)?;
        let title = args["title"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'title'"))?;
        let key = format!("wiki:{title}");
        let cached = cache_get(&key);
        let summary = match cached {
            Some(s) => s,
            None => {
                let url = format!(
                    "https://en.wikipedia.org/api/rest_v1/page/summary/{}",
                    urlencode(&title.replace(' ', "_"))
                );
                let resp = client(Duration::from_secs(8))?
                    .get(&url)
                    .send()
                    .await
                    .map_err(|e| anyhow!("wikipedia: {e}"))?;
                let json: Value = resp.json().await.map_err(|e| anyhow!("wikipedia json: {e}"))?;
                let extract = json_str(&json, &["extract"]);
                if extract.trim().is_empty() {
                    return Err(anyhow!("no Wikipedia article for {title:?}"));
                }
                let page_url = json_str(&json, &["content_urls", "desktop", "page"]);
                let summary = if page_url.is_empty() {
                    extract
                } else {
                    format!("{extract}\n\nSource: {page_url}")
                };
                cache_put(key, summary.clone());
                summary
            }
        };
        Ok(tool_result(serde_json::json!({
            "title": title,
            "summary": truncate_chars(&summary, MAX_RESULT_CHARS),
        })))
    }
}

pub struct FetchJsonTool {
    cfg: WebConfig,
}

#[async_trait]
impl Tool for FetchJsonTool {
    fn name(&self) -> &str {
        "fetch_json"
    }
    fn description(&self) -> &str {
        "Fetch a URL that returns JSON (REST API, raw data) and return the parsed \
         JSON. Use for structured data."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "http(s) URL returning JSON." }
            },
            "required": ["url"]
        })
    }
    async fn execute(&self, args: Value) -> Result<ToolResult> {
        require(&self.cfg)?;
        let url = args["url"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'url'"))?;
        let key = format!("json:{url}");
        let body = match cache_get(&key) {
            Some(b) => b,
            None => {
                let resp = client(Duration::from_secs(8))?
                    .get(url)
                    .send()
                    .await
                    .map_err(|e| anyhow!("fetch_json: {e}"))?;
                let text = resp.text().await.map_err(|e| anyhow!("fetch_json body: {e}"))?;
                let val: Value = serde_json::from_str(&text)
                    .map_err(|e| anyhow!("response is not JSON: {e}"))?;
                let body = serde_json::to_string(&val)?;
                cache_put(key, body.clone());
                body
            }
        };
        // Validate it parses (cached bodies are already validated).
        serde_json::from_str::<Value>(&body)?;
        Ok(tool_result(serde_json::json!({
            "url": url,
            "json": truncate_chars(&body, MAX_RESULT_CHARS),
            "summary": "Fetched JSON.",
        })))
    }
}

/// The five built-in web tools, in registration order.
pub fn create_web_tools(cfg: WebConfig) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(WebResearchTool { cfg: cfg.clone() }),
        Box::new(WebSearchTool { cfg: cfg.clone() }),
        Box::new(WebExtractTool { cfg: cfg.clone() }),
        Box::new(WikipediaTool { cfg: cfg.clone() }),
        Box::new(FetchJsonTool { cfg }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_percent_decode() {
        assert_eq!(percent_decode("a%20b+c"), "a b c");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn test_parse_ddg_html_extracts_results() {
        let html = r#"
            <div class="result">
              <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&amp;rut=abc">Rust Language</a>
            </div>
            <div class="result">
              <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com">Example</a>
            </div>"#;
        let hits = parse_ddg_html(html);
        assert_eq!(hits.len(), 2, "got {hits:?}");
        assert_eq!(hits[0].url, "https://rust-lang.org/");
        assert_eq!(hits[0].title, "Rust Language");
        assert_eq!(hits[1].url, "https://example.com");
    }

    #[test]
    fn test_strip_tags() {
        assert_eq!(strip_tags("<b>hi</b> <i>there</i>"), "hi there");
    }

    #[test]
    fn test_truncate_chars() {
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
    }

    #[tokio::test]
    async fn test_disabled_web_tools_error() {
        let tool = WebSearchTool {
            cfg: WebConfig::default(),
        };
        assert!(tool.execute(serde_json::json!({ "query": "x" })).await.is_err());
    }

    /// One live search (skipped unless XENON_LIVE_WEB=1). Makes at most a
    /// couple of requests so it is safe to run manually.
    #[tokio::test]
    async fn live_web_search() {
        if std::env::var("XENON_LIVE_WEB").as_deref() != Ok("1") {
            eprintln!("skip: set XENON_LIVE_WEB=1 to run");
            return;
        }
        let cfg = WebConfig {
            enabled: true,
            ..Default::default()
        };
        let started = Instant::now();
        let hits = search(&cfg, "latest stable Rust version").await.unwrap();
        let ms = started.elapsed().as_millis();
        eprintln!("live web_search => {} hits in {ms} ms", hits.len());
        for h in hits.iter().take(3) {
            eprintln!("  {} — {}", h.title, h.url);
        }
        assert!(!hits.is_empty(), "live search returned nothing");
    }

    /// Live extract via r.jina.ai fallback path (skipped unless XENON_LIVE_WEB=1).
    #[tokio::test]
    async fn live_web_extract() {
        if std::env::var("XENON_LIVE_WEB").as_deref() != Ok("1") {
            eprintln!("skip: set XENON_LIVE_WEB=1 to run");
            return;
        }
        let started = Instant::now();
        let md = extract_one("https://example.com").await.unwrap();
        eprintln!("live web_extract => {} chars in {} ms", md.len(), started.elapsed().as_millis());
        assert!(md.len() > 20);
    }
}