//! Hongguo web fallback (`hongguoduanju.com`): `_ROUTER_DATA` JSON embedded in
//! the page provides catalog / detail / player / search data without the app
//! signing. Mirrors provider_hongguo web paths.

use crate::hgapp::{drama_from_value, media_addresses, HgClient, WEB_BASE, WEB_UA};
use crate::jsonx::{jbool, jlist, jmap, jnum_str, jstr};
use crate::model::{is_numeric_id, Chapter, Drama, Media, Variant};
use serde_json::Value;
use std::time::Duration;

impl HgClient {
    fn fetch_page(&self, url: &str) -> Result<String, String> {
        self.fetch_page_with_cookie(url, None)
    }

    /// 登录会话版：带用户粘贴的红果网页 cookie(播放页需登录态才有播放数据)。
    fn fetch_page_with_cookie(&self, url: &str, cookie: Option<&str>) -> Result<String, String> {
        let mut request = self
            .agent
            .get(url)
            .timeout(Duration::from_secs(20))
            .set("User-Agent", WEB_UA)
            .set("Referer", &format!("{}/", WEB_BASE))
            .set("Accept", "text/html,application/json")
            .set("Accept-Language", "zh-CN,zh;q=0.9");
        if let Some(c) = cookie.filter(|c| !c.trim().is_empty()) {
            request = request.set("Cookie", c.trim());
        }
        let response = request.call();
        match response {
            Ok(r) => {
                let text = r
                    .into_string()
                    .map_err(|_| "红果网页响应读取失败".to_string())?;
                if text.len() > 20 * 1024 * 1024 {
                    return Err("红果网页响应过大".into());
                }
                Ok(text)
            }
            Err(ureq::Error::Status(code, _)) => Err(format!("红果网页 HTTP {}", code)),
            Err(e) => Err(format!("红果网页请求失败：{}", e)),
        }
    }
}

/// Extract the first JSON value after `_ROUTER_DATA =` and ignore trailing JS.
pub fn parse_router_data(raw: &str) -> Option<Value> {
    let marker = "_ROUTER_DATA";
    let start = raw.find(marker)? + marker.len();
    let rest = raw[start..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let mut de = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
    de.next()?.ok()
}

/// `routerLoaderMap`: exact key first, then any key with the trimmed prefix.
pub fn loader_map<'a>(data: &'a Value, names: &[&str]) -> Option<&'a Value> {
    let loader = data.get("loaderData")?;
    for name in names {
        if let Some(page) = loader.get(*name) {
            if page.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                return Some(page);
            }
        }
    }
    for (key, value) in loader.as_object()? {
        for name in names {
            let prefix = name.strip_suffix('$').unwrap_or(name);
            if !prefix.is_empty() && key.starts_with(prefix)
                && value.as_object().is_some_and(|o| !o.is_empty())
            {
                return Some(value);
            }
        }
    }
    None
}

impl HgClient {
    /// Web catalog: four routes fetched like the original (all pages until a
    /// page underfills). One route per call; caller fans out.
    pub fn web_catalog_route(
        &self,
        route: &str,
        category: &str,
        max_pages: usize,
    ) -> Result<Vec<Drama>, String> {
        let mut dramas = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut pages = max_pages.max(1);
        let mut page = 1;
        while page <= pages {
            let body = self.fetch_page(&format!("{}/category/{}?page={}", WEB_BASE, route, page))?;
            let data = parse_router_data(&body).ok_or("红果分类数据不可用")?;
            let loader =
                loader_map(&data, &["category_page", "category_$"]).ok_or("红果分类数据不可用")?;
            if jbool(loader, "isSuccess") == Some(false) {
                return Err("红果分类数据不可用，可能是页面结构或访问权限变化".into());
            }
            let items = jlist(loader.get("recommendList").unwrap_or(&Value::Null));
            let item_count = items.len();
            let mut added = 0;
            for item in items {
                let drama = drama_from_value(item, category);
                if !drama.id.is_empty() && seen.insert(drama.id.clone()) {
                    dramas.push(drama);
                    added += 1;
                }
            }
            let total_pages = jmap(loader, &["pagination"])
                .get("totalPages")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(0);
            if total_pages > pages {
                pages = total_pages.min(500);
            }
            if added == 0 || item_count < 24 {
                break;
            }
            page += 1;
        }
        dramas.sort_by(|a, b| a.title.cmp(&b.title));
        Ok(dramas)
    }

