//! Shared application context and small helpers used by every controller module.

use slint::{ComponentHandle, Rgba8Pixel, SharedPixelBuffer};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use tankobon_core::downloads::DownloadManager;
use tankobon_core::imaging::{Decoded, DiskCache, PageCache};

/// Pixel buffer shared between the caches and the Slint images (refcounted, no copies).
pub type Pixels = SharedPixelBuffer<Rgba8Pixel>;
use tankobon_core::source::{MangaEntry, Registry};
use tankobon_core::{Chapter, Db, Settings};
use tankobon_ui::AppWindow;

/// Controller state that is not part of the Slint models.
#[derive(Default)]
pub struct AppState {
    pub selected: HashSet<i64>,
    pub library_gen: u64,
    pub explorer: Explorer,
    pub manga: MangaPage,
    pub reader: Option<Arc<Mutex<crate::reader::Session>>>,
    pub reader_gen: u64,
    pub toast_seq: u64,
    pub resize_seq: u64,
    pub covers_inflight: HashSet<String>,
    pub update_running: bool,
    pub writes_since_trim: u32,
}

#[derive(Default)]
pub struct Explorer {
    pub source_id: String,
    pub mode: i32,
    pub query: String,
    pub page: u32,
    pub entries: Vec<MangaEntry>,
    pub gen: u64,
}

#[derive(Default)]
pub struct MangaPage {
    pub id: i64,
    pub selected: HashSet<i64>,
    pub chapters: Vec<Chapter>,
    pub gen: u64,
    /// chapter id -> (state code 1 queued, 2 downloading, 3 paused, 4 failed; progress 0..1)
    pub dl: HashMap<i64, (i32, f32)>,
}

pub struct Ctx {
    pub db: Db,
    pub rt: tokio::runtime::Handle,
    pub ui: slint::Weak<AppWindow>,
    pub registry: Registry,
    pub paths: tankobon_core::paths::AppPaths,
    pub settings: RwLock<Settings>,
    pub downloads: DownloadManager,
    pub page_cache: PageCache<Pixels>,
    pub cover_cache: PageCache<Pixels>,
    pub disk_pages: DiskCache,
    pub disk_covers: DiskCache,
    pub cover_permits: tokio::sync::Semaphore,
    pub page_permits: tokio::sync::Semaphore,
    pub app: Mutex<AppState>,
    /// Window scale factor as f32 bits, readable from any thread.
    pub scale: std::sync::atomic::AtomicU32,
}

pub type C = Arc<Ctx>;

impl Ctx {
    pub fn settings(&self) -> Settings {
        self.settings.read().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn set_settings(&self, s: Settings) {
        if let Err(e) = self.db.save_settings(&s) {
            eprintln!("could not save settings: {e:#}");
        }
        if let Ok(mut w) = self.settings.write() {
            *w = s;
        }
    }

    pub fn app(&self) -> std::sync::MutexGuard<'_, AppState> {
        self.app.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Run `f` on the UI thread.
    pub fn ui<F: FnOnce(&AppWindow) + Send + 'static>(&self, f: F) {
        let _ = self.ui.upgrade_in_event_loop(move |w| f(&w));
    }

    /// UI-thread only.
    pub fn win(&self) -> Option<AppWindow> {
        self.ui.upgrade()
    }

    pub fn toast(self: &Arc<Self>, msg: impl Into<String>) {
        let msg = msg.into();
        let seq = {
            let mut a = self.app();
            a.toast_seq += 1;
            a.toast_seq
        };
        let m = msg.clone();
        self.ui(move |w| w.global::<tankobon_ui::State>().set_toast(m.into()));
        let this = self.clone();
        self.rt.spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(4)).await;
            let current = this.app().toast_seq;
            if current == seq {
                this.ui(|w| w.global::<tankobon_ui::State>().set_toast("".into()));
            }
        });
    }

    /// Run blocking work off the UI thread, then hand the result to `then` on the UI thread.
    pub fn blocking<T, W, D>(self: &Arc<Self>, work: W, then: D)
    where
        T: Send + 'static,
        W: FnOnce(&Ctx) -> T + Send + 'static,
        D: FnOnce(&AppWindow, T) + Send + 'static,
    {
        let this = self.clone();
        self.rt.spawn(async move {
            let ctx = this.clone();
            if let Ok(v) = tokio::task::spawn_blocking(move || work(&ctx)).await {
                this.ui(move |w| then(w, v));
            }
        });
    }

    /// Physical window size and scale factor (UI thread).
    pub fn view_metrics(&self) -> (u32, u32, f32) {
        match self.win() {
            Some(w) => {
                let s = w.window().size();
                let f = w.window().scale_factor();
                self.scale.store(f.to_bits(), std::sync::atomic::Ordering::Relaxed);
                (s.width.max(1), s.height.max(1), f)
            }
            None => (1280, 800, 1.0),
        }
    }

    /// Last known window scale factor (any thread).
    pub fn scale_factor(&self) -> f32 {
        let f = f32::from_bits(self.scale.load(std::sync::atomic::Ordering::Relaxed));
        if f.is_finite() && f > 0.0 { f } else { 1.0 }
    }
}

pub fn to_buffer(d: &Decoded) -> Pixels {
    SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&d.rgba, d.width, d.height)
}

/// Bytes held by a pixel buffer.
pub fn pixels_bytes(p: &Pixels) -> usize {
    p.width() as usize * p.height() as usize * 4
}

pub fn to_image(b: SharedPixelBuffer<Rgba8Pixel>) -> slint::Image {
    slint::Image::from_rgba8(b)
}

/// Replace the content of a Slint array property, reusing the existing `VecModel` when possible.
pub fn set_model<T: Clone + 'static>(current: slint::ModelRc<T>, new: Vec<T>) -> Option<slint::ModelRc<T>> {
    use slint::Model;
    if let Some(vm) = current.as_any().downcast_ref::<slint::VecModel<T>>() {
        vm.set_vec(new);
        None
    } else {
        Some(slint::ModelRc::new(slint::VecModel::from(new)))
    }
}
