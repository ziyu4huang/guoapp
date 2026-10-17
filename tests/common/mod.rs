//! hermetic 測試共用基建：本地 fixture 伺服器（目錄/詳情/搜索/封面）+ Range
//! 媒體源 + 環境接管。目標=**第三方可用性徹底脫鉤**——上游(紅果)再怎麼改
//! （鎖端點/改風控/斷流），整套測試都照樣綠（INC-20261015 教訓的制度化）。
//!
//! 樣本鐵則：JSON/HTML fixture 是 `dump_fixtures` 抓的**真盤樣本**（非想像
//! 格式），只在載入時做三類確定性改寫——封面 URL 指回本地、分頁頁碼的
//! series_id 數值偏移、詳情回包的 series_id 對齊請求。上游格式變更後重跑
//! `cargo test --test dump_fixtures -- --ignored` 重抓即可。
//!
//! 唯二故意打真網的測試檔：`dump_fixtures`（樣本抓取器）、`probe_play_raw`
//! （INC 診斷探針）——兩者皆 #[ignore]，不屬於 hermetic 套件。

use serde_json::Value;
use std::io::{Read, Write};
use std::sync::OnceLock;

/// 每進程一份共享伺服器（env 是 process-global：並行測試各自 install() 到
/// 同一組位址，消除「誰後 set 誰贏」的 env 競態）。
static HERMETIC: OnceLock<Hermetic> = OnceLock::new();

pub struct Hermetic {
    /// GUO_APP_BASE / GUO_WEB_BASE：fixture 伺服器（目錄/詳情/搜索/封面）。
    pub base: String,
    /// GUO_RESOLVE_OVERRIDE：本地 Range 媒體源（try_resolve_media 頂部短路）。
    pub media: String,
}

impl Hermetic {
    /// 幂等安裝：三個 env 反覆設成同一組值，任意交錯順序結果一致。
    pub fn install(&self) {
        std::env::set_var("GUO_APP_BASE", &self.base);
        std::env::set_var("GUO_WEB_BASE", &self.base);
        std::env::set_var("GUO_RESOLVE_OVERRIDE", &self.media);
    }
}

/// 取共享 hermetic 環境（首次呼叫時拉起兩個本地伺服器）。
pub fn hermetic() -> &'static Hermetic {
    HERMETIC.get_or_init(|| Hermetic {
        base: spawn_fixture_server(),
        media: spawn_media_server(),
    })
}

// ---------------------------------------------------------------------------
// fixture 伺服器：路由真樣本（改寫後）回包
// ---------------------------------------------------------------------------

const PAGE_SIZE: i64 = 18;
const PAGE_COUNT: usize = 3;
/// 分類維度（req_scene → 索引）：fixture 按分類偏移 id/標題——三個分類的清單
/// 互不相同（真實語義：不同頻道不同劇；同標題多卡也會讓 kittest 2+ 命中炸）。
const GENRE_SCENES: [&str; 3] = ["default", "comic_series", "ai_series"];
const GENRE_SUFFIX: [&str; 3] = ["", "·漫", "·AI"];

fn spawn_fixture_server() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
    let port = listener.local_addr().unwrap().port();
    let cover_url = format!("http://127.0.0.1:{port}/cover.png");
    let pages: Vec<Vec<Value>> = (0..GENRE_SCENES.len())
        .map(|g| {
            (0..PAGE_COUNT)
                .map(|p| build_catalog_page(&cover_url, g, p))
                .collect()
        })
        .collect();
    let detail = build_detail(&cover_url);
    let suggestion = build_suggestion(&cover_url);
    let web_search: &'static [u8] = include_bytes!("../fixtures/web-search.real.html");
    let cover_png: &'static [u8] = include_bytes!("../fixtures/cover.png");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let pages = pages.clone();
            let detail = detail.clone();
            let suggestion = suggestion.clone();
            std::thread::spawn(move || {
                let Some(request) = read_request(&mut stream) else { return };
                let (status, ctype, body) = route(
                    &request,
                    &pages,
                    &detail,
                    &suggestion,
                    web_search,
                    cover_png,
                );
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&body);
            });
        }
    });
    format!("http://127.0.0.1:{port}")
}

struct Request {
    path: String,
    body: Vec<u8>,
}

/// 讀一個 HTTP 請求（頭 + Content-Length body）；非標準輸入一律 None 丟棄。
fn read_request(stream: &mut std::net::TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut one = [0u8; 1];
    loop {
        let n = stream.read(&mut one).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&one[..n]);
        if buf.ends_with(b"\r\n\r\n") || buf.len() > 16_384 {
            break;
        }
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let mut lines = head.lines();
    let first = lines.next()?;
    let path = first.split_whitespace().nth(1)?.to_string();
    let content_length = lines
        .filter_map(|l| l.strip_prefix("Content-Length: ").or_else(|| l.strip_prefix("content-length: ")))
        .next()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        stream.read_exact(&mut body).ok()?;
    }
    Some(Request { path, body })
}

