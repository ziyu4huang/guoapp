//! 原生 GUI（egui/eframe）：目录卡片墙、搜索、详情选集、下载管理与片库。
//! 网络操作全部在后台线程，通过 mpsc 通道回传消息；下载并发固定为 2。

use crate::hgapp::{detail_with_fallback, search_combined, HgClient, GENRES};
use crate::media;
use crate::model::{Chapter, Drama};
use crate::state::{EpisodeEntry, LibraryEntry, Store};
use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
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

/// 封面缓存 + 抓取队列：4 个 worker 消费（一页 18+ 张不再同时开线程），
/// 纹理按 URL 缓存（滚动/换页零重复解码）。
struct CoverCache {
    map: HashMap<String, Option<egui::TextureHandle>>,
    pending: HashSet<String>,
    queue_tx: mpsc::Sender<String>,
}

const COVER_WORKERS: usize = 4;

impl CoverCache {
    fn new(msg_tx: mpsc::Sender<Msg>) -> Self {
        let (queue_tx, queue_rx) = mpsc::channel::<String>();
        let queue_rx = Arc::new(Mutex::new(queue_rx));
        for _ in 0..COVER_WORKERS {
            let queue_rx = queue_rx.clone();
            let msg_tx = msg_tx.clone();
            thread::spawn(move || loop {
                let url = queue_rx.lock().ok().and_then(|g| g.recv().ok());
                let Some(url) = url else { return };
                let rgba = fetch_cover(&url);
                let _ = msg_tx.send(Msg::Cover { url, rgba });
            });
        }
        CoverCache { map: HashMap::new(), pending: HashSet::new(), queue_tx }
    }

    /// 命中缓存返回纹理；未命中入队抓取并返回 None（卡片画「…」占位）。
    fn texture(&mut self, url: &str) -> Option<egui::TextureHandle> {
        if url.is_empty() {
            return None;
        }
        if let Some(handle) = self.map.get(url) {
            return handle.clone();
        }
        if self.pending.insert(url.to_string()) {
            let _ = self.queue_tx.send(url.to_string());
        }
        None
    }

    /// 后台抓取完成：落地纹理（下次帧即显示，配合 update 的 repaint 触发）。
    fn deliver(&mut self, ctx: &egui::Context, url: String, rgba: Option<(Vec<u8>, usize, usize)>) {
        self.pending.remove(&url);
        let handle = rgba.and_then(|(bytes, w, h)| {
            let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &bytes);
            Some(ctx.load_texture("cover", image, Default::default()))
        });
        self.map.insert(url, handle);
    }

    fn clear(&mut self) {
        self.map.clear();
        self.pending.clear();
    }
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
        sort: CatalogSort,
        dramas: Vec<Drama>,
        /// 分页游标：必须存回 store，否则「更多×」永远重抓第一页。
        cursor: crate::state::FeedCursor,
    },
    /// 目录页加载失败：只解除 busy 标记（可重试），不标已到底。
    CatalogFailed {
        genre: String,
        sort: CatalogSort,
        err: String,
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
        drama_id: String,
        drama_title: String,
        ep: usize,
        total: usize,
        url: String,
        referer: String,
        key: Option<String>,
        /// 请求代数：request_play 每次递增，回包时对不上号 = 过期请求
        /// （连点/连播与手动点击并发时后完成的 resolve 不得抢播）。
        seq: u64,
    },
    /// 播放器报告当前文件结束（reason: eof=自然播完 / stop|quit=我们自己换集退出 / 其余=异常）。
    PlayerEnded {
        reason: String,
    },
    /// 后台捕获到的播放器窗口/设定快照（存 state.json，下次启动恢复）。
    PlayerPrefs(crate::player::PlayerPrefs),
    /// 播放器载入完成了一个文件（含连播自动切换的下一集）——GUI 借此推进
    /// 「确保下一集已预载」链。
    PlayerFileLoaded,
    /// 下一集取流完成，可 append 进播放列表（连播预载）。
    AppendReady {
        drama_id: String,
        ep: usize,
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

/// 目录服务端排序（select_items.sort）。键=API 值，label=界面文案。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CatalogSort {
    Latest,
    Hot,
    MostViewed,
}

