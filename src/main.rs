//! guo — 红果短剧命令行工具。Rust 重写的 guoapp 核心：
//! 目录 / 搜索 / 详情 / 取流（App→网页→备用三层回退）/ 离线下载。

use guo::hgapp::{detail_with_fallback, search_combined, HgClient, GENRES};
use guo::model::{Chapter, Drama};
use guo::state::{EpisodeEntry, LibraryEntry, Store};
use std::collections::BTreeMap;
use std::io::Write as _;

/// mimalloc 全域分配器:egui 官方文檔實測即時模式 UI ~20% 提速
#[global_allocator]
static GLOBAL_ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// panic 现场写入数据目录 panic.log(尽力而为,失败静默)——公开版用户
/// 不会填 issue,有现场日志才可诊断。
fn install_panic_hook() {
    let default_dir = || -> std::path::PathBuf {
        if let Ok(x) = std::env::var("XDG_DATA_HOME") {
            if !x.is_empty() {
                return std::path::PathBuf::from(x).join("guoapp");
            }
        }
        if cfg!(target_os = "windows") {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_else(|_| ".".into());
            return std::path::PathBuf::from(home).join("AppData").join("Local").join("guoapp");
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        if cfg!(target_os = "macos") {
            std::path::PathBuf::from(home).join("Library/Application Support/guoapp")
        } else {
            std::path::PathBuf::from(home).join(".local/share/guoapp")
        }
    };
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let msg = format!(
            "\n[panic {}] {}\n{}\n",
            ts,
            info,
            std::backtrace::Backtrace::force_capture()
        );
        let dir = default_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("panic.log"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, msg.as_bytes()));
        default_hook(info);
    }));
}

const USAGE: &str = "\
guo — 红果短剧命令行工具（guoapp 的 Rust 重写核心）

用法:
  guo catalog [--genre short_play|comic_series|ai_series] [--pages N] [--reset] [--json]
  guo search <关键词> [--json]
  guo suggest <关键词> [--json]
  guo detail <剧ID> [--json]
  guo resolve <剧ID> <集数> [--quality N] [--json]
  guo play <剧ID> <集数> [--quality N] [--player mpv|iina|print]
  guo download <剧ID> [--eps 1-5,8] [--out 目录] [--quality N] [--json]
  guo library [--json]
  guo cookie [--no-save] [--domain 域名]

剧ID 形如 hongguo:7123456789012345678（catalog / search / detail 输出的第一列）。
数据目录: $GUOAPP_DATA 或 ~/.local/share/guoapp（macOS: ~/Library/Application Support/guoapp）。";

