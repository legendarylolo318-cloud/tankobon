//! The reader: paged (RTL/LTR/vertical) and webtoon modes, page loading,
//! preloading, progress tracking and per-manga settings.

use crate::ctx::{pixels_bytes, to_buffer, to_image, Pixels, C};
use anyhow::{anyhow, Result};
use slint::ComponentHandle;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tankobon_core::downloads;
use tankobon_core::imaging;
use tankobon_core::models::{ReaderMode, ReaderSettings, Scaling};
use tankobon_core::{Chapter, Manga};
use tankobon_ui::{Actions, State, WebtoonItem};

/// Upper bound of the memory budget for decoded pages.
pub const PAGE_CACHE_BYTES: usize = 192 * 1024 * 1024;

/// Budget for decoded pages given the window size in physical pixels: room for
/// about 16 full-screen pages (prefetch needs 3-4), between 48 and 192 MiB.
pub fn page_cache_budget(pw: u32, ph: u32) -> usize {
    let page = pw as usize * ph as usize * 4;
    (page * 16).clamp(48 * 1024 * 1024, PAGE_CACHE_BYTES)
}

#[derive(Debug, Clone)]
pub enum PageRef {
    Source(String),
    File(PathBuf),
}

pub struct Session {
    pub gen: u64,
    pub manga: Manga,
    pub chapter: Chapter,
    /// All chapters of the manga, newest first.
    pub siblings: Vec<Chapter>,
    pub pages: Vec<PageRef>,
    pub settings: ReaderSettings,
    pub has_override: bool,
    pub page: usize,
    /// Pages visible in the current paged spread (1 or 2).
    pub shown: usize,
    /// Physical window width/height and scale factor.
    pub view: (u32, u32, f32),
    pub dims: HashMap<usize, (u32, u32)>,
    pub locks: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
    // webtoon
    pub aspects: Vec<f32>,
    pub wt_scroll: f32,
    pub wt_vh: f32,
    pub wt_vw: f32,
    pub wt_range: (usize, usize),
    /// Webtoon pages currently being fetched/decoded.
    pub wt_inflight: HashSet<usize>,
    /// Bumped on every paged navigation; only the newest load may update the view.
    pub view_seq: u64,
}

type Sess = Arc<Mutex<Session>>;

thread_local! {
    /// Decoded webtoon pages as Slint images (UI thread only).
    static WT_IMAGES: RefCell<HashMap<usize, slint::Image>> = RefCell::new(HashMap::new());
}

fn lock(s: &Sess) -> std::sync::MutexGuard<'_, Session> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

fn current(ctx: &C, gen: u64) -> Option<Sess> {
    let a = ctx.app();
    if a.reader_gen != gen {
        return None;
    }
    a.reader.clone()
}

fn current_any(ctx: &C) -> Option<Sess> {
    ctx.app().reader.clone()
}

pub fn wire(ctx: &C, w: &tankobon_ui::AppWindow) {
    let a = w.global::<Actions>();
    {
        let c = ctx.clone();
        a.on_reader_next(move || next(&c));
    }
    {
        let c = ctx.clone();
        a.on_reader_prev(move || prev(&c));
    }
    {
        let c = ctx.clone();
        a.on_reader_seek(move |p| seek(&c, p));
    }
    {
        let c = ctx.clone();
        a.on_reader_chapter(move |d| step_chapter(&c, d, false));
    }
    {
        let c = ctx.clone();
        a.on_reader_close(move || close(&c));
    }
    {
        let c = ctx.clone();
        a.on_reader_settings_changed(move || settings_changed(&c));
    }
    {
        let c = ctx.clone();
        a.on_reader_override_toggled(move |on| override_toggled(&c, on));
    }
    {
        let c = ctx.clone();
        a.on_reader_fullscreen_toggled(move || {
            if let Some(w) = c.win() {
                let st = w.global::<State>();
                st.set_r_fullscreen(!st.get_r_fullscreen());
            }
        });
    }
    {
        let c = ctx.clone();
        a.on_reader_resized(move || resized(&c));
    }
    {
        let c = ctx.clone();
        a.on_reader_webtoon_scrolled(move |y, vh, vw| wt_scrolled(&c, y, vh, vw));
    }
}

// ---------------------------------------------------------------------------
// opening / closing

