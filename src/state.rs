use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FeedCursor {
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub exhausted: bool,
    #[serde(default)]
    pub initialized: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub install_id: String,
    #[serde(default)]
    pub feeds: BTreeMap<String, FeedCursor>,
    /// 播放器窗口位置/大小与播放设定记忆（跨启动恢复；player::PlayerPrefs）。
    #[serde(default)]
    pub player_prefs: crate::player::PlayerPrefs,
    /// 目录排序偏好（hgapp::SORT_*，跨启动记忆）。
    #[serde(default)]
    pub catalog_sort: String,
    /// 红果网页登录会话 cookie（用户从浏览器粘贴；播放页需登录态才有播放数据）
    #[serde(default)]
    pub web_cookie: String,
    /// 每部剧各自的播放记录（互不共用；key=drama_id）。
    #[serde(default)]
    pub watch: BTreeMap<String, WatchRec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EpisodeEntry {
    pub index: usize,
    pub quality: i64,
    pub file: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LibraryEntry {
    pub drama_id: String,
    pub title: String,
    pub dir: String,
    pub entry: String,
    pub kind: String,
    pub episodes: Vec<EpisodeEntry>,
}

/// 单部剧的播放记录（每部独立，key=drama_id）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WatchRec {
    #[serde(default)]
    pub last_ep: usize,
    #[serde(default)]
    pub updated_at: i64,
    /// 剧名（「继续观看」列显示用；旧记录可能为空）
    #[serde(default)]
    pub title: String,
    /// 集内播放位置（秒）：精确续看「从 12:30 继续」
    #[serde(default)]
    pub pos: u64,
    /// pos 属于哪一集（换集后旧位置自动失效）
    #[serde(default)]
    pub pos_ep: usize,
    /// 该集总长（秒，0=未知）
    #[serde(default)]
    pub dur: u64,
    /// 上次看到该剧时的总集数——目录回传更大的值=有新集（追更提醒数据源）
    #[serde(default)]
    pub seen_total: usize,
}

impl WatchRec {
    /// 精确续看判定（纯函数）：pos 属于该集、≥5s、且未接近结尾（≥95% 视为看完
    /// 从头播）→ 返回起播秒数；否则 0（整集重播）。
    pub fn resume_start(&self, ep: usize) -> u64 {
        if self.pos_ep != ep || self.pos < 5 {
            return 0;
        }
        if self.dur > 0 && self.pos as f64 >= self.dur as f64 * 0.95 {
            return 0;
        }
        self.pos
    }
}

/// 「继续观看」列表（纯函数）：按最近观看排序、过滤无标题记录、截取前 n 条。
pub fn continue_watching(
    watch: &BTreeMap<String, WatchRec>,
    n: usize,
) -> Vec<(String, String, usize)> {
    let mut items: Vec<(&String, &WatchRec)> = watch.iter().collect();
    items.sort_by_key(|(_, r)| std::cmp::Reverse(r.updated_at));
    items
        .into_iter()
        .filter(|(_, r)| !r.title.is_empty() && r.last_ep > 0)
        .take(n)
        .map(|(id, r)| (id.clone(), r.title.clone(), r.last_ep))
        .collect()
}

/// 秒 → mm:ss（超过 1h → h:mm:ss）。
pub fn mmss(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

impl LibraryEntry {
    /// 已下载集 → 本地章节（按集号排序；video_id=本地文件路径，local=true）。
    pub fn local_chapters(&self) -> Vec<crate::model::Chapter> {
        let mut eps: Vec<&EpisodeEntry> = self.episodes.iter().collect();
        eps.sort_by_key(|e| e.index);
        eps.into_iter()
            .map(|e| crate::model::Chapter {
                index: e.index,
                title: format!("第{}集", e.index),
                video_id: {
                    let dir = std::path::PathBuf::from(&self.dir)
                        .join(format!("ep_{:03}", e.index))
                        .join(&e.file);
                    dir.display().to_string()
                },
                local: true,
            })
            .collect()
    }
}

#[derive(Debug, Default, Clone)]
pub struct Store {
    pub dir: PathBuf,
    pub state: State,
    pub library: BTreeMap<String, LibraryEntry>,
}

fn default_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("guoapp");
        }
    }
    if cfg!(target_os = "windows") {
        let home = std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOME"))
            .unwrap_or_else(|_| ".".into());
        return PathBuf::from(home).join("AppData").join("Local").join("guoapp");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    if cfg!(target_os = "macos") {
        PathBuf::from(home).join("Library/Application Support/guoapp")
    } else {
        PathBuf::from(home).join(".local/share/guoapp")
    }
}

