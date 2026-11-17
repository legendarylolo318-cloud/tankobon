//! MangaDex source (official public API, https://api.mangadex.org).
//!
//! Rate limits: the API allows roughly 5 requests/second per client; we space
//! API calls ~250ms apart. `/at-home/server` has its own, much lower limit
//! (40 requests/minute), so those calls are additionally spaced 1.6s apart.
//! On HTTP 429 we wait for `X-RateLimit-Retry-After` / `Retry-After`.

use super::{http_client, ChapterEntry, MangaDetails, MangaEntry, Paged, Source};
use crate::models::Status;
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::RwLock;
use std::time::Duration;
use tokio::time::Instant;

const API: &str = "https://api.mangadex.org";
const COVERS: &str = "https://uploads.mangadex.org/covers";
pub const PAGE_SIZE: u32 = 24;
const FEED_LIMIT: u32 = 500;
/// 40 requests per minute on `/at-home/server`, with a little headroom.
const AT_HOME_INTERVAL: Duration = Duration::from_millis(1600);

struct RateLimiter {
    next: tokio::sync::Mutex<Instant>,
    interval: Duration,
}

impl RateLimiter {
    fn new(interval: Duration) -> Self {
        Self { next: tokio::sync::Mutex::new(Instant::now()), interval }
    }
    async fn wait(&self) {
        let mut next = self.next.lock().await;
        let now = Instant::now();
        if *next > now {
            tokio::time::sleep(*next - now).await;
        }
        *next = Instant::now().max(*next) + self.interval;
    }
}

pub struct MangaDex {
    lang: RwLock<String>,
    nsfw: RwLock<bool>,
    limiter: RateLimiter,
    at_home_limiter: RateLimiter,
}

impl Default for MangaDex {
    fn default() -> Self {
        Self::new("en", false)
    }
}

impl MangaDex {
    pub fn new(lang: &str, nsfw: bool) -> Self {
        Self {
            lang: RwLock::new(lang.to_string()),
            nsfw: RwLock::new(nsfw),
            limiter: RateLimiter::new(Duration::from_millis(250)),
            at_home_limiter: RateLimiter::new(AT_HOME_INTERVAL),
        }
    }

    fn lang(&self) -> String {
        self.lang.read().map(|l| l.clone()).unwrap_or_else(|_| "en".into())
    }

    fn ratings(&self) -> Vec<&'static str> {
        let mut v = vec!["safe", "suggestive"];
        if self.nsfw.read().map(|b| *b).unwrap_or(false) {
            v.extend(["erotica", "pornographic"]);
        }
        v
    }

    async fn get_text(&self, path: &str, query: &[(String, String)]) -> Result<String> {
        let url = format!("{API}{path}");
        let mut last_err = anyhow!("request failed");
        for attempt in 0..4u64 {
            if path.starts_with("/at-home/") {
                self.at_home_limiter.wait().await;
            }
            self.limiter.wait().await;
            let resp = match http_client().get(&url).query(query).send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = e.into();
                    tokio::time::sleep(Duration::from_millis(500 * (attempt + 1))).await;
                    continue;
                }
            };
            let status = resp.status();
            if status.as_u16() == 429 {
                let header = |n: &str| resp.headers().get(n).and_then(|v| v.to_str().ok()).map(String::from);
                let wait = retry_after_secs(header("x-ratelimit-retry-after").as_deref(), header("retry-after").as_deref(), crate::util::now());
                last_err = anyhow!("rate limited by MangaDex");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            if status.is_server_error() {
                last_err = anyhow!("MangaDex server error {status}");
                tokio::time::sleep(Duration::from_millis(700 * (attempt + 1))).await;
                continue;
            }
            let text = resp.text().await?;
            if !status.is_success() {
                bail!("MangaDex error {status}: {}", error_detail(&text));
            }
            return Ok(text);
        }
        Err(last_err)
    }

    fn list_query(&self, page: u32, order: Option<(&str, &str)>, title: Option<&str>) -> Vec<(String, String)> {
        let mut q = vec![
            ("limit".to_string(), PAGE_SIZE.to_string()),
            ("offset".to_string(), (page.saturating_sub(1) * PAGE_SIZE).to_string()),
        ];
        for inc in ["cover_art", "author", "artist"] {
            q.push(("includes[]".into(), inc.into()));
        }
        for r in self.ratings() {
            q.push(("contentRating[]".into(), r.into()));
        }
        if let Some(t) = title {
            q.push(("title".into(), t.to_string()));
        }
        if let Some((k, v)) = order {
            q.push((format!("order[{k}]"), v.to_string()));
        }
        q
    }
}

