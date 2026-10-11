//! Hongguo App API client (`api5-normal-sinfonlineb.fqnovel.com`) with the
//! X-Gorgon request signing, plus catalog / detail / media / search parsing.

use crate::jsonx::{jbool, jint, jlist, jmap, jnum_str, jstr};
use crate::model::{is_numeric_id, Chapter, Drama, Media, Variant, SRC_HONGGUO};
use crate::sign::{now_ms, sign_request};
use crate::state::{FeedCursor, Store};
use serde_json::{json, Value};
use std::io::Read;
use std::time::Duration;

pub const APP_BASE: &str = "https://api5-normal-sinfonlineb.fqnovel.com";
pub const WEB_BASE: &str = "https://hongguoduanju.com";
const APP_UA: &str = "com.phoenix.read/73532 (Linux; U; Android 16; zh_CN; 25053RT47C; Build/BP2A.250605.031.A3; Cronet/TTNetVersion:04657795 2026-01-23 QuicVersion:c67e9834 2025-09-08)";
pub const WEB_UA: &str = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_7 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.6 Mobile/15E148 Safari/604.1";

#[derive(Clone, Copy)]
pub struct Genre {
    pub key: &'static str,
    pub scene: &'static str,
    pub name: &'static str,
}

pub static GENRES: [Genre; 3] = [
    Genre { key: "short_play", scene: "default", name: "真人剧" },
    Genre { key: "comic_series", scene: "comic_series", name: "漫剧" },
    Genre { key: "ai_series", scene: "ai_series", name: "AI剧" },
];

pub struct HgClient {
    pub agent: ureq::Agent,
    /// Agent without an overall timeout, for large media downloads.
    pub agent_dl: ureq::Agent,
}

