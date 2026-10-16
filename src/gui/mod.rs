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
    /// 插入序（FIFO 逐出用）：texture 只进不出会线性吃 GPU/内存
    order: std::collections::VecDeque<String>,
}

const COVER_WORKERS: usize = 4;
/// 封面纹理上限（张）——超出逐出最旧（handle drop 即释放显存）
const COVER_CAP: usize = 300;

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
        CoverCache {
            map: HashMap::new(),
            pending: HashSet::new(),
            queue_tx,
            order: std::collections::VecDeque::new(),
        }
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
        let handle = rgba.map(|(bytes, w, h)| {
            let image = egui::ColorImage::from_rgba_unmultiplied([w, h], &bytes);
            ctx.load_texture("cover", image, Default::default())
        });
        // FIFO 逐出：纹理只进不出=显存/内存线性涨（长会话刷多页实测路径）
        if !self.order.contains(&url) {
            self.order.push_back(url.clone());
        }
        while self.map.len() >= COVER_CAP {
            let Some(oldest) = self.order.pop_front() else { break };
            if self.map.remove(&oldest).is_some() {
                break;
            }
        }
        self.map.insert(url, handle);
    }

    fn clear(&mut self) {
        self.map.clear();
        self.pending.clear();
        self.order.clear();
    }
}

struct DetailState {
    drama: Drama,
    chapters: Option<Vec<Chapter>>,
    selected: HashSet<usize>,
    error: Option<String>,
    /// 看片/下载模式（默认看片；同一次打开内记忆切换）
    mode: DetailMode,
    /// 章节载入后自动开播这一集（卡片 ▶ 直播入口用；None=不自动播）
    auto_play: Option<usize>,
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
    /// 用户点 ✕ 取消：不入片库、不在进行中计数里（在飞的那一集数据仍会下完）
    Cancelled,
}

#[allow(clippy::large_enum_variant)] // 通道訊息;Box 化 CatalogPage 的 Vec 換來的只是搬運指針
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
        #[allow(dead_code)]
        total: usize,
        url: String,
        referer: String,
        key: Option<String>,
        /// 起播秒数（精确续看；0=从头）
        start: u64,
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
    /// 播放位置取样（每 5s）：写进该剧的播放记录 → 精确续看「从 12:30 继续」。
    WatchPos { pos: u64, dur: u64 },
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
    /// 浏览器 cookie 自动提取完成：Ok=(来源, cookie 名列表, Cookie 请求头)。
    CookieFetch {
        result: Result<(String, Vec<String>, String), String>,
    },
    Status(String),
}

/// 卡片交互结果：点封面=开详情；点 hover ▶ 角标=不开面板直接播（续看/第 1 集）。
enum CardAction {
    Open(Drama),
    Play(Drama),
}

/// 详情面板的两个互不干扰的操作模式（用户令：看片与下载分开，不硬放一起）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DetailMode {
    /// 看片（默认）：大播放按钮 + 干净的集数网格（左键播放）
    Watch,
    /// 下载：勾选格 + 批量按钮 + 本剧进度/取消
    Download,
}

struct DownloadJob {
    store: Store,
    /// 共享 HTTP 客戶端（連線池/TLS session 跨集復用；舊代碼每集重建=白扔握手）
    client: Arc<HgClient>,
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

/// 看片会话：正在播的剧+全部章节+当前集。自带 chapters——播放/连播/底栏
/// **不依赖详情面板是否打开**（旧 NowPlaying 依赖 detail 取章节数据，看片时
/// 关掉详情面板连播链就断，是本次重构修掉的结构性缺陷）。
#[derive(Clone)]
struct WatchSession {
    drama: Drama,
    chapters: Vec<Chapter>,
    ep: usize,
    /// 本地已下载会话（片库内建播放）：预载/下一集走本地文件，不走取流
    is_local: bool,
}

impl WatchSession {
    fn total(&self) -> usize {
        self.chapters.len()
    }
    fn ep_title(&self) -> String {
        self.ep_title_for(self.ep)
    }
    fn ep_title_for(&self, ep: usize) -> String {
        format!("《{}》第{}集", self.drama.title, ep)
    }
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
    /// 下载勾选状态（按剧记忆，重开详情不重置；会话内存态）
    dl_selection: HashMap<String, HashSet<usize>>,
    /// 目录显示过滤（None=全部；Some(genre_key)=只显示该分类）
    catalog_view: Option<String>,
    /// 网页登录对话框（cookie 导入）
    login_dialog_open: bool,
    login_cookie_input: String,
    /// 「从浏览器自动获取」进行中（钥匙圈授权/解密在后台线程）。
    cookie_fetch_busy: bool,
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
    /// 看片会话（None=没在播）。与详情面板解耦：关掉详情不影响连播。
    watch: Option<WatchSession>,
    /// 取流中的看片会话（PlayReady 落位，seq 守卫防串台）。
    pending_watch: Option<WatchSession>,
    /// 已预载进 mpv 播放列表的下一集（drama_id, ep）——当前集播完时 mpv
    /// 内部直接切换，无黑屏等待。
    appended_next: Option<(String, usize)>,
    /// 正在后台解析的「下一集」集号（防重复解析）。
    append_in_flight: Option<usize>,
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
            watch: None,
            pending_watch: None,
            appended_next: None,
            append_in_flight: None,
            autoplay_next: true,
            dl_selection: HashMap::new(),
            catalog_view: None,
            login_dialog_open: false,
            login_cookie_input: String::new(),
            cookie_fetch_busy: false,
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
            let cursor = store.feed(cat_key(genre.key, sort).as_str());
            match client.catalog_page(&store, genre, &cursor, sort.key()) {
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

    /// 后台线程自动提取浏览器登录 cookie（Chrome 家族主路+Safari 兜底）。
    /// 钥匙圈首次访问会弹授权对话框，用户点「允许」后线程才能拿到解密密钥。
    fn request_cookie_fetch(&mut self) {
        if self.cookie_fetch_busy {
            return;
        }
        self.cookie_fetch_busy = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = crate::browser_cookies::extract_login_cookie(
                crate::browser_cookies::WEB_LOGIN_DOMAIN,
            );
            let _ = tx.send(Msg::CookieFetch {
                result: result.map(|ex| (ex.source, ex.names, ex.header)),
            });
        });
    }