/// Open a chapter in the reader (UI thread). `start` is the page to begin on;
/// `None` resumes saved progress.
pub fn open_chapter(ctx: &C, chapter_id: i64, start: Option<usize>) {
    let Some(w) = ctx.win() else { return };
    let view = ctx.view_metrics();
    let gen = {
        let mut a = ctx.app();
        a.reader_gen += 1;
        a.reader = None;
        a.reader_gen
    };
    WT_IMAGES.with(|m| m.borrow_mut().clear());
    ctx.page_cache.set_limit(page_cache_budget(view.0, view.1));
    let st = w.global::<State>();
    st.set_overlay(2);
    st.set_r_loading(true);
    st.set_r_error("".into());
    st.set_r_has1(false);
    st.set_r_has2(false);
    st.set_r_menu(false);
    st.set_r_settings_open(false);
    st.set_r_page(0);
    st.set_r_total(0);
    st.set_wt_total_h(0.0);
    set_wt_items(&w, vec![]);
    let c = ctx.clone();
    ctx.rt.spawn(async move {
        match prepare(&c, chapter_id, start, view, gen).await {
            Ok(sess) => {
                if c.app().reader_gen != gen {
                    return;
                }
                c.app().reader = Some(sess.clone());
                let snapshot = meta_snapshot(&sess);
                let c2 = c.clone();
                c.ui(move |w| {
                    apply_meta(w, &snapshot);
                    render(&c2, gen);
                });
            }
            Err(e) => {
                let msg = format!("{e:#}");
                c.ui(move |w| {
                    let st = w.global::<State>();
                    st.set_r_loading(false);
                    st.set_r_error(format!("Could not open chapter: {msg}").into());
                    st.set_r_menu(true);
                });
            }
        }
    });
}

async fn prepare(c: &C, chapter_id: i64, start: Option<usize>, view: (u32, u32, f32), gen: u64) -> Result<Sess> {
    let c2 = c.clone();
    let (chapter, manga, siblings) = tokio::task::spawn_blocking(move || -> Result<_> {
        let chapter = c2.db.get_chapter(chapter_id)?.ok_or_else(|| anyhow!("chapter not found"))?;
        let manga = c2.db.get_manga(chapter.manga_id)?.ok_or_else(|| anyhow!("manga not found"))?;
        let siblings = c2.db.chapters(chapter.manga_id)?;
        Ok((chapter, manga, siblings))
    })
    .await??;

    // Prefer the offline copy when it is complete.
    let mut pages: Vec<PageRef> = Vec::new();
    if chapter.downloaded {
        let dir = downloads::chapter_dir(&c.downloads.root(), &manga, &chapter);
        if downloads::is_complete(&dir) {
            pages = downloads::downloaded_pages(&dir).into_iter().map(PageRef::File).collect();
        }
        if pages.is_empty() {
            let _ = c.db.set_downloaded(chapter.id, false, None);
        }
    }
    if pages.is_empty() {
        let src = c.registry.get(&manga.source).ok_or_else(|| anyhow!("unknown source {}", manga.source))?;
        pages = src.pages(&chapter.key).await?.into_iter().map(PageRef::Source).collect();
    }
    if pages.is_empty() {
        return Err(anyhow!("this chapter has no pages"));
    }
    let total = pages.len();
    let start_page = match start {
        Some(p) => p.min(total - 1),
        None if !chapter.read && chapter.last_page > 0 => (chapter.last_page as usize).min(total - 1),
        None => 0,
    };
    let global = c.settings().reader;
    let (settings, has_override) = match &manga.reader_override {
        Some(o) => (o.clone(), true),
        None => (global, false),
    };
    {
        let (db, id) = (c.db.clone(), chapter.id);
        tokio::task::spawn_blocking(move || db.set_progress(id, start_page as i32, total as i32)).await.ok();
    }
    Ok(Arc::new(Mutex::new(Session {
        gen,
        manga,
        chapter,
        siblings,
        pages,
        settings,
        has_override,
        page: start_page,
        shown: 1,
        view,
        dims: HashMap::new(),
        locks: HashMap::new(),
        aspects: vec![0.0; total],
        wt_scroll: 0.0,
        wt_vh: view.1 as f32 / view.2,
        wt_vw: view.0 as f32 / view.2,
        wt_range: (usize::MAX, 0),
        wt_inflight: HashSet::new(),
        view_seq: 0,
    })))
}

struct Meta {
    title: String,
    chapter: String,
    total: usize,
    page: usize,
    settings: ReaderSettings,
    has_override: bool,
    has_prev: bool,
    has_next: bool,
}

