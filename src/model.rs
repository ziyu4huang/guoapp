use serde::Serialize;

pub const SRC_HONGGUO: &str = "hongguo";

#[derive(Clone, Debug, Default, Serialize)]
pub struct Drama {
    #[serde(rename = "id")]
    pub id: String,
    pub source: String,
    #[serde(rename = "source_id")]
    pub source_id: String,
    pub title: String,
    pub category: String,
    pub cover: String,
    pub intro: String,
    pub remark: String,
    #[serde(rename = "episode_count")]
    pub episode_count: String,
    pub tags: Vec<String>,
    #[serde(rename = "release_status")]
    pub release_status: String,
    pub score: String,
    pub views: String,
    pub heat: String,
    #[serde(rename = "online_date")]
    pub online_date: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Chapter {
    pub index: usize,
    pub title: String,
    #[serde(rename = "video_id")]
    pub video_id: String,
}

#[derive(Clone, Debug, Default)]
pub struct Media {
    pub url: String,
    pub referer: String,
    pub quality: i64,
    /// 16-byte AES key decoded from spade_a, when the source delivers one.
    pub cenc_key: Option<[u8; 16]>,
    pub variants: Vec<Variant>,
}

#[derive(Clone, Debug)]
pub struct Variant {
    pub url: String,
    pub referer: String,
    pub quality: i64,
    pub cenc_key: Option<[u8; 16]>,
}

pub fn drama_id(source: &str, source_id: &str) -> String {
    format!("{}:{}", source, source_id.trim())
}

pub fn split_drama_id(id: &str) -> Option<(&str, &str)> {
    let (source, rest) = id.trim().split_once(':')?;
    if rest.trim().is_empty() {
        return None;
    }
    Some((source, rest.trim()))
}

pub fn is_numeric_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_digit())
}

pub fn is_http_media_url(raw: &str) -> bool {
    match url::Url::parse(raw.trim()) {
        Ok(u) => {
            matches!(u.scheme(), "http" | "https") && !u.host_str().unwrap_or("").is_empty()
        }
        Err(_) => false,
    }
}
