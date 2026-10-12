//! 原生 GUI（egui/eframe）：目录卡片墙、搜索、详情选集、下载管理与片库。
//! 网络操作全部在后台线程，通过 mpsc 通道回传消息；下载并发固定为 2。

use crate::hgapp::{detail_with_fallback, search_combined, HgClient, GENRES};
use crate::media;
use crate::model::{Chapter, Drama};
use crate::state::{EpisodeEntry, LibraryEntry, Store};
use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

const COVER_W: f32 = 112.0;
const COVER_H: f32 = 149.0;
const DOWNLOAD_WORKERS: usize = 2;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Catalog,
    Search,
    Library,
}

struct DetailState {
    drama: Drama,
    chapters: Option<Vec<Chapter>>,
    selected: HashSet<usize>,
    error: Option<String>,
}

#[derive(Clone)]
struct DownloadItem {
    drama_id: String,
    title: String,
    ep: usize,
    frac: f32,
    status: DlStatus,
    note: String,
}

#[derive(Clone, Copy, PartialEq)]
enum DlStatus {
    Active,
    Done,
    Failed,
}

enum Msg {
    CatalogPage {
        genre: String,
        dramas: Vec<Drama>,
        exhausted: bool,
    },
    SearchDone {
        query: String,
        dramas: Vec<Drama>,
        warning: Option<String>,
    },
    DetailLoaded {
        drama_id: String,
        drama: Drama,
        chapters: Result<Vec<Chapter>, String>,
    },
    Cover {
        url: String,
        rgba: Option<(Vec<u8>, usize, usize)>,
    },
    DownloadProgress {
        drama_id: String,
        ep: usize,
        frac: f32,
    },
    DownloadDone {
        drama_id: String,
        ep: usize,
        ok: bool,
        note: String,
        result: Option<(String, String, i64, String)>,
    },
    PlayReady {
        label: String,
        url: String,
        referer: String,
        key: Option<String>,
    },
    Status(String),
}

struct DownloadJob {
    store: Store,
    drama_id: String,
    series_id: String,
    chapters: Vec<Chapter>,
    ep: usize,
    out_root: PathBuf,
    quality: i64,
}

pub struct GuiApp {
    store: Store,
    client: Arc<HgClient>,
    tx: mpsc::Sender<Msg>,
    rx: mpsc::Receiver<Msg>,
    job_tx: mpsc::Sender<DownloadJob>,
    tab: Tab,
    catalog: HashMap<String, Vec<Drama>>,
    catalog_exhausted: HashSet<String>,
    catalog_loading: HashSet<String>,
    search_input: String,
    search_submitted: String,
    search_results: Vec<Drama>,
    search_busy: bool,
    detail: Option<DetailState>,
    covers: HashMap<String, Option<egui::TextureHandle>>,
    covers_pending: HashSet<String>,
    downloads: Vec<DownloadItem>,
    status: (String, std::time::Instant),
}