fn meta_snapshot(s: &Sess) -> Meta {
    let g = lock(s);
    let idx = g.siblings.iter().position(|c| c.id == g.chapter.id);
    Meta {
        title: g.manga.title.clone(),
        chapter: g.chapter.display_title(),
        total: g.pages.len(),
        page: g.page,
        settings: g.settings.clone(),
        has_override: g.has_override,
        has_prev: idx.is_some_and(|i| i + 1 < g.siblings.len()),
        has_next: idx.is_some_and(|i| i > 0),
    }
}

fn apply_meta(w: &tankobon_ui::AppWindow, m: &Meta) {
    let st = w.global::<State>();
    st.set_r_title(m.title.clone().into());
    st.set_r_chapter(m.chapter.clone().into());
    st.set_r_total(m.total as i32);
    st.set_r_page(m.page as i32);
    st.set_r_mode(m.settings.mode.index());
    st.set_r_scaling(m.settings.scaling.index());
    st.set_r_double(m.settings.double_page);
    st.set_r_crop(m.settings.crop_borders);
    st.set_r_bg(m.settings.background);
    st.set_r_override(m.has_override);
    st.set_r_has_prev_chapter(m.has_prev);
    st.set_r_has_next_chapter(m.has_next);
}

pub fn close(ctx: &C) {
    let Some(w) = ctx.win() else { return };
    if let Some(s) = current_any(ctx) {
        let (id, page, shown, total) = {
            let g = lock(&s);
            (g.chapter.id, g.page, g.shown, g.pages.len())
        };
        let last = if shown == 2 { page + 1 } else { page };
        save_progress(ctx, id, last, total);
    }
    {
        let mut a = ctx.app();
        a.reader_gen += 1;
        a.reader = None;
    }
    WT_IMAGES.with(|m| m.borrow_mut().clear());
    let st = w.global::<State>();
    st.set_overlay(if ctx.app().manga.id != 0 { 1 } else { 0 });
    st.set_r_fullscreen(false);
    st.set_r_has1(false);
    st.set_r_has2(false);
    st.set_r_img1(Default::default());
    st.set_r_img2(Default::default());
    set_wt_items(&w, vec![]);
    // progress changed: refresh dependants after the write below has landed
    let c = ctx.clone();
    ctx.rt.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let c2 = c.clone();
        c.ui(move |_| {
            crate::manga::reload(&c2);
            crate::library::refresh(&c2);
            crate::lists::refresh_history(&c2);
            crate::lists::refresh_updates(&c2);
        });
    });
}

fn save_progress(ctx: &C, chapter_id: i64, page: usize, total: usize) {
    let db = ctx.db.clone();
    ctx.rt.spawn_blocking(move || {
        let _ = db.set_progress(chapter_id, page as i32, total as i32);
    });
}

// ---------------------------------------------------------------------------
// page loading

async fn load_bytes(c: &C, sess: &Sess, idx: usize) -> Result<Vec<u8>> {
    let (page, src_id, chapter_key) = {
        let g = lock(sess);
        (g.pages.get(idx).cloned().ok_or_else(|| anyhow!("page {} out of range", idx + 1))?, g.manga.source.clone(), g.chapter.key.clone())
    };
    match page {
        PageRef::File(p) => Ok(tokio::fs::read(p).await?),
        PageRef::Source(r) => {
            let local = src_id == "local";
            let key = format!("{src_id}|{chapter_key}|{}", stable_page_key(&r));
            if !local {
                let dc = c.disk_pages.clone();
                let k = key.clone();
                if let Some(b) = tokio::task::spawn_blocking(move || dc.get(&k)).await? {
                    return Ok(b);
                }
            }
            let src = c.registry.get(&src_id).ok_or_else(|| anyhow!("unknown source"))?;
            let _permit = c.page_permits.acquire().await?;
            let bytes = match src.image(&chapter_key, &r).await {
                Ok(b) => b,
                Err(e) if local => return Err(e),
                Err(_) => {
                    // Source image URLs can expire (MangaDex@Home: ~15 min); refresh once.
                    let fresh = src.pages(&chapter_key).await?;
                    let url = fresh.get(idx).cloned().ok_or_else(|| anyhow!("page {} vanished", idx + 1))?;
                    {
                        let mut g = lock(sess);
                        if g.pages.len() == fresh.len() {
                            g.pages = fresh.into_iter().map(PageRef::Source).collect();
                        }
                    }
                    src.image(&chapter_key, &url).await?
                }
            };
            if !local {
                let dc = c.disk_pages.clone();
                let (k, b) = (key, bytes.clone());
                tokio::task::spawn_blocking(move || {
                    let _ = dc.put(&k, &b);
                });
                crate::covers::note_write(c);
            }
            Ok(bytes)
        }
    }
}