impl Default for HgClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HgClient {
    pub fn new() -> HgClient {
        HgClient {
            agent: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(10))
                .build(),
            agent_dl: ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(15))
                .build(),
        }
    }

    fn common_query(&self, store: &Store) -> Vec<(String, String)> {
        let mut q: Vec<(String, String)> = [
            ("aid", "8662"),
            ("app_name", "novelread"),
            ("version_code", "73532"),
            ("version_name", "7.3.5.32"),
            ("manifest_version_code", "73532"),
            ("update_version_code", "73532"),
            ("channel", "update_64"),
            ("device_platform", "android"),
            ("os", "android"),
            ("ssmix", "a"),
            ("device_type", "25053RT47C"),
            ("device_brand", "Redmi"),
            ("language", "zh"),
            ("os_api", "36"),
            ("os_version", "16"),
            ("resolution", "1280*2772"),
            ("dpi", "520"),
            ("ac", "wifi"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        q.push(("device_id".into(), store.state.device_id.clone()));
        q.push(("iid".into(), store.state.install_id.clone()));
        q
    }

    /// Signed App API request; returns the parsed JSON object or an error.
    pub fn app_request(
        &self,
        store: &Store,
        path: &str,
        payload: Option<&Value>,
    ) -> Result<Value, String> {
        let mut query = self.common_query(store);
        let body: Vec<u8> = payload
            .map(|p| serde_json::to_vec(p).map_err(|e| e.to_string()))
            .transpose()?
            .unwrap_or_default();
        let millis = now_ms();
        query.push(("_rticket".into(), millis.to_string()));
        query.sort();
        let query_string = encode_query(&query);
        let signature = sign_request(
            &query_string,
            if body.is_empty() { None } else { Some(&body) },
            (millis / 1000) as u32,
            millis,
        );
        let url = format!("{}{}?{}", APP_BASE, path, query_string);
        let mut request = self
            .agent
            .post(&url)
            .timeout(Duration::from_secs(20))
            .set("User-Agent", APP_UA)
            .set("Accept", "application/json")
            .set("X-XS-From-Web", "0")
            .set("Sdk-Version", "2")
            .set("X-Khronos", &signature.khronos)
            .set("X-Gorgon", &signature.gorgon)
            .set("X-SS-Req-Ticket", &signature.ticket);
        if let Some(stub) = &signature.stub {
            request = request.set("X-SS-STUB", stub);
        }
        let response = if body.is_empty() {
            request.call()
        } else {
            request
                .set("Content-Type", "application/json; charset=utf-8")
                .send_bytes(&body)
        };
        let response = match response {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => return Err(format!("红果 App 接口 HTTP {}", code)),
            Err(e) => return Err(format!("红果 App 请求失败：{}", e)),
        };
        let mut reader = response.into_reader().take(32 * 1024 * 1024);
        let mut bytes = Vec::new();
        reader
            .read_to_end(&mut bytes)
            .map_err(|e| format!("红果 App 响应读取失败：{}", e))?;
        if bytes.is_empty() {
            return Err("红果 App 接口未返回有效数据".into());
        }
        let result: Value =
            serde_json::from_slice(&bytes).map_err(|_| "红果 App 接口返回格式异常".to_string())?;
        let code = jnum_str(&result, &["code", "Code", "status_code"]);
        let code = if code.is_empty() {
            jnum_str(jmap(&result, &["BaseResp"]), &["StatusCode"])
        } else {
            code
        };
        if !code.is_empty() && code != "0" {
            return Err(format!("红果 App 接口暂不可用（{}）", truncate(&code, 20)));
        }
        Ok(result)
    }

    /// One catalog page for a genre feed. Returns dramas and the next cursor.
    pub fn catalog_page(
        &self,
        store: &Store,
        genre: &Genre,
        cursor: &FeedCursor,
    ) -> Result<(Vec<Drama>, FeedCursor), String> {
        let mut payload = json!({
            "req_scene": genre.scene,
            "offset": cursor.offset,
            "limit": 18,
            "req_type": "only_content",
            "need_selector_panel": false,
            "client_req_type": if cursor.offset > 0 { 2 } else { 3 },
            "session_id": cursor.session_id,
            "filter_ids": "",
            "select_items": {
                "genre": [genre.key],
                "sort": ["online_time"],
                "gender": [],
                "category_dim_theme": [],
                "category_dim_role": [],
                "category_dim_epoch": [],
                "online_time": [],
                "creation_status": []
            }
        });
        let attempt_cursor = cursor.clone();
        let mut last_err = String::new();
        for attempt in 0..2 {
            if attempt > 0 {
                payload["session_id"] = json!("");
            }
            match self.app_request(store, "/reading/distribution/category/landpage/v/", Some(&payload)) {
                Ok(result) => {
                    return parse_catalog_page(&result, &attempt_cursor, genre.name);
                }
                Err(err) => {
                    last_err = err;
                    if attempt_cursor.session_id.is_empty() {
                        break;
                    }
                }
            }
        }
        Err(last_err)
    }

    pub fn detail(&self, store: &Store, series_id: &str) -> Result<(Drama, Vec<Chapter>), String> {
        if !is_numeric_id(series_id) {
            return Err("红果剧集 ID 无效".into());
        }
        let result = self.app_request(
            store,
            "/novel/player/video_detail/v1/",
            Some(&json!({ "series_id": series_id })),
        )?;
        parse_app_detail(&result, series_id)
    }

    /// Resolve playable media for one episode via the App API.
    pub fn media(&self, store: &Store, video_id: &str) -> Result<Media, String> {        if !is_numeric_id(video_id) {
            return Err("红果视频 ID 无效".into());
        }
        let result = self.app_request(
            store,
            "/novel/player/video_model/v1/",
            Some(&json!({
                "video_id": video_id,
                "content_type": 1,
                "biz_param": { "need_all_video_definition": true, "video_platform": 3 }
            })),
        )?;
        let data = jmap(&result, &["data"]);
        let mut model = data.get("video_model").cloned().unwrap_or(Value::Null);
        if let Some(encoded) = model.as_str() {
            model = serde_json::from_str(encoded)
                .map_err(|_| "红果 App 播放信息格式异常".to_string())?;
        }
        select_app_media(&model)
    }
}

pub fn encode_query(query: &[(String, String)]) -> String {
    let mut out = String::new();
    for (index, (key, value)) in query.iter().enumerate() {
        if index > 0 {
            out.push('&');
        }
        out.push_str(&encode_component(key));
        out.push('=');
        out.push_str(&encode_component(value));
    }
    out
}

fn encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

pub fn parse_catalog_page(
    result: &Value,
    cursor: &FeedCursor,
    category: &str,
) -> Result<(Vec<Drama>, FeedCursor), String> {
    let data = jmap(result, &["data"]);
    let rows = match data.get("video_data").and_then(|v| v.as_array()) {
        Some(rows) => rows,
        None => return Err("App 分类数据格式异常".into()),
    };
    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let drama = drama_from_value(row, category);
        if !drama.id.is_empty() {
            items.push(drama);
        }
    }
    if !rows.is_empty() && items.is_empty() {
        return Err("App 分类未返回可识别的剧集".into());
    }
    #[allow(unused_assignments)]
    let mut next = cursor.offset;
    if let Some(value) = jint(data, &["next_offset"]) {
        next = value;
    } else if !rows.is_empty() {
        return Err("App 分页标记无效，已保留上次位置".into());
    }
    let has_more = jbool(data, "has_more");
    let session = jstr(data, &["session_id"]);
    if session.len() > 4096 {
        return Err("App 分页会话无效，已保留上次位置".into());
    }
    let has_more = has_more.ok_or("App 分页标记无效，已保留上次位置")?;
    if has_more && (items.is_empty() || next <= cursor.offset || next > 1_000_000) {
        return Err("App 分页未前进，已保留上次位置".into());
    }
    if !has_more && (next < cursor.offset || next > 1_000_000) {
        next = cursor.offset;
    }
    Ok((
        items,
        FeedCursor {
            offset: next,
            session_id: session,
            exhausted: !has_more,
            initialized: true,
        },
    ))
}