impl Store {
    pub fn load() -> Store {
        let dir = std::env::var("GUOAPP_DATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| default_dir());
        let mut store = Store {
            dir,
            state: State::default(),
            library: BTreeMap::new(),
        };
        if let Ok(text) = std::fs::read_to_string(store.dir.join("state.json")) {
            if let Ok(parsed) = serde_json::from_str::<State>(&text) {
                store.state = parsed;
            }
        }
        if let Ok(text) = std::fs::read_to_string(store.dir.join("library.json")) {
            if let Ok(map) = serde_json::from_str::<BTreeMap<String, LibraryEntry>>(&text) {
                store.library = map;
            }
        }
        if store.state.device_id.is_empty() {
            store.state.device_id = crate::sign::new_device_id();
            store.state.install_id = crate::sign::new_device_id();
        }
        store
    }

    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(
            self.dir.join("state.json"),
            serde_json::to_vec_pretty(&self.state)?,
        )?;
        std::fs::write(
            self.dir.join("library.json"),
            serde_json::to_vec_pretty(&self.library)?,
        )?;
        Ok(())
    }

    pub fn feed(&self, key: &str) -> FeedCursor {
        self.state.feeds.get(key).cloned().unwrap_or_default()
    }

    pub fn set_feed(&mut self, key: &str, cursor: FeedCursor) {
        self.state.feeds.insert(key.to_string(), cursor);
    }
}

#[cfg(test)]
mod watch_tests {
    use super::*;

    #[test]
    fn new_episode_detection() {
        let r = WatchRec {
            last_ep: 3,
            seen_total: 12,
            ..Default::default()
        };
        let has_new = |ep_count: usize| ep_count > r.seen_total;
        assert!(has_new(14), "14 > 12 应有更新");
        assert!(!has_new(12), "12 == 12 无更新");
        assert!(!has_new(11));
    }

    #[test]
    fn resume_start_matrix() {
        let mut r = WatchRec {
            last_ep: 3,
            pos: 750,
            pos_ep: 3,
            dur: 1500,
            updated_at: 0,
            title: "剧".into(),
            seen_total: 12,
        };
        // 位置屬於該集+≥5s+未到結尾 → 從 750s 續
        assert_eq!(r.resume_start(3), 750);
        // 換集 → 舊位置失效
        assert_eq!(r.resume_start(4), 0);
        // <5s → 從頭
        r.pos = 3;
        assert_eq!(r.resume_start(3), 0);
        // ≥95% 視為看完 → 從頭
        r.pos = 1440;
        assert_eq!(r.resume_start(3), 0);
        // dur 未知 → 不套 95% 規則
        r.dur = 0;
        r.pos = 750;
        assert_eq!(r.resume_start(3), 750);
    }

    #[test]
    fn continue_watching_sorts_and_filters() {
        let mut w: BTreeMap<String, WatchRec> = BTreeMap::new();
        w.insert("a".into(), WatchRec { last_ep: 2, updated_at: 100, title: "甲".into(), ..Default::default() });
        w.insert("b".into(), WatchRec { last_ep: 5, updated_at: 300, title: "乙".into(), ..Default::default() });
        w.insert("c".into(), WatchRec { last_ep: 1, updated_at: 200, title: "".into(), ..Default::default() }); // 無標題
        w.insert("d".into(), WatchRec { last_ep: 0, updated_at: 400, title: "丁".into(), ..Default::default() }); // 未播
        let list = continue_watching(&w, 10);
        assert_eq!(list, vec![("b".into(), "乙".into(), 5), ("a".into(), "甲".into(), 2)]);
        // cap 生效
        assert_eq!(continue_watching(&w, 1).len(), 1);
    }

    #[test]
    fn mmss_formats() {
        assert_eq!(mmss(0), "0:00");
        assert_eq!(mmss(750), "12:30");
        assert_eq!(mmss(3723), "1:02:03");
    }

    #[test]
    fn local_chapters_sorted_and_marked() {
        let e = LibraryEntry {
            drama_id: "hongguo:1".into(),
            title: "剧".into(),
            dir: "/dl/剧".into(),
            entry: String::new(),
            kind: "mp4".into(),
            episodes: vec![
                EpisodeEntry { index: 3, quality: 1080, file: "e3.mp4".into() },
                EpisodeEntry { index: 1, quality: 1080, file: "e1.mp4".into() },
            ],
        };
        let cs = e.local_chapters();
        assert_eq!(cs.len(), 2);
        assert_eq!((cs[0].index, cs[1].index), (1, 3), "必須按集號排序");
        assert!(cs[0].local);
        assert!(cs[0].video_id.ends_with("ep_001/e1.mp4"));
    }
}

#[cfg(test)]
mod update_tests {
    use super::WatchRec;

    #[test]
    fn new_episode_detection() {
        let mut r = WatchRec { last_ep: 3, seen_total: 12, ..Default::default() };
        // 目录回传更大总集数 → 有更新 +K
        assert_eq!(r.seen_total, 12);
        // 语义:episode_count(目录) > seen_total → 有新集
        let count = |r: &WatchRec, ep_count: usize| ep_count > r.seen_total;
        assert!(count(&r, 14), "14 > 12 应有更新");
        assert!(!count(&r, 12), "12 == 12 无更新");
        // 记录更新后不再报
        r.seen_total = 14;
        assert!(!count(&r, 14));
    }
}