fn main() {
    install_panic_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        println!("{}", USAGE);
        return;
    }
    if args[0] == "-V" || args[0] == "--version" {
        println!("guo {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let mut store = Store::load();
    let client = HgClient::new();
    let rest = &args[1..];
    let code = match args[0].as_str() {
        "catalog" => catalog(&client, &mut store, rest),
        "search" => search(&client, rest),
        "suggest" => suggest(&client, rest),
        "detail" => detail(&client, &store, rest),
        "resolve" => resolve(&client, &store, rest),
        "play" => play(&client, &store, rest),
        "web-play" => web_play(&client, &store, rest),
        "download" => download(&client, &mut store, rest),
        "library" => library(&store, rest),
        "cookie" => cookie_cmd(&mut store, rest),
        "cenc-debug" => {
            let (_, flags) = parse_args(rest);
            match flags.get("file") {
                Some(file) => {
                    println!("{}", guo::cenc::inspect(std::path::Path::new(file)));
                    0
                }
                None => fail("用法：guo cenc-debug --file <mp4>"),
            }
        }
        "fetch" => fetch(&client, rest),
        other => fail(&format!("未知命令 {}（guo 查看用法）", other)),
    };
    let _ = store.save();
    std::process::exit(code);
}

fn fail(message: &str) -> i32 {
    eprintln!("错误：{}", message);
    1
}

/// Hand-rolled flag parser: positional args plus --key[ value] flags.
fn parse_args(rest: &[String]) -> (Vec<String>, BTreeMap<String, String>) {
    let mut positional = Vec::new();
    let mut flags = BTreeMap::new();
    let mut index = 0;
    while index < rest.len() {
        let arg = &rest[index];
        if let Some(name) = arg.strip_prefix("--") {
            let name = name.to_string();
            let (key, inline) = match name.split_once('=') {
                Some((k, v)) => (k.to_string(), Some(v.to_string())),
                None => (name, None),
            };
            match inline {
                Some(value) => {
                    flags.insert(key, value);
                }
                None => {
                    let next = rest.get(index + 1);
                    match next {
                        Some(value) if !value.starts_with("--") => {
                            flags.insert(key, value.clone());
                            index += 1;
                        }
                        _ => {
                            flags.insert(key, String::new());
                        }
                    }
                }
            }
        } else {
            positional.push(arg.clone());
        }
        index += 1;
    }
    (positional, flags)
}

fn flag_bool(flags: &BTreeMap<String, String>, key: &str) -> bool {
    match flags.get(key).map(|s| s.as_str()) {
        Some("false") | Some("0") | Some("no") => false,
        Some(_) => true,
        None => false,
    }
}

fn flag_num(flags: &BTreeMap<String, String>, key: &str, default: i64) -> i64 {
    flags
        .get(key)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn catalog(client: &HgClient, store: &mut Store, rest: &[String]) -> i32 {
    let (positional, flags) = parse_args(rest);
    let json = flag_bool(&flags, "json");
    let reset = flags.contains_key("reset");
    let pages = flag_num(&flags, "pages", 1).max(1) as usize;
    let genre_arg = flags.get("genre").cloned().or_else(|| positional.first().cloned());
    let genres: Vec<&guo::hgapp::Genre> = match genre_arg.as_deref() {
        Some(name) => match GENRES.iter().find(|g| g.key == name || g.name == name) {
            Some(g) => vec![g],
            None => {
                return fail("红果分类无效，可选 short_play / comic_series / ai_series");
            }
        },
        None => GENRES.iter().collect(),
    };
    let sort = flags
        .get("sort")
        .map(String::as_str)
        .unwrap_or(guo::hgapp::SORT_LATEST);
    if !guo::hgapp::valid_sort(sort) {
        return fail("排序无效，可选 online_time（最新）/ hot_score（最热）/ play_cnt（最多观看）");
    }
    let mut all: Vec<Drama> = Vec::new();
    let mut failures: Vec<String> = Vec::new();
    for g in genres {
        let feed_key = g.key.to_string();
        let mut cursor = if reset { guo::state::FeedCursor::default() } else { store.feed(&feed_key) };
        let mut collected: Vec<Drama> = Vec::new();
        let mut page_err = String::new();
        for _ in 0..pages {
            if cursor.exhausted {
                break;
            }
            match client.catalog_page(store, g, &cursor, sort) {
                Ok((items, next)) => {
                    let added = items.len();
                    let (next_offset, next_exhausted) = (next.offset, next.exhausted);
                    guo::hgweb::merge_dramas(&mut collected, items);
                    cursor = next;
                    if added == 0 || (!next_exhausted && next_offset == 0) {
                        break;
                    }
                }
                Err(err) => {
                    page_err = err;
                    break;
                }
            }
        }
        if collected.is_empty() {
            let (route, web_name) = match g.key {
                "short_play" => ("real-drama", "真人剧"),
                "comic_series" => ("comic-drama", "漫剧"),
                "ai_series" => ("ai-drama", "AI剧"),
                _ => continue,
            };
            match client.web_catalog_route(route, web_name, 1) {
                Ok(items) => collected = items,
                Err(web_err) => {
                    failures.push(format!("{}：{}；网页：{}", g.name, page_err, web_err));
                    continue;
                }
            }
        } else if !page_err.is_empty() {
            failures.push(format!("{}：{}", g.name, page_err));
        }
        store.set_feed(&feed_key, cursor);
        all.append(&mut collected);
    }
    if all.is_empty() {
        return fail(&if failures.is_empty() {
            "红果目录为空".to_string()
        } else {
            failures.join("；")
        });
    }
    if !failures.is_empty() {
        eprintln!("提示：{}", failures.join("；"));
    }
    all.sort_by(|a, b| a.title.cmp(&b.title));
    if json {
        println_json(&all);
    } else {
        for drama in &all {
            println!(
                "{}\t{}  [{}] {}  {}",
                drama.id, drama.title, drama.category, drama.remark, drama.release_status
            );
        }
        eprintln!("\n共 {} 部；游标已保存，--reset 从头扫描", all.len());
    }
    0
}

fn search(client: &HgClient, rest: &[String]) -> i32 {
    let (positional, flags) = parse_args(rest);
    let keyword = match positional.first() {
        Some(k) => k.clone(),
        None => return fail("缺少搜索词"),
    };
    let json = flag_bool(&flags, "json");
    let (dramas, warning) = match search_combined(client, &keyword) {
        Ok(result) => result,
        Err(e) => return fail(&e),
    };
    if let Some(warning) = warning {
        eprintln!("提示：{}，可重试", warning);
    }
    if json {
        println_json(&dramas);
    } else {
        for drama in &dramas {
            println!(
                "{}\t{}  [{}] {}  {}",
                drama.id, drama.title, drama.category, drama.remark, drama.release_status
            );
        }
        eprintln!("\n共 {} 部", dramas.len());
    }
    0
}

fn suggest(client: &HgClient, rest: &[String]) -> i32 {
    let (positional, flags) = parse_args(rest);
    let raw = match positional.first() {
        Some(q) => q.clone(),
        None => return fail("缺少联想词"),
    };
    let query = match guo::hgweb::validate_keyword(&raw) {
        Ok(k) => k,
        Err(e) => return fail(&e),
    };
    match client.suggestions(&query, 10) {
        Ok(records) => {
            let mut seen = std::collections::HashSet::new();
            let items: Vec<serde_json::Value> = records
                .iter()
                .filter_map(|record| {
                    let name = guo::jsonx::jstr(record, &["name"]);
                    if name.is_empty() || !seen.insert(name.to_lowercase()) {
                        return None;
                    }
                    Some(serde_json::json!({
                        "name": name,
                        "type": guo::jsonx::jstr(record, &["word_type"])
                    }))
                })
                .take(10)
                .collect();
            if flag_bool(&flags, "json") {
                println_json(&items);
            } else {
                for item in &items {
                    println!(
                        "{}\t{}",
                        item["name"].as_str().unwrap_or(""),
                        item["type"].as_str().unwrap_or("")
                    );
                }
            }
            0
        }
        Err(e) => fail(&e),
    }
}

fn fetch_detail(
    client: &HgClient,
    store: &Store,
    drama_id: &str,
) -> Result<(Drama, Vec<Chapter>), String> {
    detail_with_fallback(client, store, drama_id)
}

fn detail(client: &HgClient, store: &Store, rest: &[String]) -> i32 {
    let (positional, flags) = parse_args(rest);
    let drama_id = match positional.first() {
        Some(id) => id.clone(),
        None => return fail("缺少剧 ID"),
    };
    match fetch_detail(client, store, &drama_id) {
        Ok((drama, chapters)) => {
            if flag_bool(&flags, "json") {
                println_json(&serde_json::json!({ "drama": drama, "chapters": chapters }));
            } else {
                println!("{}  《{}》", drama.id, drama.title);
                println!(
                    "  分类：{}  备注：{}  状态：{}",
                    drama.category, drama.remark, drama.release_status
                );
                if !drama.intro.is_empty() {
                    println!("  简介：{}", truncate_text(&drama.intro, 120));
                }
                for chapter in &chapters {
                    println!("  {:>3}. {}\tvid={}", chapter.index, chapter.title, chapter.video_id);
                }
                eprintln!("\n共 {} 集", chapters.len());
            }
            0
        }
        Err(e) => fail(&e),
    }
}

fn pick_chapter(chapters: &[Chapter], episode: usize) -> Result<Chapter, String> {
    chapters
        .iter()
        .find(|c| c.index == episode)
        .cloned()
        .ok_or_else(|| format!("第 {} 集不存在（共 {} 集）", episode, chapters.len()))
}

fn pick_quality(m: guo::model::Media, quality: i64) -> guo::model::Media {
    if quality <= 0 || m.variants.is_empty() {
        return m;
    }
    let chosen = m
        .variants
        .iter()
        .find(|v| v.quality == quality)
        .or_else(|| m.variants.iter().max_by_key(|v| v.quality));
    match chosen {
        Some(variant) => guo::model::Media {
            url: variant.url.clone(),
            referer: variant.referer.clone(),
            quality: variant.quality,
            cenc_key: variant.cenc_key,
            variants: Vec::new(),
        },
        None => m,
    }
}

fn resolve(client: &HgClient, store: &Store, rest: &[String]) -> i32 {
    let (positional, flags) = parse_args(rest);
    let drama_id = match positional.first() {
        Some(id) => id.clone(),
        None => return fail("缺少剧 ID"),
    };
    let episode: usize = match positional.get(1).and_then(|e| e.parse().ok()) {
        Some(ep) => ep,
        None => return fail("缺少集数"),
    };
    let quality = flag_num(&flags, "quality", 0);
    let (drama, chapters) = match fetch_detail(client, store, &drama_id) {
        Ok(d) => d,
        Err(e) => return fail(&e),
    };
    let chapter = match pick_chapter(&chapters, episode) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let media = pick_quality(
        guo::media::resolve_media(client, store, &drama.source_id, &chapter.video_id),
        quality,
    );
    if flag_bool(&flags, "json") {
        println_json(&serde_json::json!({
            "drama": drama.title,
            "episode": chapter.index,
            "url": media.url,
            "referer": media.referer,
            "quality": media.quality,
            "key": media.cenc_key.map(hex::encode),
            "variants": media.variants.iter().map(|v| serde_json::json!({
                "url": v.url, "quality": v.quality,
                "key": v.cenc_key.map(hex::encode)
            })).collect::<Vec<_>>()
        }));
    } else {
        println!("《{}》第{}集  {}p", drama.title, chapter.index, media.quality);
        println!("地址：{}", media.url);
        println!("Referer：{}", media.referer);
        if let Some(key) = media.cenc_key {
            println!("密钥：{}", hex::encode(key));
        }
    }
    0
}

fn play(client: &HgClient, store: &Store, rest: &[String]) -> i32 {
    let (positional, flags) = parse_args(rest);
    let drama_id = match positional.first() {
        Some(id) => id.clone(),
        None => return fail("缺少剧 ID"),
    };
    let episode: usize = match positional.get(1).and_then(|e| e.parse().ok()) {
        Some(ep) => ep,
        None => return fail("缺少集数"),
    };
    let quality = flag_num(&flags, "quality", 0);
    let player = flags
        .get("player")
        .cloned()
        .unwrap_or_else(|| "print".into());
    let (drama, chapters) = match fetch_detail(client, store, &drama_id) {
        Ok(d) => d,
        Err(e) => return fail(&e),
    };
    let chapter = match pick_chapter(&chapters, episode) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    let media = pick_quality(
        guo::media::resolve_media(client, store, &drama.source_id, &chapter.video_id),
        quality,
    );
    match player.as_str() {
        "mpv" | "iina" => {
            // 与 GUI 同一路径：hwdec 硬解（防闪跳丢帧）+ 标题显示 + 预读缓冲
            let kind = if player == "iina" {
                guo::player::PlayerKind::Iina
            } else {
                guo::player::PlayerKind::Mpv
            };
            let item = guo::player::PlayItem {
                url: media.url.clone(),
                referer: media.referer.clone(),
                key: media.cenc_key.as_ref().map(hex::encode),
                title: format!("《{}》第{}集", drama.title, episode),
                // CLI 支援 --from 秒数（精确续看同款机制）
                start: flags
                    .get("from")
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(0),
            };
            let ipc = guo::player::ipc_path();
            let log_path = store.dir.join("mpv-last.log");
            match guo::player::PlayerSession::launch(
                kind,
                player.as_str(),
                &item,
                &ipc,
                &log_path,
                false, // CLI 一次性播放：播完自然退出（keepalive 是 GUI 连播形态专属）
                &store.state.player_prefs,
            ) {
                Ok(mut session) => match session.child_wait() {
                    Ok(status) if status.success() => 0,
                    Ok(status) => fail(&format!("播放器退出码 {:?}", status.code())),
                    Err(e) => fail(&format!("播放器异常：{}", e)),
                },
                Err(e) => fail(&format!("无法启动 {}：{}", player, e)),
            }
        }
        _ => {
            println!("{}", media.url);
            eprintln!(
                "提示：在线播放加密流可用 mpv --referrer=\"{}\" <URL>；离线观看请先 guo download",
                media.referer
            );
            0
        }
    }
}


/// `guo web-play <series_id> <video_id>` — 网页层取流探针：
/// 验证粘贴的网页 cookie 是否解锁播放数据（INC-20261015 的验证入口）。
fn web_play(client: &HgClient, store: &Store, rest: &[String]) -> i32 {
    if rest.len() < 2 {
        return fail("用法：guo web-play <series_id> <video_id>");
    }
    if store.state.web_cookie.is_empty() {
        eprintln!("提示：尚未设置网页 cookie（GUI 详情页🔑登录，或编辑数据目录 state.json）");
    }
    match client.web_media_with_cookie(&rest[0], &rest[1], Some(store.state.web_cookie.as_str())) {
        Ok(m) => {
            println!("✓ 网页取流成功（cookie 解锁有效）");
            println!("  地址: {}", &m.url[..80.min(m.url.len())]);
            println!("  变体数: {}", m.variants.len());
            0
        }
        Err(e) => fail(&format!("网页取流仍失败：{}（cookie 未解锁——可能需要完整登录态或该功能已下线）", e)),
    }
}

/// `guo cookie` — 从本机浏览器自动提取网页登录 cookie（INC-20261015 解锁自动化）。
/// Chrome 家族主路 + Safari 兜底；值永不全文打印，结果写入本机 state.json。
fn cookie_cmd(store: &mut Store, rest: &[String]) -> i32 {
    let (_, flags) = parse_args(rest);
    let domain = flags
        .get("domain")
        .cloned()
        .unwrap_or_else(|| guo::browser_cookies::WEB_LOGIN_DOMAIN.into());
    let save = !flag_bool(&flags, "no-save");
    println!(
        "正在从本机浏览器提取 {} 的登录 cookie（Chrome→Edge→Brave→Arc→Chromium→Safari）…\
         若弹出钥匙圈授权对话框请点「允许」",
        domain
    );
    match guo::browser_cookies::extract_login_cookie(&domain) {
        Ok(ex) => {
            println!(
                "✓ 来源：{}——{} 颗 cookie（{}）",
                ex.source,
                ex.names.len(),
                ex.names.join(", ")
            );
            if save {
                store.state.web_cookie = ex.header;
                let _ = store.save();
                println!("已写入本机数据目录 state.json（仅本机，不上传）");
                println!("验证：guo web-play <series_id> <video_id>");
            } else {
                println!("（--no-save 试提取，未写入）");
            }
            0
        }
        Err(e) => fail(&format!(
            "自动提取失败：{}\n  回退方案：浏览器登录后，在 GUI 🔑 对话框手动粘贴整行 Cookie",
            e
        )),
    }
}

fn parse_eps(spec: &str, total: usize) -> Result<Vec<usize>, String> {    let mut out = std::collections::BTreeSet::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let start: usize = start
                .trim()
                .parse()
                .map_err(|_| format!("集数范围无效：{}", part))?;
            let end: usize = end.trim().parse().map_err(|_| format!("集数范围无效：{}", part))?;
            if start < 1 || end < start || end > total {
                return Err(format!("集数范围超出：{}（共 {} 集）", part, total));
            }
            for ep in start..=end {
                out.insert(ep);
            }
        } else {
            let ep: usize = part.parse().map_err(|_| format!("集数无效：{}", part))?;
            if ep < 1 || ep > total {
                return Err(format!("集数超出：{}（共 {} 集）", ep, total));
            }
            out.insert(ep);
        }
    }
    Ok(out.into_iter().collect())
}