/// `hongguoDramaFromAny` port.
pub fn drama_from_value(value: &Value, category: &str) -> Drama {
    let empty = Value::Null;
    let vd = value.get("video_data").unwrap_or(&empty);
    let vd = if vd.as_object().map(|o| o.is_empty()).unwrap_or(false) || vd.is_null() {
        value
    } else {
        vd
    };
    let source_id = first_non_empty(
        &[
            jnum_str(vd, &["series_id_str", "series_id"]),
            jnum_str(value, &["series_id_str", "series_id"]),
            jnum_str(vd, &["keyword"]),
            jnum_str(value, &["keyword"]),
        ],
        is_numeric_id,
    );
    if source_id.is_empty() || !is_numeric_id(&source_id) {
        return Drama::default();
    }
    let title = first_non_empty(
        &[
            jstr(vd, &["series_title", "series_name", "title"]),
            jstr(value, &["series_name", "name"]),
        ],
        |s| !s.is_empty(),
    );
    let title = if title.is_empty() { source_id.clone() } else { title };
    let cover = first_non_empty(
        &[jstr(vd, &["series_cover", "cover"]), jstr(value, &["series_cover"])],
        |s| !s.is_empty(),
    );
    let intro = first_non_empty(
        &[jstr(vd, &["series_intro", "video_desc"]), jstr(value, &["series_intro"])],
        |s| !s.is_empty(),
    );
    let count = first_non_empty(
        &[jnum_str(vd, &["episode_cnt"]), jnum_str(value, &["episode_cnt"])],
        |s| !s.is_empty(),
    );
    let remark_raw = jnum_str(vd, &["episode_right_text"]);
    let mut release_status = release_status_from_remark(&remark_raw);
    match jnum_str(vd, &["series_status"]).as_str() {
        "1" => release_status = "finished".into(),
        "0" => release_status = "ongoing".into(),
        _ => {}
    }
    let mut remark = remark_raw.clone();
    if remark.is_empty() && !count.is_empty() {
        remark = format!("共{}集", count);
    }
    let mut tags = jstr_list(vd, "tags");
    for item in jlist(vd.get("category_list").unwrap_or(&Value::Null)) {
        if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
            if !name.is_empty() && !tags.iter().any(|t| t == name) {
                tags.push(name.to_string());
            }
        }
    }
    if let Ok(schema) =
        serde_json::from_str::<Vec<Value>>(&jstr(vd, &["category_schema"]))
    {
        for item in &schema {
            if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                if !name.is_empty() && !tags.iter().any(|t| t == name) {
                    tags.push(name.to_string());
                }
            }
        }
    }
    let genre = jstr(vd, &["category_name", "categoryName", "category"]);
    let category_name = if genre.is_empty() {
        tags.first().cloned().unwrap_or_else(|| category.to_string())
    } else {
        genre
    };
    let hot = jmap(vd, &["hot_score_data"]);
    let heat = {
        let score = first_non_empty(
            &[jnum_str(hot, &["score"]), jnum_str(vd, &["hot_score"])],
            |s| {
                !s.is_empty()
                    && s.parse::<f64>().map(|v| v.is_finite() && v >= 0.0).unwrap_or(false)
            },
        );
        if score.is_empty() {
            jstr(hot, &["text"])
        } else {
            score
        }
    };
    Drama {
        id: crate::model::drama_id(SRC_HONGGUO, &source_id),
        source: SRC_HONGGUO.into(),
        source_id,
        title,
        category: category_name,
        cover,
        intro,
        remark,
        episode_count: count,
        tags,
        release_status,
        score: jnum_str(vd, &["score"]),
        views: jnum_str(vd, &["series_play_cnt", "play_cnt"]),
        heat,
        online_date: online_date(vd),
    }
}