impl GuiApp {
    #[doc(hidden)]
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_cjk_font(&cc.egui_ctx);
        let (tx, rx) = mpsc::channel();
        let store = Store::load();
        let client = Arc::new(HgClient::new());
        let (job_tx, job_rx) = mpsc::channel::<DownloadJob>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        for _ in 0..DOWNLOAD_WORKERS {
            let job_rx = job_rx.clone();
            let tx = tx.clone();
            thread::spawn(move || loop {
                let job = job_rx.lock().ok().and_then(|guard| guard.recv().ok());
                let Some(job) = job else { return };
                run_download_job(job, tx.clone());
            });
        }
        let mut app = GuiApp {
            store,
            client,
            tx,
            rx,
            job_tx,
            tab: Tab::Catalog,
            catalog: HashMap::new(),
            catalog_exhausted: HashSet::new(),
            catalog_loading: HashSet::new(),
            search_input: String::new(),
            search_submitted: String::new(),
            search_results: Vec::new(),
            search_busy: false,
            detail: None,
            covers: HashMap::new(),
            covers_pending: HashSet::new(),
            downloads: Vec::new(),
            status: ("就绪".into(), std::time::Instant::now()),
        };
        for genre in GENRES.iter() {
            app.request_catalog_page(genre.key);
        }
        app
    }

    fn set_status(&mut self, text: impl Into<String>) {
        self.status = (text.into(), std::time::Instant::now());
    }

    fn request_catalog_page(&mut self, genre_key: &str) {
        if self.catalog_loading.contains(genre_key) || self.catalog_exhausted.contains(genre_key) {
            return;
        }
        self.catalog_loading.insert(genre_key.to_string());
        let client = self.client.clone();
        let store = self.store.clone();
        let tx = self.tx.clone();
        let genre_key = genre_key.to_string();
        let genre: Option<&'static crate::hgapp::Genre> =
            GENRES.iter().find(|g| g.key == genre_key);
        thread::spawn(move || {
            let Some(genre) = genre else { return };
            let cursor = store.feed(genre.key);
            match client.catalog_page(&store, &genre, &cursor) {
                Ok((items, next)) => {
                    let _ = tx.send(Msg::CatalogPage {
                        genre: genre.key.to_string(),
                        dramas: items,
                        exhausted: next.exhausted,
                    });
                }
                Err(err) => {
                    let _ = tx.send(Msg::Status(format!("{} 目录失败：{}", genre.name, err)));
                    let _ = tx.send(Msg::CatalogPage {
                        genre: genre.key.to_string(),
                        dramas: Vec::new(),
                        exhausted: true,
                    });
                }
            }
        });
    }

    fn request_search(&mut self) {
        let keyword = self.search_input.trim().to_string();
        if keyword.is_empty() || self.search_busy {
            return;
        }
        self.search_busy = true;
        self.search_submitted = keyword.clone();
        let client = self.client.clone();
        let tx = self.tx.clone();
        thread::spawn(move || match search_combined(&client, &keyword) {
            Ok((dramas, warning)) => {
                let _ = tx.send(Msg::SearchDone { query: keyword, dramas, warning });
            }
            Err(err) => {
                let _ = tx.send(Msg::SearchDone {
                    query: keyword,
                    dramas: Vec::new(),
                    warning: Some(err),
                });
            }
        });
    }

    fn open_detail(&mut self, drama: &Drama) {
        let drama_id = drama.id.clone();
        self.detail = Some(DetailState {
            drama: drama.clone(),
            chapters: None,
            selected: HashSet::new(),
            error: None,
        });
        let client = self.client.clone();
        let store = self.store.clone();
        let tx = self.tx.clone();
        thread::spawn(move || match detail_with_fallback(&client, &store, &drama_id) {
            Ok((fresh, chapters)) => {
                let _ = tx.send(Msg::DetailLoaded {
                    drama_id,
                    drama: fresh,
                    chapters: Ok(chapters),
                });
            }
            Err(err) => {
                let _ = tx.send(Msg::DetailLoaded {
                    drama_id,
                    drama: Drama::default(),
                    chapters: Err(err),
                });
            }
        });
    }

    fn queue_download(&mut self, episodes: Vec<usize>) {
        let Some(detail) = self.detail.as_ref() else { return };
        let Some(chapters) = detail.chapters.clone() else { return };
        let drama_id = detail.drama.id.clone();
        let series_id = detail.drama.source_id.clone();
        let title = detail.drama.title.clone();
        let out_root = self
            .store
            .library
            .get(&drama_id)
            .map(|e| PathBuf::from(&e.dir))
            .unwrap_or_else(|| {
                self.store
                    .dir
                    .join("library")
                    .join(drama_id.replace([':', '/'], "_"))
            });
        for ep in episodes {
            if self
                .downloads
                .iter()
                .any(|d| d.drama_id == drama_id && d.ep == ep && d.status == DlStatus::Active)
            {
                continue;
            }
            let existing = self.store.library.get(&drama_id).and_then(|e| {
                e.episodes
                    .iter()
                    .find(|x| x.index == ep)
                    .map(|x| PathBuf::from(&e.dir).join(format!("ep_{:03}", ep)).join(&x.file))
            });
            if existing.map(|p| p.exists()).unwrap_or(false) {
                self.set_status(format!("第{}集 已存在，跳过", ep));
                continue;
            }
            self.downloads.push(DownloadItem {
                drama_id: drama_id.clone(),
                title: title.clone(),
                ep,
                frac: 0.0,
                status: DlStatus::Active,
                note: "排队中".into(),
            });
            let _ = self.job_tx.send(DownloadJob {
                store: self.store.clone(),
                drama_id: drama_id.clone(),
                series_id: series_id.clone(),
                chapters: chapters.clone(),
                ep,
                out_root: out_root.clone(),
                quality: 0,
            });
        }
    }

    fn request_play(&mut self, ep: usize) {
        let Some(detail) = self.detail.as_ref() else { return };
        let Some(chapter) = detail.chapters.as_ref().and_then(|cs| cs.iter().find(|c| c.index == ep).cloned()) else {
            return;
        };
        let series_id = detail.drama.source_id.clone();
        let label = format!("《{}》第{}集", detail.drama.title, ep);
        let client = self.client.clone();
        let store = self.store.clone();
        let tx = self.tx.clone();
        thread::spawn(move || match media::try_resolve_media(&client, &store, &series_id, &chapter.video_id) {
            Ok(resolved) => {
                let _ = tx.send(Msg::PlayReady {
                    label,
                    url: resolved.url,
                    referer: resolved.referer,
                    key: resolved.cenc_key.map(hex::encode),
                });
            }
            Err(err) => {
                let _ = tx.send(Msg::Status(err));
            }
        });
    }

    fn cover_texture(&mut self, _ctx: &egui::Context, url: &str) -> Option<egui::TextureHandle> {
        if url.is_empty() {
            return None;
        }
        if let Some(handle) = self.covers.get(url) {
            return handle.clone();
        }
        if self.covers_pending.insert(url.to_string()) {
            let tx = self.tx.clone();
            let url = url.to_string();
            thread::spawn(move || {
                let rgba = fetch_cover(&url);
                let _ = tx.send(Msg::Cover { url, rgba });
            });
        }
        None
    }

    fn apply_msg(&mut self, ctx: &egui::Context, msg: Msg) {
        match msg {
            Msg::CatalogPage { genre, dramas, exhausted } => {
                let slot = self.catalog.entry(genre.clone()).or_default();
                crate::hgweb::merge_dramas(slot, dramas);
                self.catalog_loading.remove(&genre);
                if exhausted {
                    self.catalog_exhausted.insert(genre);
                }
            }
            Msg::SearchDone { query, dramas, warning } => {
                if query == self.search_submitted {
                    self.search_results = dramas;
                    self.search_busy = false;
                }
                if let Some(warning) = warning {
                    self.set_status(format!("搜索：{}", warning));
                }
            }
            Msg::DetailLoaded { drama_id, drama, chapters } => {
                let Some(detail) = self.detail.as_mut() else { return };
                if detail.drama.id != drama_id {
                    return;
                }
                match chapters {
                    Ok(list) => {
                        if !drama.title.is_empty() {
                            detail.drama = drama;
                        }
                        detail.selected = list.iter().map(|c| c.index).collect();
                        detail.chapters = Some(list);
                    }
                    Err(err) => detail.error = Some(err),
                }
            }
            Msg::Cover { url, rgba } => {
                self.covers_pending.remove(&url);
                let handle = rgba.and_then(|(bytes, w, h)| {
                    let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &bytes);
                    Some(ctx.load_texture("cover", image, Default::default()))
                });
                self.covers.insert(url, handle);
            }
            Msg::DownloadProgress { drama_id, ep, frac } => {
                if let Some(item) = self
                    .downloads
                    .iter_mut()
                    .find(|d| d.drama_id == drama_id && d.ep == ep && d.status == DlStatus::Active)
                {
                    item.frac = frac;
                }
            }
            Msg::DownloadDone { drama_id, ep, ok, note, result } => {
                if let Some(item) = self
                    .downloads
                    .iter_mut()
                    .find(|d| d.drama_id == drama_id && d.ep == ep && d.status == DlStatus::Active)
                {
                    item.status = if ok { DlStatus::Done } else { DlStatus::Failed };
                    if ok {
                        item.frac = 1.0;
                    }
                    item.note = note;
                }
                self.set_status(format!("第{}集 {}", ep, if ok { "完成" } else { "失败" }));
                if let Some((file, kind, quality, dir)) = result {
                    self.record_library_episode(&drama_id, ep, &file, kind, quality, &dir);
                }
            }
            Msg::PlayReady { label, url, referer, key } => {
                let player = std::env::var("GUO_MPV").unwrap_or_else(|_| "mpv".into());
                let mut cmd = std::process::Command::new(&player);
                cmd.arg(&url).arg(format!("--referrer={}", referer));
                if let Some(key) = &key {
                    cmd.arg(format!("--demuxer-lavf-o=decryption_key={}", key));
                }
                match cmd.spawn() {
                    Ok(_) => self.set_status(format!("{} → {} 已启动", label, player)),
                    Err(_) => self
                        .set_status("未找到 mpv（mpv.io 或设 GUO_MPV 指向 mpv 可执行文件），或改用下载后本地播放"),
                }
            }
            Msg::Status(text) => self.set_status(text),
        }
    }

    fn record_library_episode(
        &mut self,
        drama_id: &str,
        ep: usize,
        file: &str,
        kind: String,
        quality: i64,
        dir: &str,
    ) {
        let title = self
            .detail
            .as_ref()
            .filter(|d| d.drama.id == drama_id)
            .map(|d| d.drama.title.clone())
            .unwrap_or_default();
        let entry = self
            .store
            .library
            .entry(drama_id.to_string())
            .or_insert_with(|| LibraryEntry {
                drama_id: drama_id.to_string(),
                title,
                dir: dir.to_string(),
                entry: String::new(),
                kind: kind.clone(),
                episodes: Vec::new(),
            });
        entry.dir = dir.to_string();
        entry.kind = kind;
        entry.episodes.retain(|e| e.index != ep);
        entry.episodes.push(EpisodeEntry {
            index: ep,
            quality,
            file: file.to_string(),
        });
        entry.episodes.sort_by_key(|e| e.index);
        entry.entry = file.to_string();
        let _ = self.store.save();
    }

}