fn route(
    request: &Request,
    pages: &[Vec<Value>],
    detail: &Value,
    suggestion: &Value,
    web_search: &'static [u8],
    cover_png: &'static [u8],
) -> (String, String, Vec<u8>) {
    let path = request.path.clone();
    if path.starts_with("/reading/distribution/category/landpage") {
        let scene = json_str_field(&request.body, "req_scene").unwrap_or_default();
        let genre = GENRE_SCENES.iter().position(|s| *s == scene).unwrap_or(0);
        let offset = json_num_field(&request.body, "offset").unwrap_or(0);
        let page = ((offset.max(0) / PAGE_SIZE) as usize).min(PAGE_COUNT - 1);
        let mut response = pages[genre][page].clone();
        response["data"]["next_offset"] = serde_json::json!(offset + PAGE_SIZE);
        response["data"]["has_more"] = serde_json::json!(true);
        return ("200 OK".into(), "application/json".into(), serde_json::to_vec(&response).unwrap());
    }
    if path.starts_with("/novel/player/video_detail") {
        let series_id = json_str_field(&request.body, "series_id").unwrap_or_default();
        let mut response = detail.clone();
        if let Some(vd) = response.pointer_mut("/data/video_data") {
            patch_series_id(vd, &series_id);
        }
        return ("200 OK".into(), "application/json".into(), serde_json::to_vec(&response).unwrap());
    }
    if path.starts_with("/novel/player/multi_video_model") {
        // 無簽名 multi_video_model(主取流層):回真樣本;vid 不在樣本=模擬未命中
        if path.contains("missing") {
            return ("200 OK".into(), "application/json".into(), br#"{"code":0,"data":{}}"#.to_vec());
        }
        let raw = include_bytes!("../fixtures/multi-video-model-ep40.real.json");
        return ("200 OK".into(), "application/json".into(), raw.to_vec());
    }
    if path.starts_with("/novel/player/video_model") || path.starts_with("/novel/player/") {
        // App 取流域：hermetic 測試一律由 GUO_RESOLVE_OVERRIDE 短路到本地，
        // 打進來=觸點外洩——顯式 503 讓它紅，而不是靜默偽裝成功。
        return ("503 SERVICE UNAVAILABLE".into(), "text/plain".into(), b"hermetic: media path must be overridden".to_vec());
    }
    if path.starts_with("/incent_resource/suggestion") {
        return ("200 OK".into(), "application/json".into(), serde_json::to_vec(suggestion).unwrap());
    }
    if path.starts_with("/video/public/") {
        // 公開播放端點(2026-10-17 發現):免費集直出。fixture 回小檔——
        // public_media 探針只驗狀態碼;路徑含 "paid" 模擬收費集 404
        if path.contains("paid") {
            return ("404 NOT FOUND".into(), "text/plain".into(), b"not in public quota".to_vec());
        }
        return ("200 OK".into(), "video/mp4".into(), cover_png.to_vec());
    }
    if path.starts_with("/search/") {
        return ("200 OK".into(), "text/html; charset=utf-8".into(), web_search.to_vec());
    }
    if path.starts_with("/cover") {
        return ("200 OK".into(), "image/png".into(), cover_png.to_vec());
    }
    (
        "404 NOT FOUND".into(),
        "text/plain".into(),
        format!("fixture server: no route for {path}").into_bytes(),
    )
}

/// 目錄頁：真樣本 + 封面指回本地 + 按（分類,頁）二維偏移 series_id（保證翻頁
/// 與分類清單互不重樣——merge_dramas 按 id 去重，同 id 回包=「更多」測試失效；
/// 同標題多卡=kittest 2+ 命中炸）。標題後綴讓重樣肉眼可辨。
fn build_catalog_page(cover_url: &str, genre: usize, page: usize) -> Value {
    let raw = include_bytes!("../fixtures/catalog-ai-p1.real.json");
    let mut value: Value = serde_json::from_slice(raw).expect("catalog fixture");
    rewrite_urls(&mut value, cover_url);
    shift_ids(&mut value, genre, page);
    value
}

/// 詳情：真樣本 + 封面指回本地；series_id 由請求端決定（GUI 點哪張卡都自洽）。
fn build_detail(cover_url: &str) -> Value {
    let raw = include_bytes!("../fixtures/detail.real.json");
    let mut value: Value = serde_json::from_slice(raw).expect("detail fixture");
    rewrite_urls(&mut value, cover_url);
    value
}

fn build_suggestion(cover_url: &str) -> Value {
    let raw = include_bytes!("../fixtures/suggestion.real.json");
    let mut value: Value = serde_json::from_slice(raw).expect("suggestion fixture");
    rewrite_urls(&mut value, cover_url);
    value
}

/// 遞迴改寫：http(s) 開頭的封面欄位 → 本地 cover URL。
fn rewrite_urls(value: &mut Value, cover_url: &str) {
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                if matches!(key.as_str(), "cover" | "series_cover" | "horiz_cover")
                    && val.as_str().map(|s| s.starts_with("http")).unwrap_or(false)
                {
                    *val = Value::String(cover_url.to_string());
                } else {
                    rewrite_urls(val, cover_url);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| rewrite_urls(i, cover_url)),
        _ => {}
    }
}