/// Cache key part for a page reference that ignores the (rotating) server:
/// `https://node.example/data/<hash>/<file>` -> `data/<hash>/<file>`.
pub fn stable_page_key(r: &str) -> &str {
    for marker in ["/data/", "/data-saver/"] {
        if let Some(i) = r.find(marker) {
            return &r[i + 1..];
        }
    }
    r
}

async fn page_dims(c: &C, sess: &Sess, idx: usize) -> Option<(u32, u32)> {
    if let Some(d) = lock(sess).dims.get(&idx) {
        return Some(*d);
    }
    let bytes = load_bytes(c, sess, idx).await.ok()?;
    let d = tokio::task::spawn_blocking(move || imaging::dimensions(&bytes)).await.ok()??;
    lock(sess).dims.insert(idx, d);
    Some(d)
}

async fn get_decoded(c: &C, sess: &Sess, idx: usize, scaling: Scaling, vw: u32, vh: u32) -> Result<Pixels> {
    let (chapter_id, crop) = {
        let g = lock(sess);
        (g.chapter.id, g.settings.crop_borders)
    };
    let key = format!("{chapter_id}:{idx}:{}:{}:{vw}x{vh}", scaling.index(), crop as u8);
    if let Some(d) = c.page_cache.get(&key) {
        return Ok(d);
    }
    let gate = lock(sess).locks.entry(key.clone()).or_default().clone();
    let _held = gate.lock().await;
    if let Some(d) = c.page_cache.get(&key) {
        return Ok(d);
    }
    let result = decode_page(c, sess, idx, scaling, vw, vh, crop).await;
    // Drop the gate whatever happened, or failed pages leak an entry each.
    lock(sess).locks.remove(&key);
    let (px, dims) = result?;
    if let Some(dm) = dims {
        lock(sess).dims.insert(idx, dm);
    }
    let n = pixels_bytes(&px);
    c.page_cache.put(key, px.clone(), n);
    Ok(px)
}

async fn decode_page(c: &C, sess: &Sess, idx: usize, scaling: Scaling, vw: u32, vh: u32, crop: bool) -> Result<(Pixels, Option<(u32, u32)>)> {
    let bytes = load_bytes(c, sess, idx).await?;
    tokio::task::spawn_blocking(move || {
        let dims = imaging::dimensions(&bytes);
        // Convert once; the Vec is freed and only the shared buffer stays alive.
        imaging::decode(&bytes, scaling, vw, vh, crop).map(|d| (to_buffer(&d), dims))
    })
    .await?
}

// ---------------------------------------------------------------------------
// rendering

/// Render the current page/position according to the active mode (UI thread).
fn render(ctx: &C, gen: u64) {
    let Some(sess) = current(ctx, gen) else { return };
    let Some(w) = ctx.win() else { return };
    let mode = lock(&sess).settings.mode;
    let st = w.global::<State>();
    if mode == ReaderMode::Webtoon {
        st.set_r_has1(false);
        st.set_r_has2(false);
        wt_start(ctx, gen);
    } else {
        set_wt_items(&w, vec![]);
        WT_IMAGES.with(|m| m.borrow_mut().clear());
        st.set_wt_total_h(0.0);
        show_paged(ctx, gen);
    }
}

fn show_paged(ctx: &C, gen: u64) {
    let Some(sess) = current(ctx, gen) else { return };
    let seq = {
        let mut g = lock(&sess);
        g.view_seq += 1;
        g.view_seq
    };
    let c = ctx.clone();
    if let Some(w) = ctx.win() {
        w.global::<State>().set_r_loading(true);
    }
    ctx.rt.spawn(async move {
        let result = load_spread(&c, &sess).await;
        let Some(sess) = current(&c, gen) else { return };
        // A newer page turn superseded this one (e.g. it finished from cache first).
        if lock(&sess).view_seq != seq {
            return;
        }
        match result {
            Ok((a, b, page, shown)) => {
                let (id, total, after, before) = {
                    let mut g = lock(&sess);
                    g.page = page;
                    g.shown = shown;
                    (g.chapter.id, g.pages.len(), page + shown, page.checked_sub(1))
                };
                save_progress(&c, id, page + shown - 1, total);
                let two = b.is_some();
                c.ui(move |w| {
                    let st = w.global::<State>();
                    st.set_r_img1(to_image(a));
                    st.set_r_has1(true);
                    if let Some(b) = b {
                        st.set_r_img2(to_image(b));
                    }
                    st.set_r_has2(two);
                    st.set_r_page(page as i32);
                    st.set_r_loading(false);
                    st.set_r_error("".into());
                });
                // preload: next two pages and the previous one
                for idx in [Some(after), Some(after + 1), before].into_iter().flatten() {
                    if idx < total {
                        let (c2, s2) = (c.clone(), sess.clone());
                        c.rt.spawn(async move {
                            let _ = decode_for_paged(&c2, &s2, idx, false).await;
                        });
                    }
                }
            }
            Err(e) => {
                let msg = format!("Could not load page: {e:#}");
                c.ui(move |w| {
                    let st = w.global::<State>();
                    st.set_r_loading(false);
                    st.set_r_error(msg.into());
                });
            }
        }
    });
}