fn run_download_job(job: DownloadJob, tx: mpsc::Sender<Msg>) {
    let chapter = match job.chapters.iter().find(|c| c.index == job.ep) {
        Some(c) => c.clone(),
        None => {
            let _ = tx.send(Msg::DownloadDone {
                drama_id: job.drama_id,
                ep: job.ep,
                ok: false,
                note: "分集不存在".into(),
                result: None,
            });
            return;
        }
    };
    let client = HgClient::new();
    let resolved = match media::try_resolve_media(&client, &job.store, &job.series_id, &chapter.video_id)
    {
        Ok(m) => m,
        Err(err) => {
            let _ = tx.send(Msg::DownloadDone {
                drama_id: job.drama_id,
                ep: job.ep,
                ok: false,
                note: err,
                result: None,
            });
            return;
        }
    };
    let out_dir = job.out_root.join(format!("ep_{:03}", job.ep));
    let tx_progress = tx.clone();
    let drama_id = job.drama_id.clone();
    let ep = job.ep;
    let result = media::download_episode_progress(
        &client,
        &resolved,
        &out_dir,
        job.quality,
        &format!("第{}集", job.ep),
        &mut move |frac| {
            let _ = tx_progress.send(Msg::DownloadProgress {
                drama_id: drama_id.clone(),
                ep,
                frac,
            });
        },
    );
    match result {
        Ok((file, kind, quality)) => {
            let _ = tx.send(Msg::DownloadDone {
                drama_id: job.drama_id,
                ep: job.ep,
                ok: true,
                note: format!("{} {}p", kind, quality),
                result: Some((file, kind, quality, job.out_root.display().to_string())),
            });
        }
        Err(err) => {
            let _ = tx.send(Msg::DownloadDone {
                drama_id: job.drama_id,
                ep: job.ep,
                ok: false,
                note: err,
                result: None,
            });
        }
    }
}