fn jstr_list(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn first_non_empty(values: &[String], pred: impl Fn(&str) -> bool) -> String {
    values
        .iter()
        .find(|s| pred(s))
        .cloned()
        .unwrap_or_default()
}

fn release_status_from_remark(remark: &str) -> String {
    let ongoing_markers = ["更新", "连载", "待更新"];
    let finished_markers = ["完结", "全集", "已完结", "全"];
    if finished_markers.iter().any(|m| remark.contains(m)) && !remark.contains("更新至") {
        "finished".into()
    } else if ongoing_markers.iter().any(|m| remark.contains(m)) {
        "ongoing".into()
    } else {
        String::new()
    }
}

fn online_date(vd: &Value) -> String {
    if let Some(date) = timestamp_date(&jnum_str(vd, &["first_visible_time"])) {
        return date;
    }
    for item in jlist(vd.get("sub_title_list").unwrap_or(&Value::Null)) {
        let label = jstr(item, &["content"]);
        if label == "今日上新" || label == "昨日上新" {
            // Both resolve relative to now (UTC+8); computed at call time.
            let days = if label == "今日上新" { 0 } else { -1 };
            return china_date(days);
        }
        if label.ends_with("上新") {
            let date: String = label[..label.len() - "上新".len()]
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '-' || *c == '/')
                .collect();
            let date = date.replace('/', "-");
            if date.len() == 10 && date.as_bytes()[4] == b'-' {
                return date;
            }
        }
    }
    String::new()
}

fn china_date(day_offset: i64) -> String {
    // UTC+8 "today" without pulling in chrono.
    let secs = crate::sign::now_ms() / 1000 + 8 * 3600 + day_offset * 86400;
    let days = secs.div_euclid(86400) as i64;
    civil_from_days(days)
}

fn civil_from_days(days: i64) -> String {
    // Howard Hinnant's civil_from_days algorithm.
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", if m <= 2 { y + 1 } else { y }, m, d)
}

fn timestamp_date(raw: &str) -> Option<String> {
    let value: i64 = raw.parse().ok()?;
    if value <= 0 {
        return None;
    }
    let secs = if raw.len() >= 13 { value / 1000 + 8 * 3600 } else { value + 8 * 3600 };
    Some(civil_from_days(secs.div_euclid(86400)))
}