/// Decode a page for paged modes. In double-page mode portrait pages are
/// decoded at half width so two fit side by side.
async fn decode_for_paged(c: &C, sess: &Sess, idx: usize, alone: bool) -> Result<Pixels> {
    let (settings, (pw, ph, _)) = {
        let g = lock(sess);
        (g.settings.clone(), g.view)
    };
    let mut half = false;
    if settings.double_page && !alone {
        if let Some((w, h)) = page_dims(c, sess, idx).await {
            half = h >= w;
        }
    }
    let scaling = if half && settings.scaling == Scaling::FitWidth { Scaling::Auto } else { settings.scaling };
    get_decoded(c, sess, idx, scaling, if half { pw / 2 } else { pw }, ph).await
}

type Spread = (Pixels, Option<Pixels>, usize, usize);

async fn load_spread(c: &C, sess: &Sess) -> Result<Spread> {
    let (page, total, double) = {
        let g = lock(sess);
        (g.page, g.pages.len(), g.settings.double_page)
    };
    if double && page + 1 < total {
        let (da, db) = (page_dims(c, sess, page).await, page_dims(c, sess, page + 1).await);
        let portrait = |d: Option<(u32, u32)>| d.is_none_or(|(w, h)| h >= w);
        if portrait(da) && portrait(db) {
            let (a, b) = tokio::try_join!(decode_for_paged(c, sess, page, false), decode_for_paged(c, sess, page + 1, false))?;
            return Ok((a, Some(b), page, 2));
        }
    }
    let a = decode_for_paged(c, sess, page, true).await?;
    Ok((a, None, page, 1))
}

// ---------------------------------------------------------------------------
// navigation

fn next(ctx: &C) {
    let Some(sess) = current_any(ctx) else { return };
    let (gen, np, total, mode) = {
        let g = lock(&sess);
        (g.gen, g.page + g.shown, g.pages.len(), g.settings.mode)
    };
    if mode == ReaderMode::Webtoon {
        return;
    }
    if np >= total {
        step_chapter(ctx, 1, true);
        return;
    }
    lock(&sess).page = np;
    show_paged(ctx, gen);
}

fn prev(ctx: &C) {
    let Some(sess) = current_any(ctx) else { return };
    let (gen, page, mode, double) = {
        let g = lock(&sess);
        (g.gen, g.page, g.settings.mode, g.settings.double_page)
    };
    if mode == ReaderMode::Webtoon {
        return;
    }
    if page == 0 {
        step_chapter(ctx, -1, true);
        return;
    }
    let step = {
        let g = lock(&sess);
        let portrait = |i: usize| g.dims.get(&i).is_none_or(|(w, h)| h >= w);
        if double && page >= 2 && portrait(page - 1) && portrait(page - 2) {
            2
        } else {
            1
        }
    };
    lock(&sess).page = page - step;
    show_paged(ctx, gen);
}

fn seek(ctx: &C, p: i32) {
    let Some(sess) = current_any(ctx) else { return };
    let (gen, total, mode) = {
        let g = lock(&sess);
        (g.gen, g.pages.len(), g.settings.mode)
    };
    let p = (p.max(0) as usize).min(total.saturating_sub(1));
    lock(&sess).page = p;
    if mode == ReaderMode::Webtoon {
        wt_goto_page(ctx, gen, p);
    } else {
        show_paged(ctx, gen);
    }
}