fn safe_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn download(client: &HgClient, store: &mut Store, rest: &[String]) -> i32 {
    let (positional, flags) = parse_args(rest);
    let drama_id = match positional.first() {
        Some(id) => id.clone(),
        None => return fail("缺少剧 ID"),
    };
    let quality = flag_num(&flags, "quality", 0);
    let (drama, chapters) = match fetch_detail(client, store, &drama_id) {
        Ok(d) => d,
        Err(e) => return fail(&e),
    };
    let episodes: Vec<usize> = match flags.get("eps") {
        Some(spec) => match parse_eps(spec, chapters.len()) {
            Ok(list) => list,
            Err(e) => return fail(&e),
        },
        None => chapters.iter().map(|c| c.index).collect(),
    };
    let root = match flags.get("out") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => store.dir.join("library").join(safe_name(&drama_id)),
    };
    let mut entry = store
        .library
        .get(&drama_id)
        .cloned()
        .unwrap_or_else(|| LibraryEntry {
            drama_id: drama_id.clone(),
            title: drama.title.clone(),
            dir: root.display().to_string(),
            entry: String::new(),
            kind: String::new(),
            episodes: Vec::new(),
        });
    entry.title = drama.title.clone();
    println!(
        "《{}》共 {} 集，待下载 {} 集 → {}",
        drama.title,
        chapters.len(),
        episodes.len(),
        root.display()
    );
    let mut ok = 0;
    let total_requested = episodes.len();
    for episode in episodes {
        let chapter = match pick_chapter(&chapters, episode) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("跳过：{}", e);
                continue;
            }
        };
        if let Some(done) = entry.episodes.iter().find(|e| e.index == episode) {
            let full = root.join(format!("ep_{:03}", episode)).join(&done.file);
            if full.exists() {
                println!("第{}集 已存在，跳过", episode);
                ok += 1;
                continue;
            }
        }
        let media = pick_quality(
            guo::media::resolve_media(client, store, &drama.source_id, &chapter.video_id),
            quality,
        );
        let dir = root.join(format!("ep_{:03}", episode));
        match guo::media::download_episode(client, &media, &dir, quality, &format!("第{}集", episode)) {
            Ok((file, kind, got_quality)) => {
                entry.episodes.retain(|e| e.index != episode);
                entry.episodes.push(EpisodeEntry {
                    index: episode,
                    quality: got_quality,
                    file: file.clone(),
                });
                entry.episodes.sort_by_key(|e| e.index);
                entry.entry = file;
                entry.kind = kind.to_string();
                entry.dir = root.display().to_string();
                store.library.insert(drama_id.clone(), entry.clone());
                let _ = store.save();
                ok += 1;
                println!("第{}集 完成（{}，{}p）", episode, kind, got_quality);
            }
            Err(e) => eprintln!("第{}集 失败：{}", episode, e),
        }
    }
    if flag_bool(&flags, "json") {
        println_json(&entry);
    }
    if ok == total_requested {
        println!("全部 {} 集完成", ok);
        0
    } else {
        println!("完成 {}/{} 集；失败可重跑同一命令续传", ok, total_requested);
        if ok == 0 {
            1
        } else {
            0
        }
    }
}