pub fn parse_app_detail(result: &Value, series_id: &str) -> Result<(Drama, Vec<Chapter>), String> {
    let detail = jmap(result, &["data", "video_data"]);
    let returned = jnum_str(detail, &["series_id_str", "series_id"]);
    if returned != series_id {
        return Err("红果 App 未返回所请求的剧集".into());
    }
    let drama = drama_from_value(detail, "短剧");
    let mut chapters: Vec<Chapter> = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();
    let mut seen_index = std::collections::HashSet::new();
    for row in jlist(detail.get("video_list").unwrap_or(&Value::Null)) {
        let video_id = jstr(row, &["vid"]);
        let index = jint(row, &["vid_index"]).unwrap_or(0);
        if index < 1 || !is_numeric_id(&video_id) {
            return Err("红果 App 分集编号或视频 ID 无效".into());
        }
        let owner = jnum_str(row, &["series_id"]);
        if !owner.is_empty() && owner != series_id {
            return Err("红果 App 返回了其他剧集的分集".into());
        }
        if !seen_ids.insert(video_id.clone()) || !seen_index.insert(index) {
            return Err("红果 App 返回了重复分集".into());
        }
        chapters.push(Chapter {
            index: index as usize,
            title: format!("第{}集", index),
            video_id,
        });
    }
    chapters.sort_by_key(|c| c.index);
    let total = jint(detail, &["episode_cnt"]).unwrap_or(0);
    if chapters.is_empty() || total as usize > chapters.len() {
        return Err("红果 App 未返回完整分集".into());
    }
    for (position, chapter) in chapters.iter().enumerate() {
        if chapter.index != position + 1 {
            return Err("红果 App 分集列表不连续".into());
        }
    }
    Ok((drama, chapters))
}

fn select_app_media(model: &Value) -> Result<Media, String> {
    let mut variants: Vec<&Value> = jlist(model.get("video_list").unwrap_or(&Value::Null));
    if variants.is_empty() {
        if let Some(map) = model.get("video_list").and_then(|v| v.as_object()) {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                variants.push(&map[key]);
            }
        }
    }
    let duration = jint(model, &["video_duration", "duration"]).unwrap_or(0);
    let mut choices: Vec<(i64, Media)> = Vec::new();
    let mut key_err = String::new();
    for variant in variants {
        let meta = jmap(variant, &["video_meta"]);
        let codec = jstr(meta, &["codec_type"]).to_lowercase();
        if codec == "bytevc2" || jstr(variant, &["gear_des_key"]).to_lowercase().contains("bytevc2") {
            continue;
        }
        let addresses = media_addresses(variant);
        if addresses.is_empty() {
            continue;
        }
        let encryption = jmap(variant, &["encrypt_info"]);
        let spade = jstr(encryption, &["spade_a"]);
        let mut cenc_key = None;
        if !spade.is_empty()
            || encryption.get("encrypt") == Some(&Value::Bool(true))
            || jstr(encryption, &["encryption_method"]) == "cenc-aes-ctr"
        {
            match crate::backup::content_key(&spade) {
                Ok(key) => cenc_key = Some(key),
                Err(err) => {
                    key_err = err;
                    continue;
                }
            }
        }
        let mut height = jint(meta, &["vheight"]).unwrap_or(0);
        if let Ok(definition) = parse_quality(&jstr(meta, &["definition"])) {
            height = definition;
        } else {
            let width = jint(meta, &["vwidth"]).unwrap_or(0);
            if width > 0 && (height == 0 || width < height) {
                height = width;
            }
        }
        let mut quality = height * 10;
        if codec == "h264" || codec == "avc1" {
            quality += 1;
        }
        for address in addresses {
            choices.push((
                quality,
                Media {
                    url: address,
                    referer: "https://novel.snssdk.com/".into(),
                    quality: height,
                    cenc_key,
                    variants: Vec::new(),
                },
            ));
        }
    }
    if choices.is_empty() {
        if !key_err.is_empty() {
            return Err(format!("红果 App 媒体密钥不可用: {}", key_err));
        }
        return Err("红果 App 未返回兼容的媒体，已跳过不支持的编码".into());
    }
    choices.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    let mut selected = choices[0].1.clone();
    for (_, choice) in &choices {
        selected.variants.push(Variant {
            url: choice.url.clone(),
            referer: choice.referer.clone(),
            quality: choice.quality,
            cenc_key: choice.cenc_key,
        });
    }
    let _ = duration;
    Ok(selected)
}