    /// Web detail: seriesDetail.vid_list becomes chapters.
    pub fn web_detail(&self, series_id: &str) -> Result<(String, Vec<Chapter>), String> {
        let body = self.fetch_page(&format!(
            "{}/detail?series_id={}",
            WEB_BASE,
            urlencode(series_id)
        ))?;
        let data = parse_router_data(&body).ok_or("hongguo detail is empty")?;
        let page = loader_map(&data, &["detail_page", "detail_"]).ok_or("hongguo detail is empty")?;
        let detail = jmap(page, &["seriesDetail"]);
        if detail.as_object().map(|o| o.is_empty()).unwrap_or(true) {
            return Err("hongguo detail is empty".into());
        }
        let title = {
            let t = jstr(detail, &["series_name", "series_title", "name"]);
            if t.is_empty() {
                series_id.to_string()
            } else {
                t
            }
        };
        let mut chapters = Vec::new();
        for (index, vid) in jlist(detail.get("vid_list").unwrap_or(&Value::Null)).iter().enumerate()
        {
            let video_id = match vid.as_str() {
                Some(s) => s.trim().to_string(),
                None => match vid.as_i64() {
                    Some(n) => n.to_string(),
                    None => continue,
                },
            };
            if video_id.is_empty() {
                continue;
            }
            let idx = index + 1;
            chapters.push(Chapter {
                local: false,
                index: idx,
                title: format!("第{}集", idx),
                video_id,
            });
        }
        if chapters.is_empty() {
            return Err("红果详情没有返回剧集 ID".into());
        }
        Ok((title, chapters))
    }

    /// Web player: returns media when the page exposes public addresses.
    pub fn web_media_with_cookie(
        &self,
        series_id: &str,
        video_id: &str,
        cookie: Option<&str>,
    ) -> Result<Media, String> {
        let page_url = format!(
            "{}/player/{}/{}/",
            WEB_BASE,
            urlencode(series_id),
            urlencode(video_id)
        );
        let body = self.fetch_page_with_cookie(&page_url, cookie)?;
        let data = parse_router_data(&body).ok_or("红果播放页数据不可用")?;
        let page = loader_map(&data, &["player_", "player_page"]).ok_or("红果播放页数据不可用")?;
        if jstr(page, &["vid"]) != video_id || jstr(page, &["series_id"]) != series_id {
            return Err("红果未返回所请求的剧集，可能仅允许网页试看".into());
        }
        let info = jmap(page, &["video_player_info"]);
        let addresses = media_addresses(info);
        if addresses.is_empty() {
            return Err("红果该集未提供公开播放地址，可能需要登录或 App 授权".into());
        }
        let duration = jstr(info, &["duration"]).parse::<f64>().unwrap_or(0.0);
        let mut media = Media {
            url: addresses[0].clone(),
            referer: format!("{}/", WEB_BASE),
            quality: 0,
            cenc_key: None,
            variants: Vec::new(),
        };
        for address in addresses {
            media.variants.push(Variant {
                url: address,
                referer: media.referer.clone(),
                quality: 0,
                cenc_key: None,
            });
        }
        let _ = duration;
        Ok(media)
    }

    /// Web search page.
    pub fn web_search(&self, keyword: &str) -> Result<(Vec<Drama>, i64), String> {
        let body = self.fetch_page(&format!("{}/search/{}", WEB_BASE, urlencode(keyword)))?;
        let data = parse_router_data(&body).ok_or("红果搜索未返回有效结果")?;
        let page = loader_map(&data, &["search_(keyword)/page", "search_"])
            .ok_or("红果搜索未返回有效结果")?;
        if jbool(page, "isSuccess") != Some(true) || jstr(page, &["query"]) != keyword {
            return Err("红果搜索未返回有效结果".into());
        }
        let rows = page
            .get("searchList")
            .and_then(|v| v.as_array())
            .ok_or("红果搜索未返回有效结果")?;
        let mut dramas = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            if row.get("video_data").and_then(|v| v.as_object()).map(|o| o.is_empty()).unwrap_or(true) {
                continue;
            }
            let drama = drama_from_value(row, "短剧");
            if !drama.id.is_empty() && seen.insert(drama.id.clone()) {
                dramas.push(drama);
            }
        }
        if !rows.is_empty() && dramas.is_empty() {
            return Err("红果搜索结果中没有可识别的剧集".into());
        }
        let total = jnum_str(page, &["totalCount"]).parse::<i64>().unwrap_or(0);
        let count = dramas.len() as i64;
        Ok((dramas, total.max(count)))
    }

    /// Suggestion endpoint; doubles as the "search by name" index.
    pub fn suggestions(&self, query: &str, count: usize) -> Result<Vec<Value>, String> {
        let url = format!(
            "{}/incent_resource/suggestion?app_id=8662&query={}&count={}",
            WEB_BASE,
            urlencode(query),
            count
        );
        let response = self
            .agent
            .get(&url)
            .timeout(Duration::from_secs(5))
            .set("User-Agent", WEB_UA)
            .set("Referer", &format!("{}/", WEB_BASE))
            .set("Accept", "application/json")
            .set("Accept-Language", "zh-CN,zh;q=0.9")
            .call();
        let body = match response {
            Ok(r) => r.into_string().map_err(|_| "红果搜索联想响应过大".to_string())?,
            Err(ureq::Error::Status(code, _)) => return Err(format!("红果搜索联想 HTTP {}", code)),
            Err(e) => return Err(format!("红果搜索联想请求失败：{}", e)),
        };
        if body.len() > 256 * 1024 {
            return Err("红果搜索联想响应过大".into());
        }
        let result: Value =
            serde_json::from_str(&body).map_err(|_| "红果搜索联想未返回有效数据".to_string())?;
        let items = result
            .get("suggest_list")
            .and_then(|v| v.as_array())
            .or_else(|| {
                result
                    .get("data")
                    .and_then(|d| d.get("suggest_list"))
                    .and_then(|v| v.as_array())
            })
            .ok_or("红果搜索联想未返回有效数据")?;
        Ok(items.clone())
    }
}