impl CatalogSort {
    fn key(self) -> &'static str {
        match self {
            CatalogSort::Latest => crate::hgapp::SORT_LATEST,
            CatalogSort::Hot => crate::hgapp::SORT_HOT,
            CatalogSort::MostViewed => crate::hgapp::SORT_MOST_VIEWED,
        }
    }
    fn label(self) -> &'static str {
        match self {
            CatalogSort::Latest => "最新上线",
            CatalogSort::Hot => "最热门",
            CatalogSort::MostViewed => "最多观看",
        }
    }
    fn all() -> [CatalogSort; 3] {
        [CatalogSort::Latest, CatalogSort::Hot, CatalogSort::MostViewed]
    }
    fn from_key(key: &str) -> Option<CatalogSort> {
        CatalogSort::all().into_iter().find(|s| s.key() == key)
    }
}

/// 目录缓存/游标复合键：排序切换后各自独立缓存与分页。
fn cat_key(genre_key: &str, sort: CatalogSort) -> String {
    format!("{genre_key}@{}", sort.key())
}

/// 正在串流播放的剧集（详情面板标记 + 下一集边界）。
#[derive(Clone)]
struct NowPlaying {
    drama_id: String,
    drama_title: String,
    ep: usize,
    total: usize,
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
    /// 目录排序（存 state.json，跨启动记忆）。
    catalog_sort: CatalogSort,
    search_input: String,
    search_submitted: String,
    search_results: Vec<Drama>,
    search_busy: bool,
    detail: Option<DetailState>,
    cover_cache: CoverCache,
    downloads: Vec<DownloadItem>,
    status: (String, std::time::Instant),
    /// 串流播放单实例会话（None=未启动/已退出）。
    player: Option<crate::player::PlayerSession>,
    /// 播放请求代数（防过期 resolve 回包串台）。
    play_seq: u64,
    /// 已预载进 mpv 播放列表的下一集（drama_id, ep）——当前集播完时 mpv
    /// 内部直接切换，无黑屏等待。
    appended_next: Option<(String, usize)>,
    /// 正在后台解析的「下一集」集号（防重复解析）。
    append_in_flight: Option<usize>,
    now_playing: Option<NowPlaying>,
    /// 播完自动接下一集。
    autoplay_next: bool,
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
        let cover_cache = CoverCache::new(tx.clone());
        let catalog_sort =
            CatalogSort::from_key(&store.state.catalog_sort).unwrap_or(CatalogSort::Latest);
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
            cover_cache,
            downloads: Vec::new(),
            status: ("就绪".into(), std::time::Instant::now()),
            player: None,
            play_seq: 0,
            appended_next: None,
            append_in_flight: None,
            now_playing: None,
            autoplay_next: true,
            catalog_sort,
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
        let key = cat_key(genre_key, self.catalog_sort);
        if self.catalog_loading.contains(&key) || self.catalog_exhausted.contains(&key) {
            return;
        }
        self.catalog_loading.insert(key);
        let client = self.client.clone();
        let store = self.store.clone();
        let tx = self.tx.clone();
        let genre_key = genre_key.to_string();
        let sort = self.catalog_sort;
        let genre: Option<&'static crate::hgapp::Genre> =
            GENRES.iter().find(|g| g.key == genre_key);
        thread::spawn(move || {
            let Some(genre) = genre else { return };
            let cursor = store.feed(&cat_key(genre.key, sort));
            match client.catalog_page(&store, &genre, &cursor, sort.key()) {
                Ok((items, next)) => {
                    let _ = tx.send(Msg::CatalogPage {
                        genre: genre.key.to_string(),
                        sort,
                        dramas: items,
                        cursor: next,
                    });
                }
                Err(err) => {
                    let _ = tx.send(Msg::Status(format!("{} 目录失败：{}", genre.name, err)));
                    let _ = tx.send(Msg::CatalogFailed {
                        genre: genre.key.to_string(),
                        sort,
                        err,
                    });
                }
            }
        });
    }

    /// 切换目录排序：记忆偏好，清空当前展示（各排序键独立缓存），重拉第一页。
    fn set_catalog_sort(&mut self, sort: CatalogSort) {
        if sort == self.catalog_sort {
            return;
        }
        self.catalog_sort = sort;
        self.store.state.catalog_sort = sort.key().to_string();
        let _ = self.store.save();
        self.catalog_loading.clear();
        self.catalog_exhausted.clear();
        for genre in GENRES.iter() {
            self.request_catalog_page(genre.key);
        }
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

    /// 关窗时刻的同步捕快（UI 线程短暂阻塞一次 IPC 查询，毫秒级）：捕完落盘。
    fn capture_player_prefs(&mut self) {
        let Some(s) = self.player.as_mut() else { return };
        if !s.process_alive() {
            return;
        }
        let prefs = s.capture_prefs();
        if prefs != crate::player::PlayerPrefs::default() {
            self.store.state.player_prefs = prefs;
            let _ = self.store.save();
        }
    }

    /// 确保「下一集」已在后台解析并 append 进 mpv 播放列表——当前集自然播完
    /// 时 mpv 内部直接切换（零黑屏），这是连播连续感的根基。幂等：已预载/
    /// 已在解析/没有下一集/关连播时不动。每次集开播（PlayReady/FileLoaded）调用。
    fn ensure_next_appended(&mut self) {
        let Some(np) = self.now_playing.as_ref() else { return };
        if !self.autoplay_next {
            return;
        }
        let next = np.ep + 1;
        if next > np.total
            || self.appended_next.as_ref().is_some_and(|(_, n)| *n == next)
            || self.append_in_flight == Some(next)
        {
            return;
        }
        let Some(detail) = self.detail.as_ref() else { return };
        if detail.drama.id != np.drama_id {
            return;
        }
        let Some(chapter) = detail
            .chapters
            .as_ref()
            .and_then(|cs| cs.iter().find(|c| c.index == next).cloned())
        else {
            return;
        };
        let series_id = detail.drama.source_id.clone();
        let drama_id = np.drama_id.clone();
        let client = self.client.clone();
        let store = self.store.clone();
        let tx = self.tx.clone();
        self.append_in_flight = Some(next);
        thread::spawn(move || match media::try_resolve_media(&client, &store, &series_id, &chapter.video_id) {
            Ok(resolved) => {
                let _ = tx.send(Msg::AppendReady {
                    drama_id,
                    ep: next,
                    url: resolved.url,
                    referer: resolved.referer,
                    key: resolved.cenc_key.map(hex::encode),
                });
            }
            Err(err) => {
                let _ = tx.send(Msg::Status(format!("下一集预载失败（播完后再取）：{}", err)));
            }
        });
    }

    fn request_play(&mut self, ep: usize) {
        let Some(detail) = self.detail.as_ref() else { return };
        let Some(chapter) = detail.chapters.as_ref().and_then(|cs| cs.iter().find(|c| c.index == ep).cloned()) else {
            return;
        };
        let series_id = detail.drama.source_id.clone();
        let drama_id = detail.drama.id.clone();
        let drama_title = detail.drama.title.clone();
        let total = detail.chapters.as_ref().map(|cs| cs.len()).unwrap_or(ep);
        let client = self.client.clone();
        let store = self.store.clone();
        let tx = self.tx.clone();
        self.play_seq += 1;
        let seq = self.play_seq;
        self.set_status(format!("第{}集 取流中…", ep));
        thread::spawn(move || match media::try_resolve_media(&client, &store, &series_id, &chapter.video_id) {
            Ok(resolved) => {
                let _ = tx.send(Msg::PlayReady {
                    drama_id,
                    drama_title,
                    ep,
                    total,
                    url: resolved.url,
                    referer: resolved.referer,
                    key: resolved.cenc_key.map(hex::encode),
                    seq,
                });
            }
            Err(err) => {
                let _ = tx.send(Msg::Status(err));
            }
        });
    }

    fn apply_msg(&mut self, ctx: &egui::Context, msg: Msg) {
        match msg {
            Msg::CatalogPage { genre, sort, dramas, cursor } => {
                let key = cat_key(&genre, sort);
                let slot = self.catalog.entry(key.clone()).or_default();
                crate::hgweb::merge_dramas(slot, dramas);
                // 服务端排序是「基准+掺推荐」，同一排序两次请求顺序会漂
                // （实测 2026-10-13）——载入集内按观看数做客户端稳定排序，
                // 顯示順序才確定。最新上线保持 API 原序。
                match sort {
                    CatalogSort::MostViewed | CatalogSort::Hot => slot.sort_by(|a, b| {
                        crate::model::count_value(&b.views)
                            .total_cmp(&crate::model::count_value(&a.views))
                    }),
                    CatalogSort::Latest => {}
                }
                self.catalog_loading.remove(&key);
                self.store.set_feed(&key, cursor);
                let _ = self.store.save();
                if self.store.feed(&key).exhausted {
                    self.catalog_exhausted.insert(key);
                }
            }
            Msg::CatalogFailed { genre, sort, err } => {
                self.catalog_loading.remove(&cat_key(&genre, sort));
                self.set_status(format!("{} 目录失败（可点「更多」重试）：{}", genre, err));
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
                            // 详情接口的 cover/intro 常为空——空字段保留卡片原有值，
                            // 不能整只覆盖（否则详情封面永远灰块、简介永远「(无)」）。
                            let mut merged = drama;
                            if merged.cover.is_empty() {
                                merged.cover = detail.drama.cover.clone();
                            }
                            if merged.intro.is_empty() {
                                merged.intro = detail.drama.intro.clone();
                            }
                            if merged.remark.is_empty() {
                                merged.remark = detail.drama.remark.clone();
                            }
                            detail.drama = merged;
                        }
                        detail.selected = list.iter().map(|c| c.index).collect();
                        detail.chapters = Some(list);
                    }
                    Err(err) => detail.error = Some(err),
                }
            }
            Msg::Cover { url, rgba } => {
                self.cover_cache.deliver(ctx, url, rgba);
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
            Msg::PlayReady { drama_id, drama_title, ep, total, url, referer, key, seq } => {
                // 过期回包丢弃：用户连点两集/连播与手动点击并发时，只有最后
                // 一次请求的 resolve 结果允许上台（后完成抢播 = 播错集）。
                if seq != self.play_seq {
                    return;
                }
                let item = crate::player::PlayItem {
                    url,
                    referer,
                    key,
                    title: format!("《{}》第{}集", drama_title, ep),
                };
                // 手动换集：清掉播放列表里预载的旧「下一集」（防止播完错接），
                // 并复位预载状态——新的下一集由本集开播后重新预载。
                self.appended_next = None;
                let _ = self.player.as_mut().map(|s| s.playlist_clear());
                // 单实例：会话还活着且 IPC 可达 → 直接换集（不闪新窗口）。
                let reuse = match self.player.as_mut() {
                    Some(s) => {
                        if s.process_alive() && s.ipc_ready() {
                            s.load(&item).is_ok()
                        } else {
                            false
                        }
                    }
                    None => false,
                };
                if reuse {
                    self.set_status(format!("▶ 正在播放：{}", item.title));
                } else {
                    if let Some(mut old) = self.player.take() {
                        old.quit();
                    }
                    let (kind, program) = crate::player::resolve_player();
                    let ipc = crate::player::ipc_path();
                    let log_path = self.store.dir.join("mpv-last.log");
                    match crate::player::PlayerSession::launch(
                        kind,
                        &program,
                        &item,
                        &ipc,
                        &log_path,
                        true,
                        &self.store.state.player_prefs,
                    ) {
                        Ok(session) => {
                            // 事件线程：end-file eof 驱动「连播/下一集」（idle 会话
                            // 下 eof 照常发事件）；开播/播完各捕一次窗口与设定快照；
                            // 停机旗随会话生灭（quit/drop 置 false，旧 reader 自退）。
                            let running = session.running_flag();
                            let tx_events = self.tx.clone();
                            let ipc_for_reader = session.ipc.clone();
                            let ipc_for_call = ipc_for_reader.clone();
                            let pid = session.child_id();
                            thread::spawn(move || {
                                // 给播放器一点启动时间（event_loop 自带断线重连）。
                                thread::sleep(std::time::Duration::from_millis(600));
                                // snapshot 持有自己的 tx 克隆（避免借住 tx_events，
                                // 挡住 event_loop 闭包的 move 捕获）。
                                let tx_snap = tx_events.clone();
                                let mut snapshot = move |ipc: &Path| {
                                    let tx2 = tx_snap.clone();
                                    let ipc2 = ipc.to_path_buf();
                                    thread::spawn(move || {
                                        let p =
                                            crate::player::PlayerSession::capture_ipc_prefs(&ipc2, pid);
                                        let _ = tx2.send(Msg::PlayerPrefs(p));
                                    });
                                };
                                crate::player::event_loop(&ipc_for_call, &running, move |ev| {
                                    let msg = match ev {
                                        crate::player::PlayerEvent::Ended => {
                                            snapshot(&ipc_for_reader);
                                            Some(Msg::PlayerEnded { reason: "eof".into() })
                                        }
                                        crate::player::PlayerEvent::Failed(reason) => {
                                            Some(Msg::PlayerEnded { reason })
                                        }
                                        crate::player::PlayerEvent::FileLoaded => {
                                            snapshot(&ipc_for_reader);
                                            Some(Msg::PlayerFileLoaded)
                                        }
                                        // 换集/启动类事件：GUI 不需要（标题等已随命令下发）
                                        crate::player::PlayerEvent::Replaced => None,
                                        crate::player::PlayerEvent::Started => None,
                                    };
                                    match msg {
                                        Some(m) => tx_events.send(m).is_ok(),
                                        None => true,
                                    }
                                });
                            });
                            self.player = Some(session);
                            self.set_status(format!("▶ 正在播放：{}", item.title));
                        }
                        Err(e) => {
                            let log_path = self.store.dir.join("mpv-last.log");
                            let tail = crate::player::tail_lines(&log_path, 2);
                            let hint = if tail.is_empty() {
                                format!("（未安装？参考 README 安装 mpv，或设 GUO_MPV 指向可执行文件）")
                            } else {
                                format!("（日志尾行：{}）", tail)
                            };
                            self.set_status(format!("启动播放器失败：{} {}", e, hint));
                        }
                    }
                }
                if self.player.is_some() {
                    // 每部剧各自的播放记录（互不共用）
                    let rec = self.store.state.watch.entry(drama_id.clone()).or_default();
                    rec.last_ep = ep;
                    rec.updated_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let _ = self.store.save();
                    self.now_playing = Some(NowPlaying { drama_id, drama_title, ep, total });
                    self.ensure_next_appended();
                }
            }
            Msg::PlayerFileLoaded => {
                // 每个文件真正载入（含连播自动切换的下一集）后确保再下一集已预载
                self.ensure_next_appended();
            }
            Msg::AppendReady { drama_id, ep, url, referer, key } => {
                self.append_in_flight = None;
                // 上台守卫：必须仍是「正在播 ep-1」的同剧目会话，且连播还开着、
                // 播放器还活着——手动换集/关连播/换剧后回来的过期预载一律丢弃。
                let ok = self
                    .now_playing
                    .as_ref()
                    .is_some_and(|np| {
                        np.drama_id == drama_id && np.ep + 1 == ep && self.autoplay_next
                    });
                if !ok {
                    return;
                }
                let Some(s) = self.player.as_ref() else { return };
                if !s.ipc_ready() {
                    return;
                }
                let np = self.now_playing.as_ref().unwrap();
                let item = crate::player::PlayItem {
                    url,
                    referer,
                    key,
                    title: format!("《{}》第{}集", np.drama_title, ep),
                };
                if s.append(&item).is_ok() {
                    self.appended_next = Some((drama_id, ep));
                    self.set_status(format!("已预载第{}集，播完无缝接上", ep));
                }
            }
            Msg::PlayerEnded { reason } => {
                // 判定收口在 player::autoplay_next_ep（纯函数 + 测试）：
                // 只有 eof + 连播开 + 还有下一集才推进；stop/quit/redirect 是
                // 我们自己 loadfile 换集/退出引起的，绝不推进。
                let request = self
                    .now_playing
                    .as_ref()
                    .and_then(|np| {
                        crate::player::autoplay_next_ep(&reason, self.autoplay_next, np.ep, np.total)
                    });
                if let Some(next_ep) = request {
                    // 已预载 → mpv 正在内部切换（或即将切），不需要再取流；
                    // 状态与「正在播」标记就地推进，等 PlayerFileLoaded 补预载再下一集。
                    let preloaded = self
                        .appended_next
                        .as_ref()
                        .is_some_and(|(did, n)| {
                            *n == next_ep
                                && self
                                    .now_playing
                                    .as_ref()
                                    .is_some_and(|np| np.drama_id == *did)
                        });
                    if preloaded {
                        if let Some(np) = self.now_playing.as_mut() {
                            np.ep = next_ep;
                        }
                        if let Some(np) = self.now_playing.as_ref() {
                            if let Some(s) = self.player.as_ref() {
                                let title = format!("《{}》第{}集", np.drama_title, next_ep);
                                let _ = s.set_title(&title);
                            }
                        }
                        self.set_status(format!("第{}集 播完，无缝接第{}集", next_ep - 1, next_ep));
                    } else {
                        // 未预载（解析慢/关过连播又开/预载失败）：退回取流路径
                        self.set_status(format!("第{}集 播完，自动接第{}集…", next_ep - 1, next_ep));
                        self.request_play(next_ep);
                    }
                } else if reason == "eof" {
                    // 关着连播却还有预载条目 → 立刻丢弃，防 mpv 自动接播
                    if self.appended_next.take().is_some() {
                        if let Some(s) = self.player.as_ref() {
                            let _ = s.playlist_clear();
                        }
                    }
                    let label = self
                        .now_playing
                        .as_ref()
                        .map(|np| format!("第{}集 播完（共{}集）", np.ep, np.total))
                        .unwrap_or_else(|| "播放结束".into());
                    self.set_status(label);
                } else if reason != "stop" && reason != "quit" && reason != "redirect" {
                    self.set_status(format!("播放中断：{}（可重试同一集；日志见数据目录 mpv-last.log）", reason));
                }
            }
            Msg::PlayerPrefs(prefs) => {
                // 只在有实质内容时落盘（IPC 全挂=全 Default，别把上一份记忆洗掉）
                if prefs != crate::player::PlayerPrefs::default() {
                    self.store.state.player_prefs = prefs;
                    let _ = self.store.save();
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
        // 关窗前最后一刻捕一次播放器设定（窗口位置/大小/音量…），拖完窗直接
        // 关掉也不会丢。捕完不拦 close（照常退出，Drop 会收掉播放器进程）。
        if ctx.input(|i| i.viewport().close_requested()) {
            self.capture_player_prefs();
        }
        let mut got_msg = false;
        while let Ok(msg) = self.rx.try_recv() {
            self.apply_msg(ctx, msg);
            got_msg = true;
        }
        // 后台线程送到的消息（封面/进度/播放事件）必须立即上屏：
        // egui 无输入事件时在睡觉，不触发重绘 = 封面要等鼠标动了才出现。
        if got_msg {
            ctx.request_repaint();
        }
        // 还有后台工作在飞时保持低频帧循环
        if !self.cover_cache.pending.is_empty()
            || self.downloads.iter().any(|d| d.status == DlStatus::Active)
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(300));
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
                        self.cover_cache.clear();
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
                // 常驻显示串流播放状态（不随 10s 状态条过期）
                let player_alive = self
                    .player
                    .as_mut()
                    .map(|s| s.process_alive())
                    .unwrap_or(false);
                if let Some(np) = &self.now_playing {
                    if player_alive {
                        ui.colored_label(
                            egui::Color32::from_rgb(255, 200, 90),
                            format!("▶ 《{}》第{}集", np.drama_title, np.ep),
                        );
                        ui.separator();
                    }
                }
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
        // 字段级拆借:cover_cache 可变、catalog 只读——同一闭包内共存,免整帧 clone
        let Self {
            cover_cache,
            catalog,
            catalog_loading,
            catalog_exhausted,
            catalog_sort,
            ..
        } = self;
        let sort = *catalog_sort;
        let mut open: Option<Drama> = None;
        let mut more: Option<String> = None;
        let mut refresh = false;
        let mut switch_sort: Option<CatalogSort> = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("排序：");
                for s in CatalogSort::all() {
                    if ui.selectable_label(sort == s, s.label()).clicked() && sort != s {
                        switch_sort = Some(s);
                    }
                }
                ui.separator();
                ui.label("分类：");
                for genre in GENRES.iter() {
                    let count = catalog.get(&cat_key(genre.key, sort)).map(|v| v.len()).unwrap_or(0);
                    let label = format!("{} ({})", genre.name, count);
                    let busy = catalog_loading.contains(&cat_key(genre.key, sort));
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
                    let Some(dramas) = catalog.get(&cat_key(genre.key, sort)) else { continue };
                    if dramas.is_empty() {
                        continue;
                    }
                    ui.add_space(6.0);
                    ui.heading(genre.name);
                    ui.add_space(2.0);
                    if let Some(d) = cover_cache.card_grid(ui, dramas) {
                        open = Some(d);
                    }
                }
                if catalog
                    .iter()
                    .filter(|(k, _)| k.ends_with(&format!("@{}", sort.key())))
                    .all(|(_, v)| v.is_empty())
                {
                    ui.add_space(30.0);
                    ui.vertical_centered(|ui| {
                        if catalog_loading.is_empty() {
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
                        let gkey = cat_key(genre.key, sort);
                        let busy = catalog_loading.contains(&gkey);
                        let label = if busy {
                            format!("{} …", genre.name)
                        } else if catalog_exhausted.contains(&gkey) {
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
        if let Some(sort) = switch_sort {
            self.set_catalog_sort(sort);
        }
        if let Some(genre) = more {
            self.request_catalog_page(&genre);
        }
        if refresh {
            for genre in GENRES.iter() {
                self.catalog.remove(&cat_key(genre.key, self.catalog_sort));
                self.catalog_exhausted
                    .remove(&cat_key(genre.key, self.catalog_sort));
                self.request_catalog_page(genre.key);
            }
        }
        if let Some(drama) = open {
            self.open_detail(&drama);
        }
    }

    fn show_search(&mut self, ctx: &egui::Context) {
        let Self { cover_cache, search_input, search_busy, search_submitted, search_results, .. } = self;
        let mut submit = false;
        let mut open: Option<Drama> = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                let response = ui.add(
                    egui::TextEdit::singleline(search_input)
                        .hint_text("输入剧名，回车搜索")
                        .desired_width(360.0),
                );
                submit = (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    || ui.button("搜索").clicked();
                if *search_busy {
                    ui.spinner();
                }
            });
            ui.separator();
            if !search_submitted.is_empty() {
                ui.label(format!(
                    "“{}” 共 {} 部",
                    search_submitted,
                    search_results.len()
                ));
            }
            egui::ScrollArea::vertical().id_salt("search_scroll").show(ui, |ui| {
                if let Some(d) = cover_cache.card_grid(ui, search_results) {
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
                        let rows: Vec<&DownloadItem> = self.downloads.iter().rev().collect();
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
            let entries: Vec<&LibraryEntry> = self.store.library.values().collect();
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
        // 字段级拆借:chapters/selected 全程只读引用,不再每帧整份克隆
        let Self { detail, cover_cache, now_playing, autoplay_next, .. } = self;
        let Some(detail) = detail.as_ref() else { return false };
        let drama = &detail.drama;
        let chapters = detail.chapters.as_ref();
        let selected = &detail.selected;
        let error = detail.error.as_ref();
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
                let texture = cover_cache.texture(&drama.cover);
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
                    // 本剧自己的播放记录（每部独立）：<= 上次播放集 = 已看
                    let watch_last = self
                        .store
                        .state
                        .watch
                        .get(&drama.id)
                        .map(|w| w.last_ep)
                        .unwrap_or(0);
                    ui.label(format!("共 {} 集（点击集数即播放 · 右键勾选要下载的）", chapters.len()));
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        for chapter in chapters.iter() {
                            let picked = selected.contains(&chapter.index);
                            // 正在串流播放的这一集：金色 + ▶ 前缀（下载勾选样式不受影响）
                            let playing_this_ep = now_playing
                                .as_ref()
                                .filter(|np| np.drama_id == drama.id && np.ep == chapter.index)
                                .is_some();
                            let label_text = if playing_this_ep {
                                format!("▶{}", chapter.index)
                            } else {
                                format!("{}", chapter.index)
                            };
                            let mut label =
                                egui::RichText::new(label_text).size(11.0);
                            if playing_this_ep {
                                label = label.strong().color(egui::Color32::from_rgb(255, 200, 90));
                            } else if chapter.index <= watch_last {
                                label = label.color(egui::Color32::from_gray(120)); // 已看=灰
                            }
                            let cell = ui.add_sized(
                                [30.0, 20.0],
                                egui::widgets::Button::new(label).selected(picked),
                            );
                            // 左键=播放这一集（默认行为）；右键=勾选/取消下载
                            if cell.clicked() {
                                play_ep = Some(chapter.index);
                            }
                            if cell.secondary_clicked() {
                                toggled = Some(chapter.index);
                            }
                        }
                    });
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        let picked_count = detail.selected.len();
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
                        // 串流播放：动态标签 + 下一集 + 连播开关。
                        // 正在播本剧 → 主按钮变「下一集」，另给「重播本集」入口。
                        let playing_here = now_playing
                            .as_ref()
                            .filter(|np| np.drama_id == drama.id)
                            .map(|np| np.ep)
                            .filter(|ep| *ep <= chapters.len());
                        if let Some(cur_ep) = playing_here {
                            if cur_ep < chapters.len()
                                && ui.button(format!("⏭ 下一集（第{}集）", cur_ep + 1)).clicked()
                            {
                                play_ep = Some(cur_ep + 1);
                            }
                            if ui.button(format!("▶ 重播第{}集", cur_ep)).clicked() {
                                play_ep = Some(cur_ep);
                            }
                        } else {
                            // 本剧有播放记录 → 续看入口（优先于从第一集开始）
                            let last = self
                                .store
                                .state
                                .watch
                                .get(&drama.id)
                                .map(|w| w.last_ep)
                                .unwrap_or(0);
                            if last > 0
                                && last <= chapters.len()
                                && ui.button(format!("▶ 续看第{}集", last)).clicked()
                            {
                                play_ep = Some(last);
                            } else if ui.button("▶ 播放第1集").clicked() {
                                play_ep = Some(1);
                            }
                        }
                        ui.checkbox(autoplay_next, "连播");
                        if let Some(cur_ep) = playing_here {
                            ui.colored_label(
                                egui::Color32::from_rgb(255, 200, 90),
                                format!("正在播放：第{}集", cur_ep),
                            );
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

impl CoverCache {
    fn card_grid(
        &mut self,
        ui: &mut egui::Ui,
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
                    if let Some(d) = self.card(ui, drama) {
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

    fn card(&mut self, ui: &mut egui::Ui, drama: &Drama) -> Option<Drama> {
        let texture = self.texture(&drama.cover);
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
                let label = if self.pending.contains(&drama.cover) {
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
            // 观看数（热度备胎）：catalog 接口的 play_cnt——排序就是按它
            let metric = if drama.views.is_empty() {
                drama.heat.clone()
            } else {
                drama.views.clone()
            };
            if !metric.is_empty() {
                ui.add_sized(
                    [COVER_W, 13.0],
                    egui::Label::new(
                        egui::RichText::new(format!("▶ {}", crate::model::format_count(&metric)))
                            .size(10.0)
                            .weak(),
                    )
                    .truncate(),
                );
            }
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