/// Open a path (directory or media file) with the platform file manager or
/// default player: `open` on macOS, `explorer` on Windows, `xdg-open` on Linux.
fn open_in_system(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "explorer";
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let program = "xdg-open";
    let _ = std::process::Command::new(program).arg(path).spawn();
}

/// Fetch a cover image and decode to RGBA at 220x294. HEIC covers (the CDN
/// default) are converted with the system `sips` on macOS.
fn fetch_cover(url: &str) -> Option<(Vec<u8>, usize, usize)> {
    let response = ureq::get(url)
        .timeout(std::time::Duration::from_secs(15))
        .set("User-Agent", crate::hgapp::WEB_UA)
        .call()
        .ok()?;
    let mut bytes = Vec::new();
    use std::io::Read;
    response
        .into_reader()
        .take(8 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .ok()?;
    decode_image(&bytes)
}

fn decode_image(bytes: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
    let decoded = match image::load_from_memory(bytes) {
        Ok(img) => img,
        Err(_) => decode_via_sips(bytes)?,
    };
    let thumb = decoded.resize_to_fill(220, 294, image::imageops::FilterType::Triangle);
    Some((thumb.to_rgba8().to_vec(), 220, 294))
}

fn decode_via_sips(bytes: &[u8]) -> Option<image::DynamicImage> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let unique = format!(
        "guo-cover-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    );
    let src = std::env::temp_dir().join(format!("{}.heic", unique));
    let dst = std::env::temp_dir().join(format!("{}.jpg", unique));
    std::fs::write(&src, bytes).ok()?;
    let output = std::process::Command::new("sips")
        .args(["-s", "format", "jpeg"])
        .arg(&src)
        .arg("--out")
        .arg(&dst)
        .output();
    let _ = std::fs::remove_file(&src);
    let ok = output.as_ref().map(|o| o.status.success()).unwrap_or(false);
    let jpeg = if ok { std::fs::read(&dst).ok() } else { None };
    let _ = std::fs::remove_file(&dst);
    let jpeg = jpeg?;
    image::load_from_memory(&jpeg).ok()
}

fn install_cjk_font(ctx: &egui::Context) {
    let candidates = [
        "/System/Library/Fonts/Hiragino Sans GB.ttc",
        "/System/Library/Fonts/PingFang.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "C:/Windows/Fonts/msyh.ttc",
    ];
    for path in candidates {
        let Ok(bytes) = std::fs::read(path) else { continue };
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("cjk".into(), egui::FontData::from_owned(bytes).into());
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(list) = fonts.families.get_mut(&family) {
                list.insert(0, "cjk".into());
            }
        }
        ctx.set_fonts(fonts);
        return;
    }
}

