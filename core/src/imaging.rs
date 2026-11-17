//! Image decoding, scaling, cropping, and caches.

use crate::models::Scaling;
use crate::util::{now, stable_hash};
use anyhow::{Context, Result};
use image::{imageops::FilterType, DynamicImage, GenericImageView, RgbaImage};
use lru::LruCache;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A decoded RGBA8 image.
#[derive(Debug, Clone)]
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Decoded {
    pub fn byte_size(&self) -> usize {
        self.rgba.len()
    }
}

/// Size an image should be decoded at for a viewport (physical pixels).
/// Never upscales: the UI stretches smaller images if it wants to.
pub fn target_size(w: u32, h: u32, scaling: Scaling, vw: u32, vh: u32) -> (u32, u32) {
    if w == 0 || h == 0 || vw == 0 || vh == 0 {
        return (w, h);
    }
    let (wf, hf) = (w as f64, h as f64);
    let scale = match scaling {
        Scaling::FitWidth => vw as f64 / wf,
        Scaling::FitHeight => vh as f64 / hf,
        Scaling::Auto => (vw as f64 / wf).min(vh as f64 / hf),
        Scaling::Original => 1.0,
    };
    if scale >= 1.0 {
        return (w, h);
    }
    (((wf * scale).round() as u32).max(1), ((hf * scale).round() as u32).max(1))
}

fn is_blank(px: &image::Rgba<u8>) -> bool {
    // fully transparent, or near-white
    px[3] < 16 || (px[0] >= 235 && px[1] >= 235 && px[2] >= 235)
}

/// Bounding box `(x, y, w, h)` of the non-white content. Rows/columns with
/// under 0.3% content pixels count as border so scan noise does not defeat cropping.
pub fn content_bounds(img: &RgbaImage) -> (u32, u32, u32, u32) {
    let (w, h) = img.dimensions();
    if w < 8 || h < 8 {
        return (0, 0, w, h);
    }
    let mut col = vec![0u32; w as usize];
    let mut row = vec![0u32; h as usize];
    for (x, y, p) in img.enumerate_pixels() {
        if !is_blank(p) {
            col[x as usize] += 1;
            row[y as usize] += 1;
        }
    }
    let row_min = ((w as f64 * 0.003).ceil() as u32).max(1);
    let col_min = ((h as f64 * 0.003).ceil() as u32).max(1);
    let first = |v: &[u32], min: u32| v.iter().position(|&c| c >= min);
    let last = |v: &[u32], min: u32| v.iter().rposition(|&c| c >= min);
    let (Some(x0), Some(x1), Some(y0), Some(y1)) = (first(&col, col_min), last(&col, col_min), first(&row, row_min), last(&row, row_min)) else {
        return (0, 0, w, h);
    };
    // Small safety margin, and refuse to crop away most of the page.
    let m = 2;
    let (x0, y0) = (x0.saturating_sub(m) as u32, y0.saturating_sub(m) as u32);
    let (x1, y1) = (((x1 as u32) + m as u32).min(w - 1), ((y1 as u32) + m as u32).min(h - 1));
    let (cw, ch) = (x1 - x0 + 1, y1 - y0 + 1);
    if (cw as u64 * ch as u64) * 100 < (w as u64 * h as u64) * 25 {
        return (0, 0, w, h);
    }
    (x0, y0, cw, ch)
}

/// Decode `bytes`, optionally crop white borders, then downscale to the viewport.
pub fn decode(bytes: &[u8], scaling: Scaling, vw: u32, vh: u32, crop: bool) -> Result<Decoded> {
    let img = image::load_from_memory(bytes).context("unsupported or corrupt image")?;
    let mut rgba = img.into_rgba8();
    if crop {
        let (x, y, w, h) = content_bounds(&rgba);
        if (w, h) != rgba.dimensions() {
            rgba = image::imageops::crop_imm(&rgba, x, y, w, h).to_image();
        }
    }
    let (tw, th) = target_size(rgba.width(), rgba.height(), scaling, vw, vh);
    if (tw, th) != rgba.dimensions() {
        rgba = image::imageops::resize(&rgba, tw, th, FilterType::CatmullRom);
    }
    Ok(Decoded { width: rgba.width(), height: rgba.height(), rgba: rgba.into_raw() })
}

/// Image dimensions without a full decode.
pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format().ok()?.into_dimensions().ok()
}

