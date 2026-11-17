//! Tankobon application: wires the Slint UI to the core library. Used by the
//! desktop binary (`main.rs`) and, on Android, through `android_main`.

#[cfg(feature = "autopilot")]
mod autopilot;
mod covers;
mod ctx;
mod explorer;
mod library;
mod lists;
mod manga;
mod prefs;
mod reader;

use anyhow::{Context, Result};
use ctx::{AppState, Ctx};
use slint::ComponentHandle;
use std::sync::{Arc, RwLock};
use tankobon_core::downloads::DownloadManager;
use tankobon_core::imaging::{DiskCache, PageCache};
use tankobon_core::paths::AppPaths;
use tankobon_core::source::local::Local;
use tankobon_core::source::mangadex::MangaDex;
use tankobon_core::source::{Registry, Source};
use tankobon_core::Db;
use tankobon_ui::AppWindow;

/// Large page buffers are allocated and freed on worker threads. With glibc's
/// default dynamic mmap threshold they end up in per-thread heaps that are never
/// returned to the OS, so RSS only grows while reading. A fixed threshold keeps
/// every buffer over 256 KiB in its own mapping, released on free.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn tune_allocator() {
    // SAFETY: mallopt only adjusts allocator parameters; called before any threads start.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, 256 * 1024);
        libc::mallopt(libc::M_ARENA_MAX, 4);
    }
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn tune_allocator() {}

/// Desktop default: Slint's software renderer. Measured on a 1920x1080 window it
/// keeps webtoon scrolling and page turns at display rate while using ~60 MB less
/// RAM and no VRAM. `SLINT_BACKEND=winit-femtovg` selects the OpenGL renderer.
fn select_renderer() {
    #[cfg(not(target_os = "android"))]
    if std::env::var_os("SLINT_BACKEND").is_none() {
        if let Err(e) = slint::BackendSelector::new().backend_name("winit".into()).renderer_name("software".into()).select() {
            eprintln!("software renderer unavailable, using the default: {e}");
        }
    }
    // Matches tankobon.desktop so docks and window rules recognise the window.
    let _ = slint::set_xdg_app_id("tankobon");
}

/// Desktop entry point.
pub fn desktop_main() -> Result<()> {
    tune_allocator();
    run(AppPaths::discover()?)
}

/// Android entry point (called by the NativeActivity glue).
#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    let internal = app.internal_data_path().unwrap_or_else(|| std::path::PathBuf::from("/data/local/tmp/tankobon"));
    // App-specific external storage is reachable over USB, so local manga go there.
    let external = app.external_data_path().unwrap_or_else(|| internal.clone());
    let mut paths = AppPaths::at(internal.join("config"), internal.join("data"), internal.join("cache"));
    paths.local = Some(external.join("local"));
    if let Err(e) = slint::android::init(app) {
        eprintln!("android init failed: {e}");
        return;
    }
    if let Err(e) = run(paths) {
        eprintln!("tankobon failed: {e:#}");
    }
}

/// Start the application with the given directories and run until the window closes.
pub fn run(paths: AppPaths) -> Result<()> {
    paths.ensure()?;
    let db = Db::open(&paths.db_file())?;
    let settings = db.load_settings();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .thread_name("tankobon-worker")
        .enable_all()
        .build()
        .context("starting async runtime")?;

    let local_dir = if settings.local_dir.trim().is_empty() { paths.local_dir() } else { settings.local_dir.clone().into() };
    let registry = Registry::new(vec![
        Arc::new(MangaDex::new(&settings.mangadex_language, settings.mangadex_nsfw)) as Arc<dyn Source>,
        Arc::new(Local::new(local_dir, paths.cache.clone())),
    ]);

    select_renderer();
    let window = AppWindow::new().context("creating window")?;
    let weak = window.as_weak();

    // The download manager reports through the context, which does not exist yet.
    let ctx_cell: Arc<std::sync::OnceLock<ctx::C>> = Arc::new(std::sync::OnceLock::new());
    let listener_cell = ctx_cell.clone();
    let download_root = if settings.download_dir.trim().is_empty() { paths.default_download_dir() } else { settings.download_dir.clone().into() };
    let downloads = DownloadManager::new(
        db.clone(),
        registry.clone(),
        download_root,
        settings.max_concurrent_downloads as usize,
        Arc::new(move |e| {
            if let Some(c) = listener_cell.get() {
                lists::on_download_event(c, e);
            }
        }),
    );

    let ctx: ctx::C = Arc::new(Ctx {
        db,
        rt: runtime.handle().clone(),
        ui: weak,
        registry,
        disk_pages: DiskCache::new(paths.cache.join("pages")),
        disk_covers: DiskCache::new(paths.covers_dir()),
        paths,
        settings: RwLock::new(settings.clone()),
        downloads: downloads.clone(),
        page_cache: PageCache::new(reader::PAGE_CACHE_BYTES),
        cover_cache: PageCache::new(48 * 1024 * 1024),
        cover_permits: tokio::sync::Semaphore::new(4),
        page_permits: tokio::sync::Semaphore::new(6),
        app: std::sync::Mutex::new(AppState::default()),
        scale: std::sync::atomic::AtomicU32::new(1.0f32.to_bits()),
    });
    let _ = ctx_cell.set(ctx.clone());
    let _ = ctx.view_metrics();

    prefs::load_into_ui(&ctx, &window);
    prefs::apply(&ctx);
    explorer::init(&ctx, &window);
    library::wire(&ctx, &window);
    explorer::wire(&ctx, &window);
    manga::wire(&ctx, &window);
    lists::wire(&ctx, &window);
    prefs::wire(&ctx, &window);
    reader::wire(&ctx, &window);

    library::refresh(&ctx);
    lists::refresh_history(&ctx);
    lists::refresh_updates(&ctx);
    lists::refresh_downloads(&ctx);
    downloads.start(runtime.handle());
    covers::trim(&ctx);
    if settings.update_on_startup {
        lists::update_library(&ctx, true);
    }

    #[cfg(feature = "autopilot")]
    autopilot::start(&ctx, &window);

    window.run().context("running UI")?;
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    Ok(())
}