fn status_text(status: &str) -> &'static str {
    match status {
        "finished" => "已完结",
        "ongoing" => "连载中",
        _ => "未知",
    }
}

impl eframe::App for GuiApp {
    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        let _ = self.store.save();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(msg) = self.rx.try_recv() {
            self.apply_msg(ctx, msg);
        }

        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Catalog, "目录");
                ui.selectable_value(&mut self.tab, Tab::Search, "搜索");
                ui.selectable_value(
                    &mut self.tab,
                    Tab::Library,
                    format!("片库 ({})", self.store.library.len()),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("清空封面缓存").clicked() {
                        self.covers.clear();
                        self.covers_pending.clear();
                    }
                });
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let active = self
                    .downloads
                    .iter()
                    .filter(|d| d.status == DlStatus::Active)
                    .count();
                ui.label(if active > 0 {
                    format!("⬇ {} 个下载中", active)
                } else {
                    "⬇".to_string()
                });
                ui.separator();
                if self.status.1.elapsed().as_secs() < 10 {
                    ui.label(egui::RichText::new(&self.status.0).weak());
                }
            });
        });

        if self.detail.is_some() {
            let mut close = false;
            egui::SidePanel::right("detail")
                .resizable(true)
                .default_width(380.0)
                .show(ctx, |ui| {
                    close = !self.show_detail(ui);
                });
            if close {
                self.detail = None;
            }
        }

        match self.tab {
            Tab::Catalog => self.show_catalog(ctx),
            Tab::Search => self.show_search(ctx),
            Tab::Library => self.show_library(ctx),
        }
    }
}