/// Build a JPEG cover thumbnail no wider than `width`.
pub fn make_thumbnail(bytes: &[u8], width: u32) -> Result<Vec<u8>> {
    let img = image::load_from_memory(bytes).context("cover is not a decodable image")?;
    let img = if img.width() > width {
        let h = ((img.height() as f64) * (width as f64 / img.width() as f64)).round().max(1.0) as u32;
        img.resize_exact(width, h, FilterType::CatmullRom)
    } else {
        img
    };
    let rgb = DynamicImage::ImageRgb8(img.to_rgb8());
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 82).encode_image(&rgb)?;
    Ok(out)
}

pub fn decode_plain(bytes: &[u8]) -> Result<Decoded> {
    let img = image::load_from_memory(bytes).context("unsupported or corrupt image")?;
    let (w, h) = img.dimensions();
    Ok(Decoded { width: w, height: h, rgba: img.into_rgba8().into_raw() })
}

// ---- in-memory page cache ------------------------------------------------

struct Inner<V> {
    lru: LruCache<String, (V, usize)>,
    bytes: usize,
    limit: usize,
}

/// LRU cache of decoded images bounded by total bytes. Values are cheap clones
/// (`Arc`s or refcounted pixel buffers), so the UI and the cache share memory.
pub struct PageCache<V: Clone = Arc<Decoded>> {
    inner: Mutex<Inner<V>>,
}

impl PageCache<Arc<Decoded>> {
    /// Convenience for `Arc<Decoded>` values.
    pub fn put_decoded(&self, key: String, img: Arc<Decoded>) {
        let n = img.byte_size();
        self.put(key, img, n);
    }
}

impl<V: Clone> PageCache<V> {
    pub fn new(limit_bytes: usize) -> Self {
        Self { inner: Mutex::new(Inner { lru: LruCache::unbounded(), bytes: 0, limit: limit_bytes }) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner<V>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self, key: &str) -> Option<V> {
        self.lock().lru.get(key).map(|v| v.0.clone())
    }

    pub fn contains(&self, key: &str) -> bool {
        self.lock().lru.contains(key)
    }

    /// Insert `value`, which occupies `bytes` of memory.
    pub fn put(&self, key: String, value: V, bytes: usize) {
        let mut g = self.lock();
        g.bytes += bytes;
        if let Some((_, old)) = g.lru.put(key, (value, bytes)) {
            g.bytes -= old;
        }
        Self::evict(&mut g);
    }

    fn evict(g: &mut Inner<V>) {
        // Always keep the most recent entry so one huge page can still be shown.
        while g.bytes > g.limit && g.lru.len() > 1 {
            if let Some((_, (_, n))) = g.lru.pop_lru() {
                g.bytes -= n;
            }
        }
    }

    pub fn set_limit(&self, limit_bytes: usize) {
        let mut g = self.lock();
        g.limit = limit_bytes;
        Self::evict(&mut g);
    }

    pub fn clear(&self) {
        let mut g = self.lock();
        g.lru.clear();
        g.bytes = 0;
    }

    pub fn bytes(&self) -> usize {
        self.lock().bytes
    }

    pub fn len(&self) -> usize {
        self.lock().lru.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---- on-disk cache --------------------------------------------------------

/// Simple content cache under a directory; eviction is oldest-mtime-first.
#[derive(Clone)]
pub struct DiskCache {
    dir: PathBuf,
}

impl DiskCache {
    pub fn new(dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        Self { dir }
    }

    pub fn path_for(&self, key: &str) -> PathBuf {
        self.dir.join(stable_hash(key))
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        let p = self.path_for(key);
        let data = std::fs::read(&p).ok()?;
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&p) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
        Some(data)
    }

    pub fn put(&self, key: &str, data: &[u8]) -> Result<()> {
        let p = self.path_for(key);
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, data)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }
}

fn walk_files(dir: &Path, out: &mut Vec<(PathBuf, u64, std::time::SystemTime)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.metadata() {
            Ok(m) if m.is_dir() => walk_files(&p, out),
            Ok(m) => out.push((p, m.len(), m.modified().unwrap_or(std::time::UNIX_EPOCH))),
            Err(_) => {}
        }
    }
}

/// Total size in bytes of all files below `dir`.
pub fn dir_size(dir: &Path) -> u64 {
    let mut v = Vec::new();
    walk_files(dir, &mut v);
    v.iter().map(|f| f.1).sum()
}

/// Delete least-recently-used files below `dir` until it is within `limit` bytes.
pub fn trim_dir(dir: &Path, limit: u64) -> u64 {
    let mut files = Vec::new();
    walk_files(dir, &mut files);
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= limit {
        return 0;
    }
    files.sort_by_key(|f| f.2);
    let mut freed = 0;
    for (p, len, _) in files {
        if total <= limit {
            break;
        }
        if std::fs::remove_file(&p).is_ok() {
            total -= len;
            freed += len;
        }
    }
    freed
}