/// Move to the next (`delta` = 1, newer) or previous (-1, older) chapter.
fn step_chapter(ctx: &C, delta: i32, from_edge: bool) {
    let Some(sess) = current_any(ctx) else { return };
    let target = {
        let g = lock(&sess);
        let idx = g.siblings.iter().position(|c| c.id == g.chapter.id);
        idx.and_then(|i| {
            let j = if delta > 0 { i.checked_sub(1) } else { Some(i + 1) };
            j.and_then(|j| g.siblings.get(j))
        })
        .map(|c| c.id)
    };
    match target {
        Some(id) => {
            // Moving backwards from the first page lands on the last page of the previous chapter.
            let start = if delta < 0 && from_edge { Some(usize::MAX) } else { Some(0) };
            open_chapter(ctx, id, start);
        }
        None => ctx.toast(if delta > 0 { "No newer chapter" } else { "This is the first chapter" }),
    }
}

// ---------------------------------------------------------------------------
// settings

fn settings_changed(ctx: &C) {
    let (Some(w), Some(sess)) = (ctx.win(), current_any(ctx)) else { return };
    let st = w.global::<State>();
    let new = ReaderSettings {
        mode: ReaderMode::from_index(st.get_r_mode()),
        scaling: Scaling::from_index(st.get_r_scaling()),
        double_page: st.get_r_double(),
        crop_borders: st.get_r_crop(),
        background: st.get_r_bg(),
    };
    let (gen, manga_id, ov) = {
        let mut g = lock(&sess);
        g.settings = new.clone();
        (g.gen, g.manga.id, g.has_override)
    };
    if ov {
        let db = ctx.db.clone();
        let n = new.clone();
        ctx.rt.spawn_blocking(move || {
            let _ = db.set_reader_override(manga_id, Some(&n));
        });
    } else {
        let mut s = ctx.settings();
        s.reader = new.clone();
        ctx.set_settings(s);
        crate::prefs::load_into_ui(ctx, &w);
    }
    render(ctx, gen);
}

fn override_toggled(ctx: &C, on: bool) {
    let (Some(w), Some(sess)) = (ctx.win(), current_any(ctx)) else { return };
    let (gen, manga_id, settings) = {
        let mut g = lock(&sess);
        g.has_override = on;
        if !on {
            g.settings = ctx.settings().reader;
        }
        (g.gen, g.manga.id, g.settings.clone())
    };
    let db = ctx.db.clone();
    let ov = settings.clone();
    ctx.rt.spawn_blocking(move || {
        let _ = db.set_reader_override(manga_id, if on { Some(&ov) } else { None });
    });
    let snap = meta_snapshot(&sess);
    apply_meta(&w, &snap);
    ctx.toast(if on { "Settings now apply to this manga only" } else { "Using the global reader settings" });
    render(ctx, gen);
}

fn resized(ctx: &C) {
    let seq = {
        let mut a = ctx.app();
        a.resize_seq += 1;
        a.resize_seq
    };
    let c = ctx.clone();
    ctx.rt.spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        if c.app().resize_seq != seq {
            return;
        }
        let c2 = c.clone();
        c.ui(move |_| {
            let Some(sess) = current_any(&c2) else { return };
            let view = c2.view_metrics();
            let (gen, changed, mode) = {
                let mut g = lock(&sess);
                let changed = (g.view.0 as i64 - view.0 as i64).abs() > 8 || (g.view.1 as i64 - view.1 as i64).abs() > 8;
                g.view = view;
                (g.gen, changed, g.settings.mode)
            };
            if !changed {
                return;
            }
            if mode == ReaderMode::Webtoon {
                WT_IMAGES.with(|m| m.borrow_mut().clear());
                lock(&sess).wt_range = (usize::MAX, 0);
                let (y, vh, vw) = {
                    let g = lock(&sess);
                    (g.wt_scroll, g.wt_vh, g.wt_vw)
                };
                wt_scrolled(&c2, y, vh, vw);
            } else {
                show_paged(&c2, gen);
            }
        });
    });
}

// ---------------------------------------------------------------------------
// webtoon

fn set_wt_items(w: &tankobon_ui::AppWindow, items: Vec<WebtoonItem>) {
    let st = w.global::<State>();
    if let Some(m) = crate::ctx::set_model(st.get_wt_items(), items) {
        st.set_wt_items(m);
    }
}

/// Logical heights of all pages (unknown ones estimated).
fn wt_heights(g: &Session) -> Vec<f32> {
    let known: Vec<f32> = g.aspects.iter().copied().filter(|a| *a > 0.0).collect();
    let est = if known.is_empty() { 1.45 } else { known.iter().sum::<f32>() / known.len() as f32 };
    g.aspects.iter().map(|a| g.wt_vw * if *a > 0.0 { *a } else { est }).collect()
}