/// Hidden debug helper: download any media URL (mp4 / m3u8) with CENC decrypt.
fn fetch(client: &HgClient, rest: &[String]) -> i32 {
    let (positional, flags) = parse_args(rest);
    let url = match positional.first() {
        Some(u) => u.clone(),
        None => return fail("用法：guo fetch <URL> [--out 目录] [--referer URL] [--key 十六进制]"),
    };
    let referer = flags
        .get("referer")
        .cloned()
        .unwrap_or_else(|| "https://novel.snssdk.com/".into());
    let key = match flags.get("key") {
        Some(hex_key) => match hex::decode(hex_key) {
            Ok(bytes) if bytes.len() == 16 => {
                Some(<[u8; 16]>::try_from(bytes).unwrap_or([0u8; 16]))
            }
            _ => return fail("密钥必须是 32 位十六进制字符"),
        },
        None => None,
    };
    let out = flags
        .get("out")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("guo-fetch"));
    let media = guo::model::Media {
        url,
        referer,
        quality: 0,
        cenc_key: key,
        variants: Vec::new(),
    };
    match guo::media::download_episode(client, &media, &out, 0, "fetch") {
        Ok((file, kind, _)) => {
            println!("{}/{}（{}）", out.display(), file, kind);
            0
        }
        Err(e) => fail(&e),
    }
}

fn library(store: &Store, rest: &[String]) -> i32 {    let (_, flags) = parse_args(rest);
    if flag_bool(&flags, "json") {
        let items: Vec<&LibraryEntry> = store.library.values().collect();
        println_json(&items);
        return 0;
    }
    if store.library.is_empty() {
        println!("本地片库为空");
        return 0;
    }
    for entry in store.library.values() {
        println!(
            "{}  《{}》  {}集  {}",
            entry.drama_id,
            entry.title,
            entry.episodes.len(),
            entry.dir
        );
    }
    0
}

fn println_json<T: serde::Serialize>(value: &T) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = writeln!(
        lock,
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into())
    );
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let cut: String = text.chars().take(max_chars).collect();
    format!("{}…", cut.trim_end())
}