/// Remove everything below `dir` but keep `dir` itself.
pub fn clear_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            if p.is_dir() {
                std::fs::remove_dir_all(&p)?;
            } else {
                std::fs::remove_file(&p)?;
            }
        }
    }
    Ok(())
}

pub fn touch_now() -> i64 {
    now()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let img = RgbaImage::from_fn(w, h, |x, y| image::Rgba(f(x, y)));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn target_sizes() {
        // 2000x3000 page on a 1000x800 viewport
        assert_eq!(target_size(2000, 3000, Scaling::FitWidth, 1000, 800), (1000, 1500));
        assert_eq!(target_size(2000, 3000, Scaling::FitHeight, 1000, 800), (533, 800));
        assert_eq!(target_size(2000, 3000, Scaling::Auto, 1000, 800), (533, 800));
        assert_eq!(target_size(2000, 3000, Scaling::Original, 1000, 800), (2000, 3000));
        // never upscale
        assert_eq!(target_size(400, 600, Scaling::FitWidth, 1000, 800), (400, 600));
        assert_eq!(target_size(10, 10, Scaling::Auto, 0, 0), (10, 10));
    }

    #[test]
    fn decode_downscales_to_display_size() {
        let data = png(400, 600, |_, _| [10, 20, 30, 255]);
        let d = decode(&data, Scaling::FitWidth, 200, 200, false).unwrap();
        assert_eq!((d.width, d.height), (200, 300));
        assert_eq!(d.rgba.len(), 200 * 300 * 4);
        let d = decode(&data, Scaling::Original, 200, 200, false).unwrap();
        assert_eq!((d.width, d.height), (400, 600));
        assert!(decode(b"garbage", Scaling::Auto, 1, 1, false).is_err());
    }

    #[test]
    fn crops_white_borders() {
        // 100x100 white with a dark 60x50 block at (20,25)
        let data = png(100, 100, |x, y| if (20..80).contains(&x) && (25..75).contains(&y) { [0, 0, 0, 255] } else { [255, 255, 255, 255] });
        let d = decode(&data, Scaling::Original, 1000, 1000, true).unwrap();
        assert!((60..70).contains(&d.width), "width {}", d.width);
        assert!((50..60).contains(&d.height), "height {}", d.height);
        // an all-white page is left alone
        let blank = png(50, 50, |_, _| [255; 4]);
        let d = decode(&blank, Scaling::Original, 1000, 1000, true).unwrap();
        assert_eq!((d.width, d.height), (50, 50));
    }

    #[test]
    fn thumbnails_are_jpeg_and_bounded() {
        let data = png(800, 1200, |x, y| [(x % 256) as u8, (y % 256) as u8, 0, 255]);
        let t = make_thumbnail(&data, 300).unwrap();
        assert_eq!(&t[..2], &[0xff, 0xd8]);
        assert_eq!(dimensions(&t), Some((300, 450)));
    }

    #[test]
    fn page_cache_evicts_lru_within_byte_limit() {
        let img = |n: usize| Arc::new(Decoded { width: 1, height: 1, rgba: vec![0; n] });
        let c: PageCache = PageCache::new(250);
        c.put_decoded("a".into(), img(100));
        c.put_decoded("b".into(), img(100));
        assert!(c.get("a").is_some()); // a is now most recent
        c.put_decoded("c".into(), img(100)); // evicts b
        assert!(c.contains("a") && c.contains("c") && !c.contains("b"));
        assert_eq!(c.bytes(), 200);
        c.put_decoded("c".into(), img(50)); // replacing adjusts accounting
        assert_eq!(c.bytes(), 150);
        c.set_limit(60);
        assert_eq!(c.len(), 1);
        c.clear();
        assert!(c.is_empty() && c.bytes() == 0);
    }

    #[test]
    fn disk_cache_and_trim() {
        let dir = tempfile::tempdir().unwrap();
        let dc = DiskCache::new(dir.path().join("pages"));
        assert!(dc.get("k").is_none());
        dc.put("k1", &[1u8; 100]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        dc.put("k2", &[2u8; 100]).unwrap();
        assert_eq!(dc.get("k1").unwrap().len(), 100);
        assert_eq!(dir_size(dir.path()), 200);
        // k1 was just read, so k2 is the oldest and goes first
        let freed = trim_dir(dir.path(), 100);
        assert_eq!(freed, 100);
        assert!(dc.get("k1").is_some());
        assert!(dc.get("k2").is_none());
        clear_dir(dir.path()).unwrap();
        assert_eq!(dir_size(dir.path()), 0);
    }
}