fn wt_layout(heights: &[f32]) -> (Vec<f32>, f32) {
    let mut ys = Vec::with_capacity(heights.len());
    let mut y = 0.0;
    for h in heights {
        ys.push(y);
        y += h;
    }
    (ys, y)
}

fn wt_start(ctx: &C, gen: u64) {
    let Some(sess) = current(ctx, gen) else { return };
    let page = {
        let mut g = lock(&sess);
        g.wt_range = (usize::MAX, 0);
        g.wt_scroll = 0.0;
        g.page
    };
    let c = ctx.clone();
    if let Some(w) = ctx.win() {
        w.global::<State>().set_r_loading(false);
    }
    wt_rebuild(&c, &sess);
    wt_goto_page(ctx, gen, page);
}

fn wt_goto_page(ctx: &C, gen: u64, page: usize) {
    let Some(sess) = current(ctx, gen) else { return };
    let Some(w) = ctx.win() else { return };
    let y = {
        let g = lock(&sess);
        let (ys, _) = wt_layout(&wt_heights(&g));
        ys.get(page).copied().unwrap_or(0.0)
    };
    let st = w.global::<State>();
    st.set_r_page(page as i32);
    st.set_wt_goto(y);
    st.set_wt_goto_seq(st.get_wt_goto_seq() + 1);
}

/// Index of the page the reader is on: the first one still more than a third
/// visible, or the last page once the view is scrolled to the very bottom
/// (a short final page would otherwise never become current or be marked read).
pub fn wt_current_page(ys: &[f32], heights: &[f32], scroll: f32, vh: f32) -> usize {
    let n = ys.len();
    if n == 0 {
        return 0;
    }
    let total = ys[n - 1] + heights[n - 1];
    if scroll + vh >= total - 2.0 {
        return n - 1;
    }
    ys.iter().zip(heights).position(|(y, h)| y + h > scroll + vh * 0.35).unwrap_or(n - 1)
}

/// Rebuild the windowed item model from the current layout (UI thread).
fn wt_rebuild(ctx: &C, sess: &Sess) {
    let Some(w) = ctx.win() else { return };
    let (heights, scroll, vh, n) = {
        let g = lock(sess);
        (wt_heights(&g), g.wt_scroll, g.wt_vh, g.pages.len())
    };
    let (ys, total) = wt_layout(&heights);
    let first = ys.iter().zip(&heights).position(|(y, h)| y + h > scroll).unwrap_or(0);
    let last = ys.iter().rposition(|y| *y < scroll + vh).unwrap_or(first);
    let (lo, hi) = (first.saturating_sub(3), (last + 4).min(n.saturating_sub(1)));
    let items: Vec<WebtoonItem> = WT_IMAGES.with(|imgs| {
        let imgs = imgs.borrow();
        (lo..=hi)
            .map(|i| match imgs.get(&i) {
                Some(img) => WebtoonItem { index: i as i32, y: ys[i], h: heights[i], img: img.clone(), loaded: true },
                None => WebtoonItem { index: i as i32, y: ys[i], h: heights[i], img: Default::default(), loaded: false },
            })
            .collect()
    });
    let st = w.global::<State>();
    if (st.get_wt_total_h() - total).abs() > 0.5 {
        st.set_wt_total_h(total);
    }
    set_wt_items(&w, items);
    lock(sess).wt_range = (lo, hi);
}

