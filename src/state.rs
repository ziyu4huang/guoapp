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