impl GuiApp {
    fn show_catalog(&mut self, ctx: &egui::Context) {
        let mut open: Option<Drama> = None;
        let mut more: Option<String> = None;
        let mut refresh = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("分类：");
                for genre in GENRES.iter() {
                    let count = self.catalog.get(genre.key).map(|v| v.len()).unwrap_or(0);
                    let label = format!("{} ({})", genre.name, count);
                    let busy = self.catalog_loading.contains(genre.key);
                    if ui.selectable_label(!busy, label).clicked() && count == 0 && !busy {
                        more = Some(genre.key.to_string());
                    }
                }
                if ui.button("刷新").clicked() {
                    refresh = true;
                }
            });
            ui.separator();
            egui::ScrollArea::vertical().id_salt("catalog_scroll").show(ui, |ui| {
                for genre in GENRES.iter() {
                    let dramas = match self.catalog.get(genre.key) {
                        Some(v) if !v.is_empty() => v.clone(),
                        _ => continue,
                    };
                    ui.add_space(6.0);
                    ui.heading(genre.name);
                    ui.add_space(2.0);
                    if let Some(d) = self.card_grid(ui, ctx, &dramas) {
                        open = Some(d);
                    }
                }
                if self.catalog.values().all(|v| v.is_empty()) {
                    ui.add_space(30.0);
                    ui.vertical_centered(|ui| {
                        if self.catalog_loading.is_empty() {
                            ui.label("目录为空或加载失败（点上方分类重试）");
                        } else {
                            ui.spinner();
                            ui.label("加载中…");
                        }
                    });
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    for genre in GENRES.iter() {
                        let busy = self.catalog_loading.contains(genre.key);
                        let label = if busy {
                            format!("{} …", genre.name)
                        } else if self.catalog_exhausted.contains(genre.key) {
                            format!("{} 已到底", genre.name)
                        } else {
                            format!("更多{}", genre.name)
                        };
                        if ui
                            .add_enabled(!busy, egui::Button::new(label))
                            .clicked()
                        {
                            more = Some(genre.key.to_string());
                        }
                    }
                });
            });
        });
        if let Some(genre) = more {
            self.request_catalog_page(&genre);
        }
        if refresh {
            for genre in GENRES.iter() {
                self.catalog.remove(genre.key);
                self.catalog_exhausted.remove(genre.key);
                self.request_catalog_page(genre.key);
            }
        }
        if let Some(drama) = open {
            self.open_detail(&drama);
        }
    }

    fn show_search(&mut self, ctx: &egui::Context) {
        let mut submit = false;
        let mut open: Option<Drama> = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.search_input)
                        .hint_text("输入剧名，回车搜索")
                        .desired_width(360.0),
                );
                submit = (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    || ui.button("搜索").clicked();
                if self.search_busy {
                    ui.spinner();
                }
            });
            ui.separator();
            if !self.search_submitted.is_empty() {
                ui.label(format!(
                    "“{}” 共 {} 部",
                    self.search_submitted,
                    self.search_results.len()
                ));
            }
            egui::ScrollArea::vertical().id_salt("search_scroll").show(ui, |ui| {
                let results = self.search_results.clone();
                if let Some(d) = self.card_grid(ui, ctx, &results) {
                    open = Some(d);
                }
            });
        });
        if submit {
            self.request_search();
        }
        if let Some(drama) = open {
            self.open_detail(&drama);
        }
    }

    fn show_library(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            if !self.downloads.is_empty() {
                ui.heading("本次会话下载");
                egui::ScrollArea::vertical().id_salt("downloads_scroll")
                    .max_height(200.0)
                    .show(ui, |ui| {
                        let rows: Vec<DownloadItem> = self.downloads.iter().rev().cloned().collect();
                        egui::Grid::new("downloads")
                            .num_columns(4)
                            .striped(true)
                            .show(ui, |ui| {
                                for item in rows {
                                    ui.label(format!("《{}》", item.title));
                                    ui.label(format!("第{}集", item.ep));
                                    match item.status {
                                        DlStatus::Active => {
                                            ui.add(
                                                egui::ProgressBar::new(item.frac.clamp(0.0, 1.0))
                                                    .show_percentage()
                                                    .desired_height(14.0),
                                            );
                                        }
                                        DlStatus::Done => {
                                            ui.colored_label(
                                                egui::Color32::from_rgb(80, 180, 90),
                                                format!("完成 {}", item.note),
                                            );
                                        }
                                        DlStatus::Failed => {
                                            ui.colored_label(
                                                egui::Color32::from_rgb(220, 90, 80),
                                                format!("失败：{}", item.note),
                                            );
                                        }
                                    }
                                    ui.end_row();
                                }
                            });
                    });
                ui.separator();
            }
            ui.heading("已下载剧目");
            if self.store.library.is_empty() {
                ui.add_space(12.0);
                ui.vertical_centered(|ui| ui.label("本地片库为空"));
                return;
            }
            let entries: Vec<LibraryEntry> = self.store.library.values().cloned().collect();
            egui::ScrollArea::vertical().id_salt("library_scroll").show(ui, |ui| {
                for entry in entries {
                    ui.horizontal(|ui| {
                        ui.label(format!("《{}》", entry.title));
                        ui.weak(format!("{}集", entry.episodes.len()));
                        if ui.small_button("打开目录").clicked() {
                            open_in_system(&PathBuf::from(&entry.dir));
                        }
                        ui.separator();
                        for ep in &entry.episodes {
                            let path = PathBuf::from(&entry.dir)
                                .join(format!("ep_{:03}", ep.index))
                                .join(&ep.file);
                            if ui
                                .small_button(format!("▶{}", ep.index))
                                .on_hover_text(path.display().to_string())
                                .clicked()
                            {
                                open_in_system(&path);
                            }
                        }
                    });
                }
            });
        });
    }

    fn show_detail(&mut self, ui: &mut egui::Ui) -> bool {
        let Some(detail) = self.detail.as_ref() else { return false };
        let drama = detail.drama.clone();
        let chapters = detail.chapters.clone();
        let selected = detail.selected.clone();
        let error = detail.error.clone();
        let mut close = false;
        let mut toggled: Option<usize> = None;
        let mut download_eps: Option<Vec<usize>> = None;
        let mut play_ep: Option<usize> = None;
        let mut clear_selection = false;
        let mut select_all = false;

        egui::ScrollArea::vertical().id_salt("detail_scroll").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(&drama.title);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("✕").clicked() {
                        close = true;
                    }
                });
            });
            ui.horizontal(|ui| {
                let texture = self.cover_texture(ui.ctx(), &drama.cover);
                match texture {
                    Some(handle) => {
                        ui.add(egui::Image::new(egui::load::SizedTexture::new(
                            &handle,
                            [COVER_W * 1.4, COVER_H * 1.4],
                        )));
                    }
                    None => {
                        let (rect, _) =
                            ui.allocate_exact_size([COVER_W * 1.4, COVER_H * 1.4].into(), egui::Sense::hover());
                        ui.painter().rect_filled(rect, 4.0, egui::Color32::from_gray(40));
                    }
                }
                ui.vertical(|ui| {
                    ui.label(format!("分类：{}", drama.category));
                    ui.label(format!("备注：{}", drama.remark));
                    ui.label(format!("状态：{}", status_text(&drama.release_status)));
                    if !drama.score.is_empty() {
                        ui.label(format!("评分：{}", drama.score));
                    }
                    if !drama.heat.is_empty() {
                        ui.label(format!("热度：{}", drama.heat));
                    }
                });
            });
            ui.separator();
            ui.label(egui::RichText::new("简介").strong());
            ui.label(if drama.intro.is_empty() { "（无）" } else { &drama.intro });
            ui.separator();

            match chapters {
                None => {
                    if let Some(error) = error {
                        ui.colored_label(egui::Color32::from_rgb(220, 90, 80), error);
                    } else {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("载入分集…");
                        });
                    }
                }
                Some(chapters) => {
                    ui.label(format!("共 {} 集（点选要下载的集数）", chapters.len()));
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        for chapter in &chapters {
                            let picked = selected.contains(&chapter.index);
                            let label = egui::RichText::new(format!("{}", chapter.index)).size(11.0);
                            if ui
                                .add_sized(
                                    [30.0, 20.0],
                                    egui::widgets::Button::new(label).selected(picked),
                                )
                                .clicked()
                            {
                                toggled = Some(chapter.index);
                            }
                        }
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let picked_count = self
                            .detail
                            .as_ref()
                            .map(|d| d.selected.len())
                            .unwrap_or(0);
                        if ui.button("全不选").clicked() {
                            clear_selection = true;
                        }
                        if ui.button("全选").clicked() {
                            select_all = true;
                        }
                        if ui
                            .button(format!("⬇ 下载选中（{}）", picked_count))
                            .clicked()
                        {
                            let mut eps: Vec<usize> = selected.iter().copied().collect();
                            eps.sort_unstable();
                            download_eps = Some(eps);
                        }
                        if ui.button("⬇ 全部下载").clicked() {
                            download_eps = Some(chapters.iter().map(|c| c.index).collect());
                        }
                        if ui.button("▶ 播放第1集").clicked() {
                            play_ep = Some(1);
                        }
                    });
                }
            }
        });
        if let Some(index) = toggled {
            if let Some(detail) = self.detail.as_mut() {
                if detail.selected.contains(&index) {
                    detail.selected.remove(&index);
                } else {
                    detail.selected.insert(index);
                }
            }
        }
        if clear_selection || select_all {
            if let Some(detail) = self.detail.as_mut() {
                if clear_selection {
                    detail.selected.clear();
                } else if let Some(chapters) = &detail.chapters {
                    detail.selected = chapters.iter().map(|c| c.index).collect();
                }
            }
        }
        if let Some(eps) = download_eps {
            self.queue_download(eps);
        }
        if let Some(ep) = play_ep {
            self.request_play(ep);
        }
        !close
    }
}