pub fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

pub fn validate_keyword(keyword: &str) -> Result<String, String> {
    let keyword = keyword.trim();
    if keyword.is_empty() || keyword.chars().count() > 80 {
        return Err("请输入 1 至 80 个字符的搜索词".into());
    }
    if keyword.chars().any(|c| c.is_control()) {
        return Err("搜索词包含非法字符".into());
    }
    Ok(keyword.to_string())
}

/// Search-by-name from suggestion records (word_type == short_play_name).
pub fn search_names(client: &HgClient, keyword: &str) -> Result<Vec<Drama>, String> {
    let records = client.suggestions(keyword, 50)?;
    let mut dramas = Vec::new();
    for record in &records {
        if jstr(record, &["word_type"]) != "short_play_name" {
            continue;
        }
        let video_data = record.get("video_data").cloned().unwrap_or(Value::Null);
        let source_id = jnum_str(&video_data, &["series_id_str", "series_id"]);
        if !is_numeric_id(&source_id) {
            continue;
        }
        let wrapped = serde_json::json!({
            "video_data": video_data,
            "name": record.get("name"),
            "keyword": record.get("keyword")
        });
        let drama = drama_from_value(&wrapped, "短剧");
        if !drama.id.is_empty() && drama.title != drama.source_id {
            dramas.push(drama);
        }
    }
    Ok(dramas)
}

/// Search rank port: exact < prefix < contains < all-words < no match.
pub fn title_search_rank(title: &str, query: &str) -> u8 {
    let query_norm = search_text(query);
    let title_norm = search_text(title);
    if title_norm == query_norm {
        return 0;
    }
    if title_norm.starts_with(&query_norm) {
        return 1;
    }
    if title_norm.contains(&query_norm) {
        return 2;
    }
    let words: Vec<&str> = query
        .split(|c: char| c.is_whitespace() || c.is_ascii_punctuation())
        .filter(|w| !w.is_empty())
        .collect();
    let matches = words
        .iter()
        .filter(|w| title_norm.contains(&search_text(w)))
        .count();
    if !words.is_empty() && matches == words.len() {
        3
    } else {
        4
    }
}

fn search_text(text: &str) -> String {
    let normalized: String = text
        .chars()
        .filter(|c| !c.is_whitespace() && !is_punct(*c))
        .flat_map(|c| c.to_lowercase())
        .collect();
    if normalized.is_empty() {
        text.trim().to_lowercase()
    } else {
        normalized
    }
}

fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation()
        || matches!(
            c,
            '，' | '。' | '！' | '？' | '：' | '；' | '、' | '（' | '）' | '《' | '》' | '“' | '”'
                | '‘' | '’' | '·' | '—' | '…' | '【' | '】'
        )
}

/// Merge two drama lists by id, keeping non-empty fields of `preferred`.
pub fn merge_dramas(base: &mut Vec<Drama>, batch: Vec<Drama>) {
    let mut positions = std::collections::HashMap::new();
    for (index, drama) in base.iter().enumerate() {
        positions.insert(drama.id.clone(), index);
    }
    for drama in batch {
        if drama.id.is_empty() {
            continue;
        }
        match positions.get(&drama.id) {
            Some(&index) => {
                let target = &mut base[index];
                if target.title.is_empty() {
                    target.title = drama.title.clone();
                }
                if target.cover.is_empty() {
                    target.cover = drama.cover.clone();
                }
                if target.intro.is_empty() {
                    target.intro = drama.intro.clone();
                }
                if target.remark.is_empty() {
                    target.remark = drama.remark.clone();
                }
                if target.episode_count.is_empty() {
                    target.episode_count = drama.episode_count.clone();
                }
                if target.heat.is_empty() {
                    target.heat = drama.heat.clone();
                }
                if target.online_date.is_empty() {
                    target.online_date = drama.online_date.clone();
                }
                if target.tags.is_empty() {
                    target.tags = drama.tags.clone();
                }
            }
            None => {
                positions.insert(drama.id.clone(), base.len());
                base.push(drama);
            }
        }
    }
}