fn wt_scrolled(ctx: &C, y: f32, vh: f32, vw: f32) {
    let Some(sess) = current_any(ctx) else { return };
    let (gen, is_wt) = {
        let g = lock(&sess);
        (g.gen, g.settings.mode == ReaderMode::Webtoon)
    };
    if !is_wt || vh <= 1.0 || vw <= 1.0 {
        return;
    }
    let (heights, ys, n, scale, phys_w, phys_h, old_range, old_page, chapter_id) = {
        let mut g = lock(&sess);
        g.wt_scroll = y.max(0.0);
        g.wt_vh = vh;
        g.wt_vw = vw;
        let heights = wt_heights(&g);
        let (ys, _) = wt_layout(&heights);
        (heights, ys, g.pages.len(), g.view.2, g.view.0, g.view.1, g.wt_range, g.page, g.chapter.id)
    };
    let scroll = y.max(0.0);
    let first = ys.iter().zip(&heights).position(|(yy, h)| yy + h > scroll).unwrap_or(0);
    let last = ys.iter().rposition(|yy| *yy < scroll + vh).unwrap_or(first);
    // current page: first one that is still more than a third visible
    let cp = wt_current_page(&ys, &heights, scroll, vh);

    if cp != old_page {
        lock(&sess).page = cp;
        if let Some(w) = ctx.win() {
            w.global::<State>().set_r_page(cp as i32);
        }
        save_progress(ctx, chapter_id, cp, n);
    }

    // load visible pages and a little around them
    let (lo, hi) = (first.saturating_sub(1), (last + 2).min(n.saturating_sub(1)));
    let target_w = ((vw * scale).round() as u32).clamp(1, phys_w.max(1));
    for idx in lo..=hi {
        let loaded = WT_IMAGES.with(|m| m.borrow().contains_key(&idx));
        // Scroll events arrive every frame: start each page load only once.
        if loaded || !lock(&sess).wt_inflight.insert(idx) {
            continue;
        }
        let (c, s) = (ctx.clone(), sess.clone());
        ctx.rt.spawn(async move {
            let res = get_decoded(&c, &s, idx, Scaling::FitWidth, target_w, phys_h).await;
            lock(&s).wt_inflight.remove(&idx);
            match res {
                Ok(buf) => {
                    let aspect = buf.height() as f32 / buf.width().max(1) as f32;
                    let c2 = c.clone();
                    c.ui(move |_| wt_apply(&c2, gen, idx, buf, aspect));
                }
                Err(e) => {
                    let msg = format!("Page {} failed: {e:#}", idx + 1);
                    c.ui(move |w| w.global::<State>().set_r_error(msg.into()));
                }
            }
        });
    }

    // keep the model window in sync
    let want = (first.saturating_sub(3), (last + 4).min(n.saturating_sub(1)));
    if want != old_range {
        // evict images far from the viewport
        WT_IMAGES.with(|m| m.borrow_mut().retain(|i, _| *i + 6 >= first && *i <= last + 8));
        wt_rebuild(ctx, &sess);
    }
}

/// A page finished decoding (UI thread).
fn wt_apply(ctx: &C, gen: u64, idx: usize, buf: Pixels, aspect: f32) {
    let Some(sess) = current(ctx, gen) else { return };
    let Some(w) = ctx.win() else { return };
    WT_IMAGES.with(|m| m.borrow_mut().insert(idx, to_image(buf)));
    let shift = {
        let mut g = lock(&sess);
        let before = wt_heights(&g);
        let (ys, _) = wt_layout(&before);
        g.aspects[idx] = aspect;
        let after = wt_heights(&g);
        // Keep what the reader is looking at in place when content above grows or shrinks.
        let above: f32 = (0..g.pages.len()).filter(|i| ys[*i] + before[*i] <= g.wt_scroll).map(|i| after[i] - before[i]).sum();
        above
    };
    wt_rebuild(ctx, &sess);
    if shift.abs() > 0.5 {
        let st = w.global::<State>();
        let target = lock(&sess).wt_scroll + shift;
        st.set_wt_goto(target.max(0.0));
        st.set_wt_goto_seq(st.get_wt_goto_seq() + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_budget_scales_with_window() {
        assert_eq!(page_cache_budget(1280, 800), 1280 * 800 * 4 * 16);
        assert_eq!(page_cache_budget(100, 100), 48 * 1024 * 1024);
        assert_eq!(page_cache_budget(7680, 4320), PAGE_CACHE_BYTES);
    }

    #[test]
    fn page_cache_key_ignores_mangadex_node() {
        let a = stable_page_key("https://abc.xyz.mangadex.network:443/tok1/data/h4sh/1-x.png");
        let b = stable_page_key("https://uploads.mangadex.org/data/h4sh/1-x.png");
        assert_eq!(a, b);
        assert_eq!(a, "data/h4sh/1-x.png");
        assert_eq!(stable_page_key("Series/ch1/001.jpg"), "Series/ch1/001.jpg");
    }

    #[test]
    fn webtoon_bottom_counts_as_last_page() {
        // three tall pages and a short credits page
        let heights = [1000.0, 1000.0, 1000.0, 200.0];
        let (ys, total) = wt_layout(&heights);
        let vh = 800.0;
        assert_eq!(wt_current_page(&ys, &heights, 0.0, vh), 0);
        assert_eq!(wt_current_page(&ys, &heights, 1500.0, vh), 1);
        // scrolled fully down: the short last page is current
        assert_eq!(wt_current_page(&ys, &heights, total - vh, vh), 3);
        assert_eq!(wt_current_page(&[], &[], 0.0, vh), 0);
    }
}