/// 遞迴偏移 series_id / 系列 id 欄位（純數值偏移，保持 is_numeric_id 通過），
/// 並給系列標題加分類/頁碼後綴（視覺可辨識；同標題多卡會讓 kittest 炸）。
fn shift_ids(value: &mut Value, genre: usize, page: usize) {
    if genre == 0 && page == 0 {
        return;
    }
    let shift = ((genre * PAGE_COUNT + page + 1) as u128) * 100_000_000_000_000_000;
    let suffix = format!("{}{}",
        GENRE_SUFFIX[genre % GENRE_SUFFIX.len()],
        if page > 0 { format!("·P{}", page + 1) } else { String::new() });
    match value {
        Value::Object(map) => {
            for (key, val) in map.iter_mut() {
                match key.as_str() {
                    "series_id" | "series_id_str" => {
                        if let Some(n) = val.as_str().and_then(|s| s.parse::<u128>().ok()) {
                            *val = Value::String((n + shift).to_string());
                        }
                    }
                    "series_title" | "series_name" | "title" | "name" => {
                        if let Some(s) = val.as_str() {
                            // 尾碼只加一次（同名鍵多層嵌套會重複進入）
                            if !s.is_empty() && !s.ends_with(&suffix) {
                                *val = Value::String(format!("{s}{suffix}"));
                            }
                        }
                    }
                    _ => shift_ids(val, genre, page),
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| shift_ids(i, genre, page)),
        _ => {}
    }
}

/// 詳情回包的 series_id 全點對齊請求（video_data 根 + 每個分集行）。
fn patch_series_id(video_data: &mut Value, series_id: &str) {
    if series_id.is_empty() {
        return;
    }
    if let Some(obj) = video_data.as_object_mut() {
        obj.insert("series_id_str".into(), Value::String(series_id.to_string()));
        obj.insert("series_id".into(), Value::String(series_id.to_string()));
    }
    if let Some(rows) = video_data.get_mut("video_list").and_then(|v| v.as_array_mut()) {
        for row in rows {
            if let Some(obj) = row.as_object_mut() {
                obj.insert("series_id".into(), Value::String(series_id.to_string()));
            }
        }
    }
}

fn json_num_field(body: &[u8], key: &str) -> Option<i64> {
    let marker = format!("\"{key}\":");
    let text = String::from_utf8_lossy(body);
    let start = text.find(&marker)? + marker.len();
    let rest = text[start..].trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
    digits.parse().ok()
}

fn json_str_field(body: &[u8], key: &str) -> Option<String> {
    let marker = format!("\"{key}\":\"");
    let text = String::from_utf8_lossy(body);
    let start = text.find(&marker)? + marker.len();
    let end = text[start..].find('"')? + start;
    Some(text[start..end].to_string())
}

// ---------------------------------------------------------------------------
// 媒體源：Range 伺服器（原 hermetic_media 三份收編於此）
// ---------------------------------------------------------------------------

fn spawn_media_server() -> String {
    let body: std::sync::Arc<Vec<u8>> =
        std::sync::Arc::new((0..2_000_000u32).map(|i| (i % 251) as u8).collect());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind media server");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let body = body.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut one = [0u8; 1];
                loop {
                    let n = stream.read(&mut one).unwrap_or(0);
                    if n == 0 || buf.len() > 16384 {
                        return;
                    }
                    buf.extend_from_slice(&one[..n]);
                    if buf.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&buf).to_string();
                let total = body.len();
                let range = request
                    .lines()
                    .find_map(|l| l.strip_prefix("Range: bytes="))
                    .map(|s| s.trim().trim_start_matches("bytes=").to_string());
                let (status, extra, slice): (&str, String, &[u8]) = match range.as_deref() {
                    Some(spec) => {
                        let mut it = spec.splitn(2, '-');
                        let s: usize = it.next().unwrap_or("").trim().parse().unwrap_or(0);
                        let e: usize = it
                            .next()
                            .unwrap_or("")
                            .trim()
                            .parse::<usize>()
                            .map(|v| (v + 1).min(total))
                            .unwrap_or(total);
                        if s < e && e <= total {
                            (
                                "206 Partial Content",
                                format!("Content-Range: bytes {}-{}/{}\r\n", s, e - 1, total),
                                &body[s..e],
                            )
                        } else {
                            ("200 OK", String::new(), &body[..])
                        }
                    }
                    None => ("200 OK", String::new(), &body[..]),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                    slice.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(slice);
            });
        }
    });
    format!("http://127.0.0.1:{port}/sample.mp4")
}
