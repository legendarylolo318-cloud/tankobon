//! Cover thumbnails: disk cached, lazily loaded, decoded to a small size.

use crate::ctx::{pixels_bytes, to_buffer, Pixels, C};
use tankobon_core::imaging;
use tankobon_core::models::Scaling;

/// Width of the thumbnail kept on disk.
const THUMB_WIDTH: u32 = 320;
/// Widest a cover is ever shown, in logical pixels (the manga page).
const DISPLAY_WIDTH: f32 = 180.0;

/// Width covers are decoded at for a given scale factor: never more than shown.
pub fn decode_width(scale: f32) -> u32 {
    ((DISPLAY_WIDTH * scale.max(1.0)).ceil() as u32).min(THUMB_WIDTH)
}

/// Load the cover for `(source, url)`; returns `None` on failure.
pub async fn load(ctx: &C, source_id: &str, url: &str) -> Option<Pixels> {
    if url.is_empty() {
        return None;
    }
    let key = format!("{source_id}|{url}");
    if let Some(p) = ctx.cover_cache.get(&key) {
        return Some(p);
    }
    let _permit = ctx.cover_permits.acquire().await.ok()?;
    if let Some(p) = ctx.cover_cache.get(&key) {
        return Some(p);
    }
    let bytes = match ctx.disk_covers.get(&key) {
        Some(b) => b,
        None => {
            let src = ctx.registry.get(source_id)?;
            let raw = src.cover(url).await.ok()?;
            let thumb = tokio::task::spawn_blocking(move || imaging::make_thumbnail(&raw, THUMB_WIDTH)).await.ok()?.ok()?;
            let _ = ctx.disk_covers.put(&key, &thumb);
            note_write(ctx);
            thumb
        }
    };
    let width = decode_width(ctx.scale_factor());
    let pixels = tokio::task::spawn_blocking(move || imaging::decode(&bytes, Scaling::FitWidth, width, u32::MAX, false).map(|d| to_buffer(&d)))
        .await
        .ok()?
        .ok()?;
    let n = pixels_bytes(&pixels);
    ctx.cover_cache.put(key, pixels.clone(), n);
    Some(pixels)
}

/// Count cache writes and occasionally trim the on-disk cache to its limit.
pub fn note_write(ctx: &C) {
    let due = {
        let mut a = ctx.app();
        a.writes_since_trim += 1;
        if a.writes_since_trim >= 40 {
            a.writes_since_trim = 0;
            true
        } else {
            false
        }
    };
    if due {
        trim(ctx);
    }
}

pub fn trim(ctx: &C) {
    let limit = ctx.settings().cache_limit_mb * 1024 * 1024;
    let dir = ctx.paths.cache.clone();
    ctx.rt.spawn_blocking(move || {
        imaging::trim_dir(&dir, limit);
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn covers_are_decoded_at_display_size() {
        assert_eq!(super::decode_width(1.0), 180);
        assert_eq!(super::decode_width(1.5), 270);
        assert_eq!(super::decode_width(3.0), 320, "capped at the stored thumbnail");
    }
}