// ---------------------------------------------------------------------------
// Card grid

impl GuiApp {
    fn card_grid(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        dramas: &[Drama],
    ) -> Option<Drama> {
        if dramas.is_empty() {
            return None;
        }
        let columns = ((ui.available_width() / (COVER_W + 10.0)).floor() as usize).clamp(1, 12);
        let mut open: Option<Drama> = None;
        for row in dramas.chunks(columns) {
            let mut picked: Option<Drama> = None;
            ui.horizontal(|ui| {
                for drama in row {
                    if let Some(d) = self.card(ui, ctx, drama) {
                        picked = Some(d);
                    }
                }
            });
            if picked.is_some() {
                open = picked;
            }
        }
        open
    }

    fn card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, drama: &Drama) -> Option<Drama> {
        let texture = self.cover_texture(ctx, &drama.cover);
        let mut clicked = false;
        ui.vertical(|ui| {
            let (rect, _) =
                ui.allocate_exact_size([COVER_W, COVER_H].into(), egui::Sense::hover());
            let painter = ui.painter();
            if let Some(handle) = &texture {
                painter.image(
                    handle.id(),
                    rect,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                painter.rect_filled(rect, 4.0, egui::Color32::from_gray(38));
                let label = if self.covers_pending.contains(&drama.cover) {
                    "…"
                } else {
                    "无封面"
                };
                painter.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    label,
                    egui::FontId::proportional(12.0),
                    egui::Color32::from_gray(100),
                );
            }
            let card = ui.interact(rect, egui::Id::new(&drama.id), egui::Sense::click());
            card.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &drama.title)
            });
            if card.clicked() {
                clicked = true;
            }
            ui.add_sized(
                [COVER_W, 16.0],
                egui::Label::new(egui::RichText::new(&drama.title).size(12.0)).truncate(),
            );
            ui.add_sized(
                [COVER_W, 14.0],
                egui::Label::new(
                    egui::RichText::new(if drama.remark.is_empty() {
                        &drama.category
                    } else {
                        &drama.remark
                    })
                    .size(10.5)
                    .weak(),
                )
                .truncate(),
            );
        });
        if clicked {
            return Some(drama.clone());
        }
        None
    }
}

/// Entry point used by the `guo-gui` binary.
pub fn run() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 760.0])
            .with_min_inner_size([720.0, 480.0]),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "guo · 红果短剧",
        options,
        Box::new(|cc| Ok(Box::new(GuiApp::new(cc)))),
    );
}