    fn open_detail(&mut self, drama: &Drama, auto_play_ep: Option<usize>) {
        let drama_id = drama.id.clone();
        let selected = self
            .dl_selection
            .get(drama.id.as_str())
            .cloned()
            .unwrap_or_default();
        self.detail = Some(DetailState {
            drama: drama.clone(),
            chapters: None,
            selected,
            error: None,
            mode: DetailMode::Watch,
            auto_play: auto_play_ep,
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
                client: self.client.clone(),
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

    /// 把看片会话的当前集写进该剧本人的播放记录（每部独立、互不共用）。
    /// PlayReady 与连播推进两条路都调它，续看/已看灰字才不会停在老集数。
    fn note_watch_progress(&mut self) {
        let Some(ws) = self.watch.as_ref() else { return };
        let rec = self.store.state.watch.entry(ws.drama.id.clone()).or_default();
        rec.last_ep = ws.ep;
        rec.title = ws.drama.title.clone();
        // 见过的总集数就地增长(追更提醒的基准线:目录超过它=有新集)
        if ws.total() > rec.seen_total {
            rec.seen_total = ws.total();
        }
        rec.updated_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let _ = self.store.save();
    }

    /// 确保「下一集」已在后台解析并 append 进 mpv 播放列表——当前集自然播完
    /// 时 mpv 内部直接切换（零黑屏），这是连播连续感的根基。幂等：已预载/
    /// 已在解析/没有下一集/关连播时不动。每次集开播（PlayReady/FileLoaded）调用。
    fn ensure_next_appended(&mut self) {
        let Some(ws) = self.watch.as_ref() else { return };
        if !self.autoplay_next {
            return;
        }
        let next = ws.ep + 1;
        if next > ws.total()
            || self.appended_next.as_ref().is_some_and(|(_, n)| *n == next)
            || self.append_in_flight == Some(next)
        {
            return;
        }
        // 本地会话：下一集已下载就同步 append 本地文件（无网络）
        if ws.is_local {
            let Some(chapter) = ws.chapters.iter().find(|c| c.index == next).cloned() else {
                return;
            };
            let item = crate::player::PlayItem {
                url: chapter.video_id.clone(),
                referer: String::new(),
                key: None,
                title: ws.ep_title_for(next),
                start: 0,
            };
            if let Some(s) = self.player.as_ref() {
                if s.append(&item).is_ok() {
                    self.appended_next = Some((ws.drama.id.clone(), next));
                    self.set_status(format!("已预载第{}集（本地），播完无缝接上", next));
                }
            }
            return;
        }
        // 章节来自看片会话——详情面板关着也能预载下一集
        let Some(chapter) = ws.chapters.iter().find(|c| c.index == next).cloned() else {
            return;
        };
        let series_id = ws.drama.source_id.clone();
        let drama_id = ws.drama.id.clone();
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

    /// 从详情面板发起播放（集格/hero 按钮）。
    fn request_play_from_detail(&mut self, ep: usize) {
        let Some(detail) = self.detail.as_ref() else { return };
        let drama = detail.drama.clone();
        let chapters = detail.chapters.clone().unwrap_or_default();
        self.begin_play(&drama, &chapters, ep);
    }

    /// 从底栏播放条发起播放（下一集/重播）——详情面板关着也能用。
    /// 本地会话走本地文件，串流会话走取流。
    fn request_play_from_watch(&mut self, ep: usize) {
        let Some(ws) = self.watch.as_ref() else { return };
        if ws.is_local {
            let drama_id = ws.drama.id.clone();
            self.play_local_episode(&drama_id, ep);
            return;
        }
        let drama = ws.drama.clone();
        let chapters = ws.chapters.clone();
        self.begin_play(&drama, &chapters, ep);
    }

    /// 播放公共入口（串流）：记下看片会话（pending，PlayReady 落位）并起取流线程。
    /// 起播秒数在此查播放记录（精确续看），随回包落到 PlayItem。
    fn begin_play(&mut self, drama: &Drama, chapters: &[Chapter], ep: usize) {
        let Some(chapter) = chapters.iter().find(|c| c.index == ep).cloned() else {
            return;
        };
        let start = self
            .store
            .state
            .watch
            .get(&drama.id)
            .map(|r| r.resume_start(ep))
            .unwrap_or(0);
        let series_id = drama.source_id.clone();
        let drama_id = drama.id.clone();
        let drama_title = drama.title.clone();
        let total = chapters.len();
        self.pending_watch = Some(WatchSession {
            drama: drama.clone(),
            chapters: chapters.to_vec(),
            ep,
            is_local: false,
        });
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
                    start,
                    seq,
                });
            }
            Err(err) => {
                let _ = tx.send(Msg::Status(err));
            }
        });
    }

    /// 片库内建播放：本地已下载集直接起 mpv 会话（无取流），
    /// 下一集/连播/底栏/精确续看与串流同一套体验。
    fn play_local_episode(&mut self, drama_id: &str, ep: usize) {
        let Some(entry) = self.store.library.get(drama_id) else { return };
        let chapters = entry.local_chapters();
        let Some(chapter) = chapters.iter().find(|c| c.index == ep).cloned() else {
            self.set_status(format!("第{}集 未下载", ep));
            return;
        };
        let start = self
            .store
            .state
            .watch
            .get(drama_id)
            .map(|r| r.resume_start(ep))
            .unwrap_or(0);
        let drama = Drama {
            id: drama_id.to_string(),
            source: crate::model::SRC_HONGGUO.into(),
            source_id: drama_id.split(':').nth(1).unwrap_or("").to_string(),
            title: entry.title.clone(),
            ..Default::default()
        };
        let ws = WatchSession { drama, chapters, ep, is_local: true };
        let item = crate::player::PlayItem {
            url: chapter.video_id.clone(),
            referer: String::new(),
            key: None,
            title: ws.ep_title_for(ep),
            start,
        };
        self.start_session_with_item(item, ws);
    }

    /// 把已就绪的 PlayItem 套到会话上（复用/重启 + 事件线程 + 位置取样线程），
    /// 并落位看片会话（写播放记录、预载下一集）。串流（PlayReady）与
    /// 本地（play_local_episode）共用同一条上台路径。
    fn start_session_with_item(&mut self, item: crate::player::PlayItem, ws: WatchSession) {
        // 单实例：会话还活着且 IPC 可达 → 直接换集（不闪新窗口）
        let reuse = match self.player.as_mut() {
            Some(s) => s.process_alive() && s.ipc_ready() && s.load(&item).is_ok(),
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
                        let snapshot = move |ipc: &Path| {
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
                    // 播放位置取样线程（每 5s 查 time-pos/duration）——
                    // 精确续看的数据源；随会话停机旗生灭
                    {
                        let running_pos = session.running_flag();
                        let tx_pos = self.tx.clone();
                        let ipc_pos = session.ipc.clone();
                        thread::spawn(move || {
                            while running_pos.load(std::sync::atomic::Ordering::Relaxed) {
                                thread::sleep(std::time::Duration::from_secs(5));
                                if !running_pos.load(std::sync::atomic::Ordering::Relaxed) {
                                    return;
                                }
                                let pos = crate::player::get_property_u64(&ipc_pos, "time-pos");
                                let dur = crate::player::get_property_u64(&ipc_pos, "duration");
                                let (Some(pos), Some(dur)) = (pos, dur) else {
                                    continue;
                                };
                                if tx_pos.send(Msg::WatchPos { pos, dur }).is_err() {
                                    return;
                                }
                            }
                        });
                    }
                    self.player = Some(session);
                    self.set_status(format!("▶ 正在播放：{}", item.title));
                }
                Err(e) => {
                    let log_path = self.store.dir.join("mpv-last.log");
                    let tail = crate::player::tail_lines(&log_path, 2);
                    let hint = if tail.is_empty() {
                        "（未安装？参考 README 安装 mpv，或设 GUO_MPV 指向可执行文件）".to_string()
                    } else {
                        format!("（日志尾行：{}）", tail)
                    };
                    self.set_status(format!("启动播放器失败：{} {}", e, hint));
                }
            }
        }
        if self.player.is_some() {
            // 每部剧各自的播放记录（互不共用；标题供「继续观看」列）
            self.watch = Some(ws);
            self.note_watch_progress();
            self.ensure_next_appended();
        }
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
                        // 预设全选；但同剧已有手动勾选记录 → 恢复它（重开不洗掉）
                        let all: HashSet<usize> = list.iter().map(|c| c.index).collect();
                        let picked = self
                            .dl_selection
                            .entry(drama_id.clone())
                            .or_insert(all)
                            .clone();
                        detail.selected = picked;
                        // 到访即更新「见过总集数」基准——追更提醒只在没看过的新集上出现
                        if !list.is_empty() {
                            let rec = self.store.state.watch.entry(drama_id.clone()).or_default();
                            if list.len() > rec.seen_total {
                                rec.seen_total = list.len();
                                let _ = self.store.save();
                            }
                        }
                        detail.chapters = Some(list);
                        // 卡片 ▶ 直播入口：章节一到位就自动开播
                        if let Some(ep) = detail.auto_play.take() {
                            let d = detail.drama.clone();
                            let chapters = detail.chapters.clone().unwrap_or_default();
                            self.begin_play(&d, &chapters, ep);
                        }
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
            Msg::PlayReady { drama_id, drama_title, ep, total: _, url, referer, key, start, seq } => {
                // 过期回包丢弃：用户连点两集/连播与手动点击并发时，只有最后
                // 一次请求的 resolve 结果允许上台（后完成抢播 = 播错集）。
                if seq != self.play_seq {
                    return;
                }
                self.append_in_flight = None;
                let item = crate::player::PlayItem {
                    url,
                    referer,
                    key,
                    title: format!("《{}》第{}集", drama_title, ep),
                    start,
                };
                // pending 会话落位核对：过期回包（连点换集）只报状态不上台
                let Some(ws) = self
                    .pending_watch
                    .take()
                    .filter(|w| w.drama.id == drama_id && w.ep == ep)
                else {
                    self.set_status(format!("▶ 正在播放：{}", drama_title));
                    return;
                };
                self.start_session_with_item(item, ws);
            }
            Msg::AppendReady { drama_id, ep, url, referer, key } => {
                // 竞态修复：只有「自己发起的那次解析」回来才清 in-flight——
                // 旧代码无条件清，手动换集后旧回包会把新解析的标记抹掉，
                // 导致同一集 resolve 两次、append 两遍（播放列表重复）。
                if self.append_in_flight == Some(ep) {
                    self.append_in_flight = None;
                }
                // 上台守卫：必须仍是「正在播 ep-1」的同剧目会话，且连播还开着、
                // 播放器还活着——手动换集/关连播/换剧后回来的过期预载一律丢弃。
                let ok = self
                    .watch
                    .as_ref()
                    .is_some_and(|ws| {
                        ws.drama.id == drama_id && ws.ep + 1 == ep && self.autoplay_next
                    });
                if !ok {
                    return;
                }
                let Some(s) = self.player.as_ref() else { return };
                if !s.ipc_ready() {
                    return;
                }
                let ws = self.watch.as_ref().unwrap();
                let item = crate::player::PlayItem {
                    url,
                    referer,
                    key,
                    title: ws.ep_title_for(ep),
                    start: 0,
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
                    .watch
                    .as_ref()
                    .and_then(|ws| {
                        crate::player::autoplay_next_ep(&reason, self.autoplay_next, ws.ep, ws.total())
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
                                    .watch
                                    .as_ref()
                                    .is_some_and(|ws| ws.drama.id == *did)
                        });
                    if preloaded {
                        if let Some(ws) = self.watch.as_mut() {
                            ws.ep = next_ep;
                        }
                        // 连播推进也必须写播放记录——旧代码只写 PlayReady 路径，
                        // 无缝连播看到第 30 集记录还停在手动播的第 1 集（续看/灰字全错）
                        self.note_watch_progress();
                        if let Some(ws) = self.watch.as_ref() {
                            if let Some(s) = self.player.as_ref() {
                                let _ = s.set_title(&ws.ep_title_for(next_ep));
                            }
                        }
                        self.set_status(format!("第{}集 播完，无缝接第{}集", next_ep - 1, next_ep));
                    } else {
                        // 未预载（解析慢/关过连播又开/预载失败）：退回取流路径
                        self.set_status(format!("第{}集 播完，自动接第{}集…", next_ep - 1, next_ep));
                        self.request_play_from_watch(next_ep);
                    }
                } else if reason == "eof" {
                    // 关着连播却还有预载条目 → 立刻丢弃，防 mpv 自动接播
                    if self.appended_next.take().is_some() {
                        if let Some(s) = self.player.as_ref() {
                            let _ = s.playlist_clear();
                        }
                    }
                    let label = self
                        .watch
                        .as_ref()
                        .map(|ws| format!("第{}集 播完（共{}集）", ws.ep, ws.total()))
                        .unwrap_or_else(|| "播放结束".into());
                    self.set_status(label);
                } else if reason != "stop" && reason != "quit" && reason != "redirect" {
                    self.set_status(format!("播放中断：{}（可重试同一集；日志见数据目录 mpv-last.log）", reason));
                }
            }
            Msg::PlayerFileLoaded => {
                // 每个文件真正载入（含连播自动切换的下一集）后确保再下一集已预载
                self.ensure_next_appended();
            }
            Msg::WatchPos { pos, dur } => {
                // 集内位置取样落进该剧的播放记录（精确续看数据源）
                if let Some(ws) = self.watch.as_ref() {
                    let rec = self.store.state.watch.entry(ws.drama.id.clone()).or_default();
                    rec.pos_ep = ws.ep;
                    rec.pos = pos;
                    rec.dur = dur;
                    let _ = self.store.save();
                }
            }
            Msg::PlayerPrefs(prefs) => {
                // 只在有实质内容时落盘（IPC 全挂=全 Default，别把上一份记忆洗掉）
                if prefs != crate::player::PlayerPrefs::default() {
                    self.store.state.player_prefs = prefs;
                    let _ = self.store.save();
                }
            }
            Msg::CookieFetch { result } => {
                self.cookie_fetch_busy = false;
                match result {
                    Ok((source, names, header)) => {
                        self.store.state.web_cookie = header;
                        self.login_cookie_input.clear();
                        let _ = self.store.save();
                        self.set_status(format!(
                            "已从{}自动获取 {} 颗登录 cookie（{}）——重新点要播的集数试试",
                            source,
                            names.len(),
                            names.join(", ")
                        ));
                    }
                    Err(err) => {
                        self.set_status(format!("自动取 cookie 失败：{}", err));
                    }
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
        // 标题优先详情面板；看片中关掉面板后从看片会话兜底
        // （旧代码只会落空串，片库会积一批《》空标题条目）
        let title = self
            .detail
            .as_ref()
            .filter(|d| d.drama.id == drama_id)
            .map(|d| d.drama.title.clone())
            .or_else(|| {
                self.watch
                    .as_ref()
                    .filter(|w| w.drama.id == drama_id)
                    .map(|w| w.drama.title.clone())
            })
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
    let client = job.client;
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
    // 进度节流：≥100ms 才发一条（10Hz 足够进度条），终值 1.0 必发。
    // 旧代码每 chunk 一条——mpsc 洪水拖累 UI 重绘频率。
    let mut last_sent = std::time::Instant::now() - std::time::Duration::from_secs(1);
    let result = media::download_episode_progress(
        &client,
        &resolved,
        &out_dir,
        job.quality,
        &format!("第{}集", job.ep),
        &mut move |frac| {
            let done = frac >= 1.0;
            if !done && last_sent.elapsed() < std::time::Duration::from_millis(100) {
                return;
            }
            last_sent = std::time::Instant::now();
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
                // 常驻播放条：任意 Tab 都能看到正在播什么、能切下一集/连播
                // （看片主目的的常驻控制位；文本全部 truncate 防 720px 最小宽挤爆）
                let player_alive = self
                    .player
                    .as_mut()
                    .map(|s| s.process_alive())
                    .unwrap_or(false);
                let mut bar_next: Option<usize> = None;
                if let (Some(ws), true) = (&self.watch, player_alive) {
                    let title_ws = ws.clone();
                    if let Some(ws) = Some(&title_ws) {
                        ui.add_sized(
                            [180.0, 18.0],
                            egui::Label::new(
                                egui::RichText::new(format!("▶ {}", ws.ep_title()))
                                    .size(12.0)
                                    .color(egui::Color32::from_rgb(255, 200, 90)),
                            )
                            .truncate(),
                        );
                    }
                    if ws.ep < ws.total()
                        && ui
                            .small_button(format!("⏭ 下一集（第{}集）", ws.ep + 1))
                            .clicked()
                    {
                        bar_next = Some(ws.ep + 1);
                    }
                    ui.checkbox(&mut self.autoplay_next, "连播");
                    ui.separator();
                }
                if let Some(ep) = bar_next {
                    self.request_play_from_watch(ep);
                }
                if self.status.1.elapsed().as_secs() < 10 {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(&self.status.0).weak())
                                .truncate(),
                        );
                    });
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

        // 网页登录对话框(cookie 导入)——上游锁定匿名取流后的试行解锁路径
        if self.login_dialog_open {
            let mut save = false;
            let mut clear = false;
            let mut auto_fetch = false;
            egui::Window::new("🔑 登录红果网页账号")
                .open(&mut self.login_dialog_open)
                .collapsible(false)
                .show(ctx, |ui| {
                    ui.label("上游已锁定匿名取流——登录态可能解锁播放（试行）。");
                    #[cfg(target_os = "macos")]
                    {
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if self.cookie_fetch_busy {
                                ui.add_enabled(
                                    false,
                                    egui::Button::new("正在读取浏览器 cookie…"),
                                );
                            } else if ui.button("⚡ 从浏览器自动获取").clicked() {
                                auto_fetch = true;
                            }
                        });
                        if self.cookie_fetch_busy {
                            ui.weak("若弹出钥匙圈授权对话框，请点「允许」（一次性授权）");
                        } else {
                            ui.weak("自动获取：直接读本机浏览器的红果登录 cookie（Chrome/Edge/Brave/Arc/Safari）");
                        }
                        ui.add_space(4.0);
                        ui.separator();
                    }
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("获取 cookie 三步：").strong());
                    ui.label("1. 浏览器打开 hongguoduanju.com 并登录账号");
                    ui.label("2. 按 F12 打开开发者工具 → 网络(Network)→ 随便点一个请求");
                    ui.label("3. 在请求头里复制整行 Cookie 的值，粘贴到下面：");
                    ui.add_space(4.0);
                    ui.add(
                        egui::TextEdit::multiline(&mut self.login_cookie_input)
                            .desired_rows(4)
                            .desired_width(380.0)
                            .hint_text("粘贴整行 Cookie 值（不含 Cookie: 前缀）"),
                    );
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        if ui.button("保存").clicked() {
                            save = true;
                        }
                        if !self.store.state.web_cookie.is_empty()
                            && ui.button("清除已存").clicked()
                        {
                            clear = true;
                        }
                        ui.weak(if self.store.state.web_cookie.is_empty() {
                            "未设置"
                        } else {
                            "已保存（仅存本机数据目录，不上传）"
                        });
                    });
                });
            if auto_fetch {
                self.request_cookie_fetch();
            }
            if save {
                self.store.state.web_cookie = self.login_cookie_input.trim().to_string();
                let _ = self.store.save();
                self.set_status("网页 cookie 已保存——重新点击要播放的集数试试");
            }
            if clear {
                self.store.state.web_cookie.clear();
                self.login_cookie_input.clear();
                let _ = self.store.save();
                self.set_status("网页 cookie 已清除");
            }
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
            catalog_view,
            ..
        } = self;
        let sort = *catalog_sort;
        let catalog_view = catalog_view.clone();
        let watched_ids: HashSet<String> = self.store.state.watch.keys().cloned().collect();
        let continue_list = crate::state::continue_watching(&self.store.state.watch, 10);
        // 追更比对：看过(≥1集)且目录总集数 > 见过的总集数 → 该剧有新集
        let mut new_eps: HashMap<String, usize> = HashMap::new();
        for slot in catalog.values() {
            for d in slot.iter() {
                if let Some(rec) = self.store.state.watch.get(&d.id) {
                    if rec.last_ep == 0 || rec.seen_total == 0 {
                        continue;
                    }
                    let count: usize = d
                        .episode_count
                        .chars()
                        .filter(|c| c.is_ascii_digit())
                        .collect::<String>()
                        .parse()
                        .unwrap_or(0);
                    if count > rec.seen_total {
                        new_eps.insert(d.id.clone(), count - rec.seen_total);
                    }
                }
            }
        }
        let mut open: Option<CardAction> = None;
        let mut more: Option<String> = None;
        let mut refresh = false;
        let mut switch_sort: Option<CatalogSort> = None;
        let mut switch_view: Option<Option<String>> = None;
        let mut continue_ep: Option<(String, usize)> = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("排序：");
                for s in CatalogSort::all() {
                    if ui.selectable_label(sort == s, s.label()).clicked() && sort != s {
                        switch_sort = Some(s);
                    }
                }
                ui.separator();
                ui.label("显示：");
                if ui.selectable_label(catalog_view.is_none(), "全部").clicked() {
                    switch_view = None;
                }
                for genre in GENRES.iter() {
                    let count = catalog.get(&cat_key(genre.key, sort)).map(|v| v.len()).unwrap_or(0);
                    let label = format!("{} ({})", genre.name, count);
                    let busy = catalog_loading.contains(&cat_key(genre.key, sort));
                    if ui
                        .selectable_label(catalog_view.as_deref() == Some(genre.key), label)
                        .clicked()
                    {
                        switch_view = Some(Some(genre.key.to_string()));
                    }
                    // 切到该分类时若还没数据 → 顺手懒加载第一页
                    if catalog_view.as_deref() == Some(genre.key)
                        && count == 0
                        && !busy
                        && !catalog_exhausted.contains(&cat_key(genre.key, sort))
                    {
                        more = Some(genre.key.to_string());
                    }
                }
                if ui.button("刷新").clicked() {
                    refresh = true;
                }
            });
            // 「继续观看」置顶列（Netflix/YouTube 惯例）：最近看的剧一键续看
            if !continue_list.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.label("继续观看：");
                    for (id, title, ep) in &continue_list {
                        let fresh = new_eps.get(id).map(|k| format!(" 🔔+{}", k)).unwrap_or_default();
                        if ui
                            .small_button(format!("《{}》续看第{}集{}", title, ep, fresh))
                            .clicked()
                        {
                            continue_ep = Some((id.clone(), *ep));
                        }
                    }
                });
                ui.separator();
            }
            egui::ScrollArea::vertical().id_salt("catalog_scroll").show(ui, |ui| {
                for genre in GENRES.iter() {
                    // 显示过滤：选了单一分类只渲染它（旧代码三类全堆叠=用户报「不正常」）
                    if catalog_view.as_deref().is_some_and(|v| v != genre.key) {
                        continue;
                    }
                    let Some(dramas) = catalog.get(&cat_key(genre.key, sort)) else { continue };
                    if dramas.is_empty() {
                        continue;
                    }
                    ui.add_space(6.0);
                    ui.heading(genre.name);
                    ui.add_space(2.0);
                    if let Some(action) = cover_cache.card_grid(ui, dramas, &watched_ids, &new_eps) {
                        open = Some(action);
                    }
                }
                let visible_empty = catalog
                    .iter()
                    .filter(|(k, _)| k.ends_with(&format!("@{}", sort.key())))
                    .filter(|(k, _)| {
                        catalog_view
                            .as_deref()
                            .is_none_or(|v| k.starts_with(&format!("{v}@")))
                    })
                    .all(|(_, v)| v.is_empty());
                if visible_empty
                    && GENRES.iter().all(|g| {
                        catalog_view
                            .as_deref()
                            .is_some_and(|v| v != g.key)
                            || !catalog_loading.contains(&cat_key(g.key, sort))
                    })
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
                        if catalog_view.as_deref().is_some_and(|v| v != genre.key) {
                            continue;
                        }
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
        if let Some((drama_id, ep)) = continue_ep {
            // 续看列：只有 id/标题（无封面详情）——构造最小 Drama 开详情并自动开播
            let drama = Drama {
                id: drama_id.clone(),
                source: crate::model::SRC_HONGGUO.into(),
                source_id: drama_id.split(':').nth(1).unwrap_or("").to_string(),
                title: self
                    .store
                    .state
                    .watch
                    .get(&drama_id)
                    .map(|r| r.title.clone())
                    .unwrap_or_default(),
                ..Default::default()
            };
            self.open_detail(&drama, Some(ep));
        }
        if let Some(sort) = switch_sort {
            self.set_catalog_sort(sort);
        }
        if let Some(view) = switch_view {
            self.catalog_view = view;
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
        match open {
            Some(CardAction::Open(drama)) => self.open_detail(&drama, None),
            // 卡片 ▶ 直播：续看上次那集；没看过从第 1 集（章节载入后自动开播）
            Some(CardAction::Play(drama)) => {
                let last = self
                    .store
                    .state
                    .watch
                    .get(&drama.id)
                    .map(|w| w.last_ep)
                    .unwrap_or(0);
                self.open_detail(&drama, Some(last.max(1)));
            }
            None => {}
        }
    }

    fn show_search(&mut self, ctx: &egui::Context) {
        let Self { cover_cache, search_input, search_busy, search_submitted, search_results, .. } = self;
        let mut submit = false;
        let mut open: Option<Drama> = None;
        let mut play: Option<Drama> = None;
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
                let watched: HashSet<String> =
                    self.store.state.watch.keys().cloned().collect();
                let mut new_eps: HashMap<String, usize> = HashMap::new();
                for d in search_results.iter() {
                    if let Some(rec) = self.store.state.watch.get(&d.id) {
                        if rec.last_ep > 0 && rec.seen_total > 0 {
                            let count: usize = d.episode_count.chars().filter(|c| c.is_ascii_digit()).collect::<String>().parse().unwrap_or(0);
                            if count > rec.seen_total {
                                new_eps.insert(d.id.clone(), count - rec.seen_total);
                            }
                        }
                    }
                }
                if let Some(action) = cover_cache.card_grid(ui, search_results, &watched, &new_eps) {
                    // 搜索结果同样支持 ▶ 直播（Play 带出详情+自动开播）
                    match action {
                        CardAction::Open(d) => open = Some(d),
                        CardAction::Play(d) => play = Some(d),
                    }
                }
            });
        });
        if submit {
            self.request_search();
        }
        if let Some(drama) = play {
            let last = self
                .store
                .state
                .watch
                .get(&drama.id)
                .map(|w| w.last_ep)
                .unwrap_or(0);
            self.open_detail(&drama, Some(last.max(1)));
        } else if let Some(drama) = open {
            self.open_detail(&drama, None);
        }
    }

    fn show_library(&mut self, ctx: &egui::Context) {
        let mut play_local: Option<(String, usize)> = None;
        let mut clear_watch = false;
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                if ui.small_button("清空播放记录").clicked() {
                    clear_watch = true;
                }
            });
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
                                        DlStatus::Active | DlStatus::Cancelled => {
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
                            // ▶=内建播放器（续看/连播/底栏同一套体验）；
                            // 系统播放器走「打开目录」自取
                            if ui
                                .small_button(format!("▶{}", ep.index))
                                .on_hover_text(format!("内建播放：{}", path.display()))
                                .clicked()
                            {
                                play_local = Some((entry.drama_id.clone(), ep.index));
                            }
                        }
                    });
                }
            });
        });
        if clear_watch {
            self.store.state.watch.clear();
            let _ = self.store.save();
            self.set_status("播放记录已清空");
        }
        if let Some((drama_id, ep)) = play_local {
            self.play_local_episode(&drama_id, ep);
        }
    }

    fn show_detail(&mut self, ui: &mut egui::Ui) -> bool {
        // 字段级拆借:chapters/selected 全程只读引用,不再每帧整份克隆
        let Self {
            detail,
            cover_cache,
            watch,
            autoplay_next,
            ..
        } = self;
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
        let mut switch_mode: Option<DetailMode> = None;
        let mut cancel_eps: Vec<usize> = Vec::new();

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
            // ── 模式切换（用户令：看片与下载分家，不硬放一起）──
            ui.horizontal(|ui| {
                if ui
                    .selectable_label(detail.mode == DetailMode::Watch, "▶ 看片")
                    .clicked()
                    && detail.mode != DetailMode::Watch
                {
                    switch_mode = Some(DetailMode::Watch);
                }
                if ui
                    .selectable_label(detail.mode == DetailMode::Download, "⬇ 下载")
                    .clicked()
                    && detail.mode != DetailMode::Download
                {
                    switch_mode = Some(DetailMode::Download);
                }
            });
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
                Some(chapters) if detail.mode == DetailMode::Watch => {
                    // ═══ 看片模式：播放是一等公民 ═══
                    let playing_here = watch
                        .as_ref()
                        .filter(|ws| ws.drama.id == drama.id)
                        .map(|ws| ws.ep)
                        .filter(|ep| *ep <= chapters.len());
                    let watch_last = self
                        .store
                        .state
                        .watch
                        .get(&drama.id)
                        .map(|w| w.last_ep)
                        .unwrap_or(0);
                    // hero 按钮一步开播：正在播→下一集；有记录→续看；否则→第 1 集
                    if let Some(cur_ep) = playing_here.filter(|e| *e < chapters.len()) {
                        if ui
                            .add_sized(
                                [ui.available_width(), 34.0],
                                egui::widgets::Button::new(
                                    egui::RichText::new(format!("⏭  下一集（第{}集）", cur_ep + 1))
                                        .size(15.0),
                                ),
                            )
                            .clicked()
                        {
                            play_ep = Some(cur_ep + 1);
                        }
                        ui.horizontal(|ui| {
                            if ui.button(format!("▶ 重播第{}集", cur_ep)).clicked() {
                                play_ep = Some(cur_ep);
                            }
                            ui.checkbox(autoplay_next, "连播");
                            if ui
                                .small_button(if self.store.state.web_cookie.is_empty() {
                                    "🔑 登录"
                                } else {
                                    "🔑 已登录"
                                })
                                .clicked()
                            {
                                self.login_dialog_open = true;
                            }
                            ui.colored_label(
                                egui::Color32::from_rgb(255, 200, 90),
                                format!("● 正在播放：第{}集", cur_ep),
                            );
                        });
                    } else {
                        let last = watch_last.min(chapters.len());
                        // 精确续看：有集内位置 → 标注「从 mm:ss」（start 由 begin_play 查记录下发）
                        let resume = self
                            .store
                            .state
                            .watch
                            .get(&drama.id)
                            .map(|r| r.resume_start(last))
                            .unwrap_or(0);
                        let (hero_label, hero_ep) = if last > 0 {
                            let label = if resume > 0 {
                                format!("▶  续看第{}集（从 {}）", last, crate::state::mmss(resume))
                            } else {
                                format!("▶  续看第{}集", last)
                            };
                            (label, last)
                        } else {
                            ("▶  播放第 1 集".to_string(), 1)
                        };
                        if ui
                            .add_sized(
                                [ui.available_width(), 34.0],
                                egui::widgets::Button::new(
                                    egui::RichText::new(hero_label).size(15.0),
                                ),
                            )
                            .clicked()
                        {
                            play_ep = Some(hero_ep);
                        }
                        ui.checkbox(autoplay_next, "连播");
                            if ui
                                .small_button(if self.store.state.web_cookie.is_empty() {
                                    "🔑 登录"
                                } else {
                                    "🔑 已登录"
                                })
                                .clicked()
                            {
                                self.login_dialog_open = true;
                            }
                        }
                        ui.add_space(2.0);
                        ui.label(format!(
                            "共 {} 集（点击集数即播放 · 右键加入下载清单）",
                            chapters.len()
                        ));
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        for chapter in chapters.iter() {
                            let playing_this_ep = playing_here == Some(chapter.index);
                            let label_text = if playing_this_ep {
                                format!("▶{}", chapter.index)
                            } else {
                                format!("{}", chapter.index)
                            };
                            let mut label = egui::RichText::new(label_text).size(11.0);
                            if playing_this_ep {
                                label = label.strong().color(egui::Color32::from_rgb(255, 200, 90));
                            } else if chapter.index <= watch_last {
                                label = label.color(egui::Color32::from_gray(120)); // 已看=灰
                            }
                            // 看片模式不画下载勾选底色——那属于下载模式
                            let cell = ui.add_sized(
                                [30.0, 20.0],
                                egui::widgets::Button::new(label),
                            );
                            if cell.clicked() {
                                play_ep = Some(chapter.index);
                            }
                            if cell.secondary_clicked() {
                                toggled = Some(chapter.index);
                            }
                        }
                    });
                }
                Some(chapters) => {
                    // ═══ 下载模式：勾选/批量/进度/取消，与看片互不干扰 ═══
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
                    });
                    ui.add_space(4.0);
                    ui.label("勾选要下载的集数（右键在看片模式也能加）：");
                    ui.horizontal_wrapped(|ui| {
                        ui.spacing_mut().item_spacing = egui::vec2(4.0, 4.0);
                        for chapter in chapters.iter() {
                            let picked = selected.contains(&chapter.index);
                            let cell = ui.add_sized(
                                [30.0, 20.0],
                                egui::widgets::Button::new(
                                    egui::RichText::new(format!("{}", chapter.index)).size(11.0),
                                )
                                .selected(picked),
                            );
                            // 下载模式里左键=勾选（主操作）；右键同样勾选（与看片模式手感一致）
                            if cell.clicked() || cell.secondary_clicked() {
                                toggled = Some(chapter.index);
                            }
                        }
                    });
                    ui.add_space(6.0);
                    // 本剧的下载进度 + 取消（误勾全选的刹车）
                    let rows: Vec<(usize, String, f32, DlStatus)> = self
                        .downloads
                        .iter()
                        .filter(|d| d.drama_id == drama.id)
                        .map(|d| (d.ep, d.note.clone(), d.frac, d.status))
                        .collect();
                    if !rows.is_empty() {
                        ui.separator();
                        for (ep, note, frac, status) in rows {
                            ui.horizontal(|ui| {
                                match status {
                                    DlStatus::Cancelled => {
                                        ui.label(
                                            egui::RichText::new(format!("第{}集 已取消", ep))
                                                .size(11.0)
                                                .weak(),
                                        );
                                    }
                                    DlStatus::Active => {
                                        ui.label(
                                            egui::RichText::new(format!("第{}集", ep)).size(11.0),
                                        );
                                        let bar = egui::ProgressBar::new(frac)
                                            .desired_height(12.0);
                                        ui.add(bar);
                                        ui.label(
                                            egui::RichText::new(note.clone()).size(10.0).weak(),
                                        );
                                        if ui.small_button("✕").clicked() {
                                            cancel_eps.push(ep);
                                        }
                                    }
                                    DlStatus::Done => {
                                        ui.label(
                                            egui::RichText::new(format!("第{}集 完成", ep))
                                                .size(11.0)
                                                .color(egui::Color32::from_rgb(120, 200, 120)),
                                        );
                                    }
                                    DlStatus::Failed => {
                                        ui.label(
                                            egui::RichText::new(format!("第{}集 失败：{}", ep, note))
                                                .size(11.0)
                                                .color(egui::Color32::from_rgb(220, 90, 80)),
                                        );
                                    }
                                }
                            });
                        }
                    }
                }
            }
            ui.separator();
            ui.label(egui::RichText::new("简介").strong());
            ui.label(if drama.intro.is_empty() { "（无）" } else { &drama.intro });
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
        if let Some(mode) = switch_mode {
            if let Some(detail) = self.detail.as_mut() {
                detail.mode = mode;
            }
        }
        // 取消下载：标记 Cancelled——进度区隐藏、完成时不再登记入片库
        if !cancel_eps.is_empty() {
            for d in self.downloads.iter_mut() {
                if d.drama_id == self.detail.as_ref().map(|x| x.drama.id.clone()).unwrap_or_default()
                    && cancel_eps.contains(&d.ep)
                    && d.status == DlStatus::Active
                {
                    d.status = DlStatus::Cancelled;
                    d.note = "已取消".into();
                }
            }
            self.set_status(format!("已取消 {} 集的下载", cancel_eps.len()));
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
        // 勾选状态按剧记忆（重开详情恢复，不洗掉用户上次的选择）
        if let Some(detail) = self.detail.as_ref() {
            if let Some(slot) = self.dl_selection.get_mut(&detail.drama.id) {
                if *slot != detail.selected {
                    *slot = detail.selected.clone();
                }
            }
        }
        if let Some(eps) = download_eps {
            self.queue_download(eps);
        }
        if let Some(ep) = play_ep {
            self.request_play_from_detail(ep);
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
        watched: &HashSet<String>,
        new_eps: &HashMap<String, usize>,
    ) -> Option<CardAction> {
        if dramas.is_empty() {
            return None;
        }
        let columns = ((ui.available_width() / (COVER_W + 10.0)).floor() as usize).clamp(1, 12);
        let mut action: Option<CardAction> = None;
        for row in dramas.chunks(columns) {
            let mut picked: Option<CardAction> = None;
            ui.horizontal(|ui| {
                for drama in row {
                    if let Some(a) = self.card(ui, drama, watched, new_eps) {
                        picked = Some(a);
                    }
                }
            });
            if picked.is_some() {
                action = picked;
            }
        }
        action
    }

    fn card(
        &mut self,
        ui: &mut egui::Ui,
        drama: &Drama,
        watched: &HashSet<String>,
        new_eps: &HashMap<String, usize>,
    ) -> Option<CardAction> {
        let texture = self.texture(&drama.cover);
        let mut open = false;
        let mut play = false;
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
            // 追更角标（左上角）：看过的剧出了新集
            if let Some(k) = new_eps.get(&drama.id) {
                painter.rect_filled(
                    egui::Rect::from_min_size(rect.left_top(), egui::vec2(58.0, 18.0)),
                    3.0,
                    egui::Color32::from_rgb(200, 60, 50),
                );
                painter.text(
                    egui::Pos2::new(rect.left() + 29.0, rect.top() + 9.0),
                    egui::Align2::CENTER_CENTER,
                    format!("新集 +{k}"),
                    egui::FontId::proportional(11.0),
                    egui::Color32::WHITE,
                );
            }
            let card = ui.interact(rect, egui::Id::new(&drama.id), egui::Sense::click());
            card.widget_info(|| {
                egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &drama.title)
            });
            if card.clicked() {
                open = true;
            }
            // hover ▶ 角标：不开详情、一步开播（续看/第 1 集）——看片主动线的第一跳
            if card.hovered() {
                let is_watched = watched.contains(&drama.id);
                let badge = format!("▶ {}", if is_watched { "续看" } else { "播放" });
                let badge_rect = egui::Rect::from_min_size(
                    egui::Pos2::new(rect.left(), rect.bottom() - 30.0),
                    egui::vec2(rect.width(), 30.0),
                );
                painter.rect_filled(badge_rect, 0.0, egui::Color32::from_black_alpha(160));
                painter.text(
                    badge_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    &badge,
                    egui::FontId::proportional(13.0),
                    egui::Color32::from_rgb(255, 200, 90),
                );
                let play_zone = ui.interact(
                    badge_rect,
                    egui::Id::new(&drama.id).with("play"),
                    egui::Sense::click(),
                );
                if play_zone.clicked() {
                    play = true;
                }
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
        if play {
            return Some(CardAction::Play(drama.clone()));
        }
        if open {
            return Some(CardAction::Open(drama.clone()));
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
