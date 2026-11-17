//! Content sources. Each source is one module implementing [`Source`].

pub mod local;
pub mod mangadex;

use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, OnceLock};

use crate::models::Status;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MangaEntry {
    pub key: String,
    pub title: String,
    pub cover_url: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MangaDetails {
    pub title: String,
    pub cover_url: String,
    pub description: String,
    pub authors: String,
    pub genres: Vec<String>,
    pub status: Status,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChapterEntry {
    pub key: String,
    pub title: String,
    pub number: Option<f64>,
    pub volume: Option<String>,
    pub scanlator: String,
    pub published: i64,
}

#[derive(Debug, Clone, Default)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub has_next: bool,
}

#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    /// Pages are 1-based.
    async fn search(&self, query: &str, page: u32) -> Result<Paged<MangaEntry>>;
    async fn latest(&self, page: u32) -> Result<Paged<MangaEntry>>;
    async fn popular(&self, page: u32) -> Result<Paged<MangaEntry>>;
    async fn details(&self, key: &str) -> Result<MangaDetails>;
    /// Newest chapter first.
    async fn chapters(&self, manga_key: &str) -> Result<Vec<ChapterEntry>>;
    /// Opaque page references, in reading order.
    async fn pages(&self, chapter_key: &str) -> Result<Vec<String>>;
    /// Fetch the encoded bytes of one page.
    async fn image(&self, chapter_key: &str, page: &str) -> Result<Vec<u8>>;
    /// Fetch cover bytes. Default: plain HTTP GET.
    async fn cover(&self, url: &str) -> Result<Vec<u8>> {
        http_get(url).await
    }
    /// Runtime option hook (language, directories...). Unknown keys are ignored.
    fn set_option(&self, _key: &str, _value: &str) {}
}

pub fn http_client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        let builder = reqwest::Client::builder()
            .user_agent(concat!("Tankobon/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(60));
        // The platform verifier needs JVM initialisation on Android; use the
        // bundled Mozilla root store there instead.
        #[cfg(target_os = "android")]
        let builder = builder.tls_certs_only(
            webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().filter_map(|c| reqwest::Certificate::from_der(c.as_ref()).ok()),
        );
        builder.build().unwrap_or_else(|_| reqwest::Client::new())
    })
}

pub async fn http_get(url: &str) -> Result<Vec<u8>> {
    let mut last = None;
    for attempt in 0..3u64 {
        match http_client().get(url).send().await.and_then(|r| r.error_for_status()) {
            Ok(r) => match r.bytes().await {
                Ok(b) => return Ok(b.to_vec()),
                Err(e) => last = Some(e),
            },
            Err(e) => {
                if e.status().is_some_and(|s| s.is_client_error() && s.as_u16() != 429) {
                    return Err(e.into());
                }
                last = Some(e)
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(400 * (attempt + 1))).await;
    }
    Err(last.map(Into::into).unwrap_or_else(|| anyhow::anyhow!("request failed")))
}

#[derive(Clone, Default)]
pub struct Registry {
    sources: Vec<Arc<dyn Source>>,
}

impl Registry {
    pub fn new(sources: Vec<Arc<dyn Source>>) -> Self {
        Self { sources }
    }
    pub fn get(&self, id: &str) -> Option<Arc<dyn Source>> {
        self.sources.iter().find(|s| s.id() == id).cloned()
    }
    pub fn all(&self) -> &[Arc<dyn Source>] {
        &self.sources
    }
}