fn parse_quality(text: &str) -> Result<i64, ()> {
    let digits: String = text.chars().skip_while(|c| !c.is_ascii_digit()).take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<i64>().map_err(|_| ())
}

pub fn parse_quality_pub(text: &str) -> i64 {
    parse_quality(text).unwrap_or(0)
}

/// Recursively collect main/backup URLs; values may be base64-encoded.
pub fn media_addresses(info: &Value) -> Vec<String> {
    let mut addresses = Vec::new();
    let mut seen = std::collections::HashSet::new();
    fn add(value: &Value, addresses: &mut Vec<String>, seen: &mut std::collections::HashSet<String>) {
        match value {
            Value::String(raw) => {
                let mut address = raw.trim().to_string();
                if address.len() > 8192 {
                    return;
                }
                if !crate::model::is_http_media_url(&address) {
                    match crate::backup::decode_b64(&address) {
                        Ok(decoded) => match String::from_utf8(decoded) {
                            Ok(text) => address = text.trim().to_string(),
                            Err(_) => return,
                        },
                        Err(_) => return,
                    }
                }
                if crate::model::is_http_media_url(&address) && seen.insert(address.clone()) {
                    addresses.push(address);
                }
            }
            Value::Array(items) => {
                for item in items {
                    add(item, addresses, seen);
                }
            }
            _ => {}
        }
    }
    for key in ["main_url", "backup_url", "backup_url_1", "backup_url_2", "backup_urls", "url_list"] {
        if let Some(value) = info.get(key) {
            add(value, &mut addresses, &mut seen);
        }
    }
    addresses
}

/// Drama detail with App → Web fallback (shared by CLI and GUI).
pub fn detail_with_fallback(
    client: &HgClient,
    store: &Store,
    drama_id: &str,
) -> Result<(Drama, Vec<Chapter>), String> {
    let (_, series_id) = crate::model::split_drama_id(drama_id)
        .ok_or_else(|| "剧 ID 无效，应为 hongguo:<数字>".to_string())?;
    if !is_numeric_id(series_id) {
        return Err("红果剧集 ID 无效".into());
    }
    match client.detail(store, series_id) {
        Ok(result) => Ok(result),
        Err(app_err) => match client.web_detail(series_id) {
            Ok((title, chapters)) => Ok((
                Drama {
                    id: crate::model::drama_id(crate::model::SRC_HONGGUO, series_id),
                    source: crate::model::SRC_HONGGUO.into(),
                    source_id: series_id.to_string(),
                    title,
                    ..Drama::default()
                },
                chapters,
            )),
            Err(web_err) => Err(format!("App 详情失败：{}；网页详情失败：{}", app_err, web_err)),
        },
    }
}

/// Combined search: web search page + suggestion-name index, merged and ranked.
/// Returns dramas plus an optional warning.
pub fn search_combined(
    client: &HgClient,
    keyword: &str,
) -> Result<(Vec<Drama>, Option<String>), String> {
    let keyword = crate::hgweb::validate_keyword(keyword)?;
    let names = crate::hgweb::search_names(client, &keyword);
    let page = client.web_search(&keyword);
    if page.is_err() && names.is_err() {
        return Err(format!(
            "红果搜索失败：{}；{}",
            page.err().unwrap(),
            names.err().unwrap()
        ));
    }
    let mut dramas: Vec<Drama> = Vec::new();
    let mut warning: Option<String> = None;
    match page {
        Ok((page_dramas, _)) => crate::hgweb::merge_dramas(&mut dramas, page_dramas),
        Err(_) => warning = Some("红果综合搜索暂不可用，已保留名称匹配结果".into()),
    }
    match names {
        Ok(list) => crate::hgweb::merge_dramas(&mut dramas, list),
        Err(_) if warning.is_none() => {
            warning = Some("红果名称检索暂不可用，结果可能缺少部分剧集".into())
        }
        Err(_) => {}
    }
    dramas.sort_by(|a, b| {
        crate::hgweb::title_search_rank(&a.title, &keyword)
            .cmp(&crate::hgweb::title_search_rank(&b.title, &keyword))
            .then_with(|| a.title.cmp(&b.title))
    });
    Ok((dramas, warning))
}