/// Seconds to wait after a 429. MangaDex sends `X-RateLimit-Retry-After` as a
/// unix timestamp; plain `Retry-After` is a number of seconds.
fn retry_after_secs(md_reset: Option<&str>, retry_after: Option<&str>, now: i64) -> u64 {
    let from_reset = md_reset.and_then(|v| v.trim().parse::<i64>().ok()).map(|ts| (ts - now).max(1) as u64);
    let from_header = retry_after.and_then(|v| v.trim().parse::<u64>().ok());
    from_reset.or(from_header).unwrap_or(2).clamp(1, 60)
}

fn error_detail(text: &str) -> String {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v["errors"][0]["detail"].as_str().map(String::from))
        .unwrap_or_else(|| text.chars().take(120).collect())
}

fn localized(v: &Value) -> String {
    match v {
        Value::Object(m) => m
            .get("en")
            .or_else(|| m.values().next())
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

fn manga_title(attrs: &Value) -> String {
    let t = localized(&attrs["title"]);
    if !t.is_empty() {
        return t;
    }
    attrs["altTitles"]
        .as_array()
        .and_then(|a| a.iter().find_map(|m| m.get("en").and_then(|s| s.as_str())))
        .unwrap_or("Untitled")
        .to_string()
}

fn cover_from(manga_id: &str, rels: &Value) -> String {
    rels.as_array()
        .and_then(|a| a.iter().find(|r| r["type"] == "cover_art"))
        .and_then(|r| r["attributes"]["fileName"].as_str())
        .map(|f| format!("{COVERS}/{manga_id}/{f}.512.jpg"))
        .unwrap_or_default()
}

fn people(rels: &Value) -> String {
    let mut names: Vec<String> = Vec::new();
    if let Some(a) = rels.as_array() {
        for r in a {
            if matches!(r["type"].as_str(), Some("author") | Some("artist")) {
                if let Some(n) = r["attributes"]["name"].as_str() {
                    if !names.iter().any(|x| x == n) {
                        names.push(n.to_string());
                    }
                }
            }
        }
    }
    names.join(", ")
}

fn parse_status(s: &str) -> Status {
    match s {
        "ongoing" => Status::Ongoing,
        "completed" => Status::Completed,
        "hiatus" => Status::Hiatus,
        "cancelled" => Status::Cancelled,
        _ => Status::Unknown,
    }
}

fn check_ok(v: &Value) -> Result<()> {
    if v["result"] == "error" {
        bail!("MangaDex: {}", v["errors"][0]["detail"].as_str().unwrap_or("unknown error"));
    }
    Ok(())
}

/// Parse a `/manga` list response.
pub fn parse_manga_list(text: &str) -> Result<Paged<MangaEntry>> {
    let v: Value = serde_json::from_str(text).context("invalid JSON from MangaDex")?;
    check_ok(&v)?;
    let data = v["data"].as_array().ok_or_else(|| anyhow!("missing data array"))?;
    let items = data
        .iter()
        .filter_map(|m| {
            let id = m["id"].as_str()?;
            Some(MangaEntry {
                key: id.to_string(),
                title: manga_title(&m["attributes"]),
                cover_url: cover_from(id, &m["relationships"]),
            })
        })
        .collect();
    let offset = v["offset"].as_u64().unwrap_or(0);
    let limit = v["limit"].as_u64().unwrap_or(0);
    let total = v["total"].as_u64().unwrap_or(0);
    Ok(Paged { items, has_next: offset + limit < total })
}

/// Parse a `/manga/{id}` response.
pub fn parse_manga_details(text: &str) -> Result<MangaDetails> {
    let v: Value = serde_json::from_str(text).context("invalid JSON from MangaDex")?;
    check_ok(&v)?;
    let m = &v["data"];
    let id = m["id"].as_str().ok_or_else(|| anyhow!("missing manga id"))?;
    let a = &m["attributes"];
    let genres = a["tags"]
        .as_array()
        .map(|t| {
            t.iter()
                .filter(|t| matches!(t["attributes"]["group"].as_str(), Some("genre") | Some("theme")))
                .map(|t| localized(&t["attributes"]["name"]))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    Ok(MangaDetails {
        title: manga_title(a),
        cover_url: cover_from(id, &m["relationships"]),
        description: localized(&a["description"]),
        authors: people(&m["relationships"]),
        genres,
        status: parse_status(a["status"].as_str().unwrap_or("")),
    })
}

/// Result of parsing one page of a chapter feed.
#[derive(Debug)]
pub struct FeedPage {
    pub chapters: Vec<ChapterEntry>,
    pub total: u32,
    pub raw_count: u32,
}

/// Parse a chapter feed page. Chapters that cannot be read in-app (external
/// links or zero pages) are skipped.
pub fn parse_chapter_feed(text: &str) -> Result<FeedPage> {
    let v: Value = serde_json::from_str(text).context("invalid JSON from MangaDex")?;
    check_ok(&v)?;
    let data = v["data"].as_array().ok_or_else(|| anyhow!("missing data array"))?;
    let mut chapters = Vec::new();
    for c in data {
        let Some(id) = c["id"].as_str() else { continue };
        let a = &c["attributes"];
        let hosted = a["externalUrl"].is_null() && a["pages"].as_u64().unwrap_or(0) > 0;
        if !hosted {
            continue;
        }
        let published = a["readableAt"]
            .as_str()
            .or_else(|| a["publishAt"].as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.timestamp())
            .unwrap_or(0);
        let scanlator = c["relationships"]
            .as_array()
            .and_then(|r| r.iter().find(|r| r["type"] == "scanlation_group"))
            .and_then(|r| r["attributes"]["name"].as_str())
            .unwrap_or("")
            .to_string();
        chapters.push(ChapterEntry {
            key: id.to_string(),
            title: a["title"].as_str().unwrap_or("").to_string(),
            number: a["chapter"].as_str().and_then(|s| s.parse().ok()),
            volume: a["volume"].as_str().map(String::from),
            scanlator,
            published,
        });
    }
    Ok(FeedPage { chapters, total: v["total"].as_u64().unwrap_or(0) as u32, raw_count: data.len() as u32 })
}

/// Keep one release per (volume, chapter number); the first listed wins.
pub fn dedupe_chapters(list: Vec<ChapterEntry>) -> Vec<ChapterEntry> {
    let mut seen = std::collections::HashSet::new();
    list.into_iter()
        .filter(|c| match c.number {
            Some(n) => seen.insert((c.volume.clone(), n.to_bits())),
            None => true,
        })
        .collect()
}

/// Parse `/at-home/server/{id}` into full image URLs.
pub fn parse_at_home(text: &str) -> Result<Vec<String>> {
    let v: Value = serde_json::from_str(text).context("invalid JSON from MangaDex")?;
    check_ok(&v)?;
    let base = v["baseUrl"].as_str().ok_or_else(|| anyhow!("missing baseUrl"))?;
    let hash = v["chapter"]["hash"].as_str().ok_or_else(|| anyhow!("missing chapter hash"))?;
    let files = v["chapter"]["data"].as_array().ok_or_else(|| anyhow!("missing page list"))?;
    Ok(files.iter().filter_map(|f| f.as_str()).map(|f| format!("{base}/data/{hash}/{f}")).collect())
}

#[async_trait]
impl Source for MangaDex {
    fn id(&self) -> &'static str {
        "mangadex"
    }
    fn name(&self) -> &'static str {
        "MangaDex"
    }

    async fn search(&self, query: &str, page: u32) -> Result<Paged<MangaEntry>> {
        let q = self.list_query(page, if query.trim().is_empty() { Some(("followedCount", "desc")) } else { Some(("relevance", "desc")) }, Some(query.trim()).filter(|s| !s.is_empty()));
        parse_manga_list(&self.get_text("/manga", &q).await?)
    }

    async fn latest(&self, page: u32) -> Result<Paged<MangaEntry>> {
        let mut q = self.list_query(page, Some(("latestUploadedChapter", "desc")), None);
        q.push(("availableTranslatedLanguage[]".into(), self.lang()));
        parse_manga_list(&self.get_text("/manga", &q).await?)
    }

    async fn popular(&self, page: u32) -> Result<Paged<MangaEntry>> {
        let mut q = self.list_query(page, Some(("followedCount", "desc")), None);
        q.push(("availableTranslatedLanguage[]".into(), self.lang()));
        parse_manga_list(&self.get_text("/manga", &q).await?)
    }

    async fn details(&self, key: &str) -> Result<MangaDetails> {
        let q = ["cover_art", "author", "artist"].iter().map(|i| ("includes[]".to_string(), i.to_string())).collect::<Vec<_>>();
        parse_manga_details(&self.get_text(&format!("/manga/{key}"), &q).await?)
    }

    async fn chapters(&self, manga_key: &str) -> Result<Vec<ChapterEntry>> {
        let mut all = Vec::new();
        let mut offset = 0u32;
        loop {
            let mut q = vec![
                ("limit".to_string(), FEED_LIMIT.to_string()),
                ("offset".to_string(), offset.to_string()),
                ("translatedLanguage[]".into(), self.lang()),
                ("order[volume]".into(), "desc".into()),
                ("order[chapter]".into(), "desc".into()),
                ("includes[]".into(), "scanlation_group".into()),
            ];
            for r in ["safe", "suggestive", "erotica", "pornographic"] {
                q.push(("contentRating[]".into(), r.into()));
            }
            let page = parse_chapter_feed(&self.get_text(&format!("/manga/{manga_key}/feed"), &q).await?)?;
            all.extend(page.chapters);
            offset += FEED_LIMIT;
            if offset >= page.total || page.raw_count == 0 {
                break;
            }
        }
        Ok(dedupe_chapters(all))
    }

    async fn pages(&self, chapter_key: &str) -> Result<Vec<String>> {
        let text = self.get_text(&format!("/at-home/server/{chapter_key}"), &[]).await?;
        parse_at_home(&text)
    }

    async fn image(&self, _chapter_key: &str, page: &str) -> Result<Vec<u8>> {
        // Image CDN nodes are not part of the rate-limited API.
        super::http_get(page).await
    }

    fn set_option(&self, key: &str, value: &str) {
        match key {
            "language" => {
                if let Ok(mut l) = self.lang.write() {
                    *l = value.to_string();
                }
            }
            "nsfw" => {
                if let Ok(mut n) = self.nsfw.write() {
                    *n = value == "true";
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = include_str!("../../tests/fixtures/manga_list.json");
    const DETAILS: &str = include_str!("../../tests/fixtures/manga_details.json");
    const FEED: &str = include_str!("../../tests/fixtures/chapter_feed.json");
    const HOSTED: &str = include_str!("../../tests/fixtures/chapter_feed_hosted.json");
    const AT_HOME: &str = include_str!("../../tests/fixtures/at_home.json");

    #[test]
    fn parses_manga_list() {
        let p = parse_manga_list(LIST).unwrap();
        assert_eq!(p.items.len(), 3);
        for m in &p.items {
            assert_eq!(m.key.len(), 36);
            assert!(!m.title.is_empty());
            assert!(m.cover_url.starts_with("https://uploads.mangadex.org/covers/"));
            assert!(m.cover_url.contains(&m.key));
            assert!(m.cover_url.ends_with(".512.jpg"));
        }
        assert!(p.has_next);
    }

    #[test]
    fn parses_manga_details() {
        let d = parse_manga_details(DETAILS).unwrap();
        assert!(!d.title.is_empty());
        assert!(!d.description.is_empty());
        assert!(!d.authors.is_empty());
        assert!(!d.genres.is_empty());
        assert_ne!(d.status, Status::Unknown);
        assert!(d.cover_url.contains("/covers/"));
    }

    #[test]
    fn feed_skips_external_chapters() {
        let p = parse_chapter_feed(FEED).unwrap();
        assert_eq!(p.raw_count, 8);
        assert!(p.total >= 8);
        assert!(p.chapters.is_empty(), "all fixture chapters are external-only");
    }

    #[test]
    fn feed_parses_hosted_chapters() {
        let p = parse_chapter_feed(HOSTED).unwrap();
        assert_eq!(p.chapters.len(), 6);
        for c in &p.chapters {
            assert_eq!(c.key.len(), 36);
            assert!(c.published > 1_500_000_000);
        }
        assert!(p.chapters.iter().any(|c| c.number.is_some()));
    }

    #[test]
    fn dedupes_same_chapter_from_multiple_groups() {
        let mk = |k: &str, n: f64| ChapterEntry { key: k.into(), number: Some(n), ..Default::default() };
        let out = dedupe_chapters(vec![mk("a", 2.0), mk("b", 2.0), mk("c", 1.0), ChapterEntry { key: "d".into(), ..Default::default() }]);
        assert_eq!(out.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), vec!["a", "c", "d"]);
    }

    #[test]
    fn parses_at_home() {
        let urls = parse_at_home(AT_HOME).unwrap();
        assert!(!urls.is_empty());
        for u in &urls {
            assert!(u.starts_with("https://"));
            assert!(u.contains("/data/"));
        }
    }

    #[test]
    fn error_responses_are_reported() {
        let err = r#"{"result":"error","errors":[{"status":404,"detail":"Manga not found"}]}"#;
        let e = parse_manga_details(err).unwrap_err().to_string();
        assert!(e.contains("Manga not found"));
        assert!(parse_manga_list("not json").is_err());
        assert!(parse_at_home(r#"{"result":"ok"}"#).is_err());
    }

    #[test]
    fn localized_prefers_english() {
        let v: Value = serde_json::json!({"ja": "日本語", "en": "English"});
        assert_eq!(localized(&v), "English");
        let v: Value = serde_json::json!({"ja": "日本語"});
        assert_eq!(localized(&v), "日本語");
    }

    #[test]
    fn retry_after_prefers_mangadex_reset_timestamp() {
        assert_eq!(retry_after_secs(Some("1010"), Some("3"), 1000), 10);
        assert_eq!(retry_after_secs(None, Some("3"), 1000), 3);
        assert_eq!(retry_after_secs(Some("900"), None, 1000), 1, "past reset waits minimally");
        assert_eq!(retry_after_secs(None, None, 0), 2);
        assert_eq!(retry_after_secs(Some("999999"), None, 0), 60, "capped");
    }

    #[tokio::test(start_paused = true)]
    async fn limiter_spaces_at_home_calls_within_quota() {
        let l = RateLimiter::new(AT_HOME_INTERVAL);
        let start = tokio::time::Instant::now();
        for _ in 0..41 {
            l.wait().await;
        }
        // 41 calls must take at least a minute to stay under 40/min
        assert!(start.elapsed() >= Duration::from_secs(60), "{:?}", start.elapsed());
    }

    #[test]
    fn query_respects_nsfw_option() {
        let md = MangaDex::default();
        assert_eq!(md.ratings(), vec!["safe", "suggestive"]);
        md.set_option("nsfw", "true");
        assert_eq!(md.ratings().len(), 4);
        let q = md.list_query(3, None, Some("x"));
        assert!(q.contains(&("offset".to_string(), "48".to_string())));
        assert!(q.contains(&("title".to_string(), "x".to_string())));
    }
}
