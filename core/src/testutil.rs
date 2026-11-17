//! Test helpers shared by unit tests.

use crate::source::{ChapterEntry, MangaDetails, MangaEntry, Paged, Source};
use anyhow::{bail, Result};
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub fn tiny_png() -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255]));
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
    out
}

/// In-memory source with a mutable chapter list.
pub struct FakeSource {
    pub chapters: Mutex<Vec<ChapterEntry>>,
    pub pages: usize,
    pub delay: Duration,
    pub image_calls: AtomicUsize,
    pub fail_chapters: Mutex<bool>,
}

impl FakeSource {
    pub fn new(chapters: usize, pages: usize) -> Self {
        Self {
            chapters: Mutex::new(Self::list(chapters)),
            pages,
            delay: Duration::ZERO,
            image_calls: AtomicUsize::new(0),
            fail_chapters: Mutex::new(false),
        }
    }

    pub fn list(n: usize) -> Vec<ChapterEntry> {
        (1..=n)
            .rev()
            .map(|i| ChapterEntry { key: format!("ch{i}"), title: format!("Chapter {i}"), number: Some(i as f64), ..Default::default() })
            .collect()
    }
}

#[async_trait]
impl Source for FakeSource {
    fn id(&self) -> &'static str {
        "fake"
    }
    fn name(&self) -> &'static str {
        "Fake"
    }
    async fn search(&self, _q: &str, _p: u32) -> Result<Paged<MangaEntry>> {
        Ok(Paged::default())
    }
    async fn latest(&self, _p: u32) -> Result<Paged<MangaEntry>> {
        Ok(Paged::default())
    }
    async fn popular(&self, _p: u32) -> Result<Paged<MangaEntry>> {
        Ok(Paged::default())
    }
    async fn details(&self, key: &str) -> Result<MangaDetails> {
        Ok(MangaDetails { title: format!("Fake {key}"), description: "d".into(), ..Default::default() })
    }
    async fn chapters(&self, _k: &str) -> Result<Vec<ChapterEntry>> {
        if *self.fail_chapters.lock().unwrap() {
            bail!("boom");
        }
        Ok(self.chapters.lock().unwrap().clone())
    }
    async fn pages(&self, chapter_key: &str) -> Result<Vec<String>> {
        Ok((0..self.pages).map(|i| format!("{chapter_key}/{i}")).collect())
    }
    async fn image(&self, _c: &str, _p: &str) -> Result<Vec<u8>> {
        self.image_calls.fetch_add(1, Ordering::SeqCst);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(tiny_png())
    }
}
