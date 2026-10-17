//! Media resolution fallback chain (App → Web → backup API) and downloads:
//! direct files with Range resume, HLS bundles rewritten for offline play.

use crate::hgapp::HgClient;
use crate::model::{is_http_media_url, Media};
use crate::state::Store;
use sha2::Digest;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Resolve playable media for one episode through all three fallbacks.
/// Returns the resolved media; callers decide how to surface errors.
pub fn try_resolve_media(
    client: &HgClient,
    store: &Store,
    series_id: &str,
    video_id: &str,
) -> Result<Media, String> {
    // 测试/离线注入:GUO_RESOLVE_OVERRIDE 指向任意媒体(本地文件或 HTTP URL),
    // 三层取流短路返回。上游锁定期间(INC-20261015),UI 测试不再依赖第三方可用性。
    if let Ok(url) = std::env::var("GUO_RESOLVE_OVERRIDE") {
        if !url.trim().is_empty() {
            return Ok(Media {
                url: url.trim().to_string(),
                referer: String::new(),
                quality: 0,
                cenc_key: None,
                variants: Vec::new(),
            });
        }
    }
    let mut errors = Vec::new();
    // 无签名 multi_video_model(2026-10-17 社区考古):收费集全画质免登录——
    // 上游只锁了带签名的 video_model,这一路是当前的主取流层
    match client.multi_video_model(video_id) {
        Ok(media) => return Ok(media),
        Err(err) => errors.push(format!("multi 取流：{}", err)),
    }
    match client.media(store, video_id) {
        Ok(media) => return Ok(media),
        Err(err) => errors.push(format!("App 取流：{}", err)),
    }
    // 网页层带登录会话 cookie(用户在 GUI 粘贴过 → 解锁登录态播放数据)
    match client.web_media_with_cookie(series_id, video_id, Some(store.state.web_cookie.as_str())) {
        Ok(media) => return Ok(media),
        Err(err) => errors.push(format!("网页取流：{}", err)),
    }
    // 匿名公开端点:免费试看集直出完整 360p(2026-10-17 前端 bundle 考古;
    // 收费集 404 快速失败,留给出备用中继)
    match client.public_media(video_id) {
        Ok(media) => return Ok(media),
        Err(err) => errors.push(format!("公开取流：{}", err)),
    }
    match client.backup_media(series_id, video_id) {
        Ok(media) => return Ok(media),
        Err(err) => errors.push(format!("备用取流：{}", err)),
    }
    Err(format!("红果取流失败：{}", errors.join("；")))
}

/// CLI variant: prints the combined error and exits.
pub fn resolve_media(client: &HgClient, store: &Store, series_id: &str, video_id: &str) -> Media {
    match try_resolve_media(client, store, series_id, video_id) {
        Ok(media) => media,
        Err(err) => {
            eprintln!("{}", err);
            std::process::exit(1);
        }
    }
}

struct Segment {
    url: String,
    name: String,
}

struct Bundle {
    playlists: Vec<(String, String)>,
    segments: Vec<Segment>,
    quality: i64,
}

fn asset_name(url: &str, fallback_ext: &str) -> String {
    let digest = sha2::Sha256::digest(url.as_bytes());
    format!("{}{}", hex::encode(&digest[..16]), fallback_ext)
}

fn ext_of(url: &str) -> &'static str {
    let path = url::Url::parse(url).map(|u| u.path().to_string()).unwrap_or_default();
    let lower = path.to_lowercase();
    for ext in [".ts", ".mp4", ".m4s", ".aac", ".m4a", ".vtt", ".key", ".m3u8"] {
        if lower.ends_with(ext) {
            return match ext {
                ".m3u8" => ".m3u8",
                _ => ext,
            };
        }
    }
    let last = lower.rsplit('.').next().unwrap_or("");
    if last.len() <= 5 && last.chars().all(|c| c.is_ascii_alphanumeric()) && !last.is_empty() {
        return ".bin";
    }
    ".bin"
}

/// Download an HLS playlist tree into `out_dir` as an offline-playable bundle.
/// Mirrors the original: pick best variant, dedupe assets by content hash of
/// URL, rewrite every reference to a local file name, enforce AES-128 keys.
fn download_bundle(
    client: &HgClient,
    media: &Media,
    quality: i64,
    out_dir: &Path,
    label: &str,
) -> Result<Bundle, String> {
    let mut playlists: Vec<(String, String)> = Vec::new();
    let mut segments: Vec<Segment> = Vec::new();
    let mut seen_assets = std::collections::HashSet::new();
    let mut bundle_quality = media.quality;

    #[allow(clippy::too_many_arguments)] // HLS 遍歷上下文參數天然多;重構簽名風險大於收益
    fn walk(
        client: &HgClient,
        media: &Media,
        quality: i64,
        address: &str,
        depth: usize,
        bundle_quality: &mut i64,
        playlists: &mut Vec<(String, String)>,
        segments: &mut Vec<Segment>,
        seen_assets: &mut std::collections::HashSet<String>,
    ) -> Result<String, String> {
        if depth > 6 || playlists.len() > 100 {
            return Err("播放列表嵌套过多".into());
        }
        let body = fetch_playlist(client, address, &media.referer)?;
        let mut lines: Vec<String> = body
            .lines()
            .map(|l| l.trim().to_string())
            .collect::<Vec<_>>();
        let mut is_master = false;
        if let Some((kept, height)) = select_variant(&lines, quality) {
            if depth == 0 && height > 0 {
                *bundle_quality = height;
            }
            lines = kept;
            is_master = true;
        }
        if !is_master && !lines.iter().any(|l| l.starts_with("#EXT-X-ENDLIST")) {
            return Err("暂不支持下载直播或尚未结束的播放列表".into());
        }
        let base = url::Url::parse(address).map_err(|_| "播放列表地址无效".to_string())?;
        let mut output: Vec<String> = Vec::with_capacity(lines.len());
        let mut next_is_playlist = false;
        for line in &lines {
            if line.starts_with("#EXT-X-DEFINE:") || line.contains("{$") {
                return Err("暂不支持下载使用变量地址的播放列表".into());
            }
            let is_key = line.starts_with("#EXT-X-KEY:") || line.starts_with("#EXT-X-SESSION-KEY:");
            let mut working = line.clone();
            if !line.starts_with('#') {
                let resolved = base.join(line).map_err(|_| "分片地址无效".to_string())?.to_string();
                if next_is_playlist {
                    let name = walk(
                        client, media, quality, &resolved, depth + 1, bundle_quality, playlists,
                        segments, seen_assets,
                    )?;
                    working = name;
                    next_is_playlist = false;
                } else {
                    let name = asset_name(&resolved, ext_of(&resolved));
                    if !name.ends_with(".m3u8") && seen_assets.insert(name.clone()) {
                        segments.push(Segment { url: resolved, name: name.clone() });
                    }
                    working = name;
                }
            } else if working.contains("URI=\"") {
                if let Some(start) = working.find("URI=\"") {
                    let after = &working[start + 5..];
                    let end = after.find('"').ok_or("内嵌地址无效")?;
                    let reference = &after[..end];
                    let resolved = base
                        .join(reference)
                        .map_err(|_| "分片地址无效".to_string())?
                        .to_string();
                    let name;
                    if next_is_playlist || working.starts_with("#EXT-X-MEDIA:") {
                        name = walk(
                            client, media, quality, &resolved, depth + 1, bundle_quality,
                            playlists, segments, seen_assets,
                        )?;
                        next_is_playlist = false;
                    } else if is_key {
                        if working.contains("METHOD=NONE") {
                            name = String::new();
                        } else {
                            name = format!("{}.key", hex::encode(
                                &sha2::Sha256::digest(resolved.as_bytes())[..16]
                            ));
                            if seen_assets.insert(name.clone()) {
                                segments.push(Segment { url: resolved, name: name.clone() });
                            }
                        }
                    } else {
                        name = asset_name(&resolved, ext_of(&resolved));
                        if !name.ends_with(".m3u8") && seen_assets.insert(name.clone()) {
                            segments.push(Segment { url: resolved, name: name.clone() });
                        }
                    }
                    let replaced = working.replacen(
                        &format!("URI=\"{}\"", reference),
                        &format!("URI=\"{}\"", name),
                        1,
                    );
                    working = replaced; // one URI per line in practice
                }
            }
            if working.starts_with("#EXT-X-STREAM-INF:") {
                next_is_playlist = true;
            }
            // Reject non-AES-128 encryption up front.
            if is_key {
                if let Some(method) = attribute(working.trim(), "METHOD") {
                    if method != "AES-128" && method != "NONE" {
                        return Err("暂不支持离线保存该视频的加密方式".into());
                    }
                }
                if let Some(format) = attribute(working.trim(), "KEYFORMAT") {
                    if format != "identity" {
                        return Err("该视频需要源站授权，暂不支持离线保存".into());
                    }
                }
            }
            output.push(working);
        }
        let name = asset_name(address, ".m3u8");
        playlists.push((name.clone(), output.join("\n") + "\n"));
        Ok(name)
    }

    let root = walk(
        client, media, quality, &media.url, 0, &mut bundle_quality, &mut playlists, &mut segments,
        &mut seen_assets,
    )?;
    let _ = (out_dir, label, root);
    Ok(Bundle { playlists, segments, quality: bundle_quality })
}

fn fetch_playlist(client: &HgClient, address: &str, referer: &str) -> Result<String, String> {
    let response = client
        .agent
        .get(address)
        .timeout(std::time::Duration::from_secs(20))
        .set("User-Agent", crate::hgapp::WEB_UA)
        .set("Referer", referer)
        .call();
    let text = match response {
        Ok(r) => r.into_string().map_err(|_| "播放列表响应过大".to_string())?,
        Err(ureq::Error::Status(code, _)) => return Err(format!("播放列表 HTTP {}", code)),
        Err(e) => return Err(format!("播放列表请求失败：{}", e)),
    };
    let text = text.trim_start_matches('\u{feff}').trim().to_string();
    if !text.starts_with("#EXTM3U") || text.len() > 4 << 20 {
        return Err("下载播放列表无效".into());
    }
    Ok(text)
}

fn attribute(line: &str, key: &str) -> Option<String> {
    let needle = format!("{}=", key);
    for part in line.split(',') {
        let part = part.trim();
        if let Some(value) = part.strip_prefix(&needle) {
            return Some(value.trim_matches('"').to_string());
        }
    }
    None
}

/// Master playlist variant selection: prefer exact height match, else highest.
/// Also prunes EXT-X-MEDIA groups to the selected rendition (DEFAULT first).
fn select_variant(lines: &[String], quality: i64) -> Option<(Vec<String>, i64)> {
    struct VariantInfo {
        line: usize,
        uri: usize,
        height: i64,
        bandwidth: i64,
        group_for: std::collections::HashMap<String, String>,
    }
    let mut variants: Vec<VariantInfo> = Vec::new();
    let mut excluded = std::collections::HashSet::new();
    for (index, line) in lines.iter().enumerate() {
        if !line.starts_with("#EXT-X-STREAM-INF:") {
            continue;
        }
        let height = attribute(line, "RESOLUTION")
            .and_then(|r| {
                r.split('x')
                    .nth(1)
                    .and_then(|h| h.trim().parse::<i64>().ok())
            })
            .unwrap_or(0);
        let bandwidth = attribute(line, "BANDWIDTH")
            .and_then(|b| b.parse::<i64>().ok())
            .unwrap_or(0);
        let mut group_for = std::collections::HashMap::new();
        for kind in ["AUDIO", "VIDEO", "SUBTITLES", "CLOSED-CAPTIONS"] {
            if let Some(group) = attribute(line, kind) {
                group_for.insert(kind.to_string(), group);
            }
        }
        for (next, text) in lines[index + 1..].iter().enumerate().map(|(i, l)| (i + index + 1, l.trim())) {
            if text.is_empty() || text.starts_with('#') {
                continue;
            }
            variants.push(VariantInfo { line: index, uri: next, height, bandwidth, group_for });
            excluded.insert(index);
            excluded.insert(next);
            break;
        }
    }
    if variants.is_empty() {
        return None;
    }
    let mut best = 0;
    for (index, candidate) in variants.iter().enumerate().skip(1) {
        let best_v = &variants[best];
        let exact = quality > 0 && candidate.height == quality;
        let best_exact = quality > 0 && best_v.height == quality;
        let better = (exact && !best_exact)
            || (exact == best_exact
                && (candidate.height > best_v.height
                    || (candidate.height == best_v.height && candidate.bandwidth > best_v.bandwidth)));
        if better {
            best = index;
        }
    }
    let chosen = &variants[best];
    excluded.remove(&chosen.line);
    excluded.remove(&chosen.uri);
    // Keep at most one EXT-X-MEDIA per TYPE, preferring DEFAULT=YES inside the
    // chosen variant's group; drop I-FRAME / SESSION-DATA lines entirely.
    let mut groups: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (index, line) in lines.iter().enumerate() {
        if !line.starts_with("#EXT-X-MEDIA:") {
            continue;
        }
        let kind = attribute(line, "TYPE").unwrap_or_default();
        let group = attribute(line, "GROUP-ID").unwrap_or_default();
        if let Some(want) = chosen.group_for.get(&kind) {
            if &group != want {
                excluded.insert(index);
                continue;
            }
        }
        let is_default = attribute(line, "DEFAULT").as_deref() == Some("YES");
        match groups.get(&kind) {
            Some(&kept) => {
                let kept_default =
                    attribute(&lines[kept], "DEFAULT").as_deref() == Some("YES");
                if is_default && !kept_default {
                    excluded.insert(kept);
                    groups.insert(kind.clone(), index);
                } else {
                    excluded.insert(index);
                }
            }
            None => {
                groups.insert(kind.clone(), index);
            }
        }
    }
    let kept: Vec<String> = lines
        .iter()
        .enumerate()
        .filter(|(index, line)| {
            if excluded.contains(index) {
                return false;
            }
            let text = line.trim();
            !text.starts_with("#EXT-X-I-FRAME-STREAM-INF:")
                && !text.starts_with("#EXT-X-SESSION-DATA:")
        })
        .map(|(_, line)| line.clone())
        .collect();
    Some((kept, chosen.height))
}

struct Progress {
    label: String,
    start: Instant,
    last_print: Instant,
}

impl Progress {
    fn new(label: &str) -> Progress {
        Progress {
            label: label.to_string(),
            start: Instant::now(),
            last_print: Instant::now(),
        }
    }
    fn report(&mut self, received: u64, total: u64) {
        let now = Instant::now();
        if now.duration_since(self.last_print).as_millis() < 200 && (total == 0 || received < total)
        {
            return;
        }
        self.last_print = now;
        let secs = now.duration_since(self.start).as_secs_f64().max(0.001);
        let speed = received as f64 / secs / 1024.0;
        if total > 0 {
            eprint!(
                "\r  {} {:>5.1}%  {:>8.1} MB/s   ",
                self.label,
                received as f64 / total as f64 * 100.0,
                speed
            );
        } else {
            eprint!("\r  {} {:>10}  {:>8.1} MB/s   ", self.label, human(received), speed);
        }
    }
    fn done(&self) {
        eprintln!(
            "\r  {} 完成（{:.1}s）        ",
            self.label,
            self.start.elapsed().as_secs_f64()
        );
    }
}

fn human(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.2} GB", bytes as f64 / (1 << 30) as f64)
    } else if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1 << 20) as f64)
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

fn write_atomic(target: &Path, data: &[u8]) -> Result<(), String> {
    let part = PathBuf::from(format!("{}.part", target.display()));
    std::fs::write(&part, data).map_err(|e| format!("写入文件失败：{}", e))?;
    std::fs::rename(&part, target).map_err(|e| format!("落盘失败：{}", e))
}

fn download_segment(
    client: &HgClient,
    url: &str,
    referer: &str,
    target: &Path,
) -> Result<u64, String> {
    if let Ok(meta) = std::fs::metadata(target) {
        if meta.len() > 0 {
            return Ok(meta.len());
        }
    }
    let response = client
        .agent_dl
        .get(url)
        .set("User-Agent", crate::hgapp::WEB_UA)
        .set("Referer", referer)
        .call();
    let mut reader = match response {
        Ok(r) => r.into_reader(),
        Err(ureq::Error::Status(code, _)) => return Err(format!("分片 HTTP {}", code)),
        Err(e) => return Err(format!("分片请求失败：{}", e)),
    };
    let part = PathBuf::from(format!("{}.part", target.display()));
    let mut file = std::fs::File::create(&part).map_err(|e| format!("写入文件失败：{}", e))?;
    let mut buffer = [0u8; 64 << 10];
    let mut received: u64 = 0;
    loop {
        let n = reader.read(&mut buffer).map_err(|e| format!("分片读取失败：{}", e))?;
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n]).map_err(|e| format!("写入文件失败：{}", e))?;
        received += n as u64;
    }
    file.sync_all().ok();
    drop(file);
    std::fs::rename(&part, target).map_err(|e| format!("落盘失败：{}", e))?;
    Ok(received)
}

/// Download one episode into `out_dir`; returns the entry file name and quality.
pub fn download_episode(
    client: &HgClient,
    media: &Media,
    out_dir: &Path,
    quality: i64,
    label: &str,
) -> Result<(String, String, i64), String> {
    download_episode_progress(client, media, out_dir, quality, label, &mut |_| {})
}

/// Same as `download_episode`, reporting 0..1 progress to the callback.
pub fn download_episode_progress(
    client: &HgClient,
    media: &Media,
    out_dir: &Path,
    quality: i64,
    label: &str,
    on_progress: &mut dyn FnMut(f32),
) -> Result<(String, String, i64), String> {
    std::fs::create_dir_all(out_dir).map_err(|e| format!("创建目录失败：{}", e))?;
    let parsed = url::Url::parse(&media.url).map_err(|_| "下载地址无效".to_string())?;
    let is_hls = parsed.path().to_lowercase().ends_with(".m3u8");
    let mut progress = Progress::new(label);
    if is_hls {
        let bundle = download_bundle(client, media, quality, out_dir, label)?;
        let total = bundle.segments.len().max(1);
        for (index, segment) in bundle.segments.iter().enumerate() {
            let target = out_dir.join(&segment.name);
            download_segment(client, &segment.url, &media.referer, &target)
                .map_err(|e| format!("分片 {}/{} 下载失败：{}", index + 1, total, e))?;
            progress.report((index + 1) as u64, total as u64);
            on_progress((index + 1) as f32 / total as f32);
        }
        for (name, body) in &bundle.playlists {
            write_atomic(&out_dir.join(name), body.as_bytes())?;
        }
        progress.done();
        Ok(("index.m3u8".into(), "hls".into(), bundle.quality))
    } else {
        let target = out_dir.join("media.mp4");
        // 多段并行：服务器支持 Range 且文件够大时 4 连线拉满带宽（限速 CDN 实测 2.5-3.5×）；
        // 探测失败/文件太小/中途失败 → 自动回退单连线（原 .part 续传语义不变）
        let size = match download_direct_parallel(client, &media.url, &media.referer, &target, on_progress) {
            Ok(size) => size,
            Err(_) => {
                download_direct(client, &media.url, &media.referer, &target, &mut progress, on_progress)?
            }
        };
        progress.done();
        if let Some(key) = media.cenc_key {
            crate::cenc::decrypt_file(&target, &key)?;
        }
        let _ = size;
        Ok(("media.mp4".into(), "mp4".into(), media.quality))
    }
}

// ── 多段 Range 并行下载 ──────────────────────────────────────────────

/// 并行下载门槛：小于此值单连线足够（握手开销不划算）
const PARALLEL_MIN_SIZE: u64 = 6 << 20;
/// 并行连线数（限速 CDN 实测 4 连线 2.5-3.5×；再多易触风控）
const PARALLEL_SEGMENTS: usize = 4;

/// 把 total 切成 parts 段（纯函数，可测）：均分、余数归最后段、零长段丢弃。
fn plan_segments(total: u64, parts: usize) -> Vec<(u64, u64)> {
    let parts = parts.max(1);
    if total == 0 {
        return Vec::new();
    }
    let chunk = total / parts as u64;
    let mut out = Vec::with_capacity(parts);
    let mut start = 0u64;
    for i in 0..parts {
        let end = if i + 1 == parts { total } else { start + chunk };
        if start < end {
            out.push((start, end));
        }
        start = end;
    }
    out
}

/// 探测：发 bytes=0-0，服务器回 206 且带可解析总长 → Some(total)；否则 None。
fn probe_range(client: &HgClient, address: &str, referer: &str) -> Option<u64> {
    let response = client
        .agent_dl
        .get(address)
        .set("User-Agent", crate::hgapp::WEB_UA)
        .set("Referer", referer)
        .set("Accept-Encoding", "identity")
        .set("Range", "bytes=0-0")
        .call()
        .ok()?;
    if response.status() != 206 {
        return None;
    }
    let range = response.header("Content-Range")?;
    let declared = range.strip_prefix("bytes ")?.split('/').nth(1)?;
    if declared == "*" {
        return None;
    }
    declared.parse::<u64>().ok()
}

/// 单段下载线程体：Range 拉自己的区间，断线按段内偏移续传（重试 3 次）。
#[allow(clippy::too_many_arguments)] // 线程体入参天然多;重构成参数结构体可读性反降
fn fetch_segment(
    client: &HgClient,
    address: &str,
    referer: &str,
    part: &Path,
    start: u64,
    end: u64,
    downloaded: &std::sync::atomic::AtomicU64,
    abort: &std::sync::atomic::AtomicBool,
) -> Result<(), String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut pos = start;
    for _ in 0..3 {
        if abort.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("已中止".into());
        }
        let request = client
            .agent_dl
            .get(address)
            .set("User-Agent", crate::hgapp::WEB_UA)
            .set("Referer", referer)
            .set("Accept-Encoding", "identity")
            .set("Range", &format!("bytes={pos}-{}", end - 1));
        let response = match request.call() {
            Ok(r) if r.status() == 206 => r,
            Ok(r) => return Err(format!("分段 HTTP {}", r.status())),
            Err(e) => {
                eprintln!("[parallel] 分段断线重试: {e}");
                std::thread::sleep(std::time::Duration::from_millis(400));
                continue;
            }
        };
        let Ok(mut file) = std::fs::OpenOptions::new().write(true).open(part) else {
            return Err("写入文件失败".into());
        };
        use std::io::Write as _;
        if file.seek(SeekFrom::Start(pos)).is_err() {
            return Err("定位写入失败".into());
        }
        let mut reader = response.into_reader();
        let mut buffer = [0u8; 64 << 10];
        // 读循环:Ok(0) 且未达段末/读错误 = 断流 → 带段内偏移重连;
        // 完整到达 end → Ok
        loop {
            if abort.load(std::sync::atomic::Ordering::Relaxed) {
                return Err("已中止".into());
            }
            match reader.read(&mut buffer) {
                Ok(0) => {
                    if pos == end {
                        return Ok(());
                    }
                    break; // 提前断流 → 重连
                }
                Ok(n) => {
                    if file.write_all(&buffer[..n]).is_err() {
                        return Err("写入下载文件失败".into());
                    }
                    pos += n as u64;
                    downloaded.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                }
                Err(_) => break,
            }
        }
        if pos != end {
            // 走到这=断流重试;重试耗尽由外层 for 兜底
            std::thread::sleep(std::time::Duration::from_millis(400));
            continue;
        }
        // 完整到达段末
        return Ok(());
    }
    Err("分段重试耗尽".into())
}

/// 多段 Range 并行下载：探测→预分配→4 连线分段→组装。任何一段失败=整体失败
/// （调用方回退单连线）；成功时 rename 到 target。幂等：target 已存在且大小
/// 与探测一致 → 直接返回（冪等重投不重拉）。
fn download_direct_parallel(
    client: &HgClient,
    address: &str,
    referer: &str,
    target: &Path,
    on_progress: &mut dyn FnMut(f32),
) -> Result<u64, String> {
    if !is_http_media_url(address) {
        return Err("媒体下载地址无效".into());
    }
    if let Ok(info) = std::fs::metadata(target) {
        if let Some(total) = probe_range(client, address, referer) {
            if info.len() == total {
                on_progress(1.0);
                return Ok(total);
            }
        }
    }
    let total = probe_range(client, address, referer).ok_or("服务器不支持 Range")?;
    if total < PARALLEL_MIN_SIZE {
        return Err("文件太小，单连线即可".into());
    }
    let segments = plan_segments(total, PARALLEL_SEGMENTS);
    let part = PathBuf::from(format!("{}.mpart", target.display()));
    {
        let file = std::fs::File::create(&part).map_err(|e| format!("预分配失败：{e}"))?;
        file.set_len(total).map_err(|e| format!("预分配失败：{e}"))?;
    }
    let downloaded = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let abort = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let total_segments = segments.len();
    std::thread::scope(|scope| {
        for (start, end) in segments {
            let client = &*client;
            let downloaded = downloaded.clone();
            let abort = abort.clone();
            let done = done.clone();
            let part = part.clone();
            scope.spawn(move || {
                let r =
                    fetch_segment(client, address, referer, &part, start, end, &downloaded, &abort);
                done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if r.is_err() {
                    abort.store(true, std::sync::atomic::Ordering::Relaxed);
                }
                r
            });
        }
        // 主线程节流回报（100ms）；全部收工后补终值
        while done.load(std::sync::atomic::Ordering::Relaxed) < total_segments {
            std::thread::sleep(std::time::Duration::from_millis(100));
            let frac = downloaded.load(std::sync::atomic::Ordering::Relaxed) as f32 / total as f32;
            on_progress(frac.clamp(0.0, 0.999));
        }
    });
    let received = downloaded.load(std::sync::atomic::Ordering::Relaxed);
    if received != total {
        let _ = std::fs::remove_file(&part);
        return Err(format!("并行分段不完整（{received}/{total}），回退单连线"));
    }
    std::fs::rename(&part, target).map_err(|e| format!("组装失败：{e}"))?;
    on_progress(1.0);
    Ok(total)
}

/// Direct download with Range resume, mirroring the original's validator logic.
fn download_direct(
    client: &HgClient,
    address: &str,
    referer: &str,
    target: &Path,
    progress: &mut Progress,
    on_progress: &mut dyn FnMut(f32),
) -> Result<u64, String> {
    if !is_http_media_url(address) {
        return Err("媒体下载地址无效".into());
    }
    #[derive(serde::Deserialize, serde::Serialize, Default)]
    struct FileState {
        url: String,
        validator: String,
        total: i64,
        complete: bool,
    }
    let state_path = PathBuf::from(format!("{}.json", target.display()));
    let part_path = PathBuf::from(format!("{}.part", target.display()));
    let mut metadata: FileState =
        std::fs::read(&state_path).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default();
    if metadata.url != address {
        metadata = FileState::default();
    }
    if metadata.complete {
        if let Ok(info) = std::fs::metadata(target) {
            if info.len() > 0 && (metadata.total <= 0 || info.len() as i64 == metadata.total) {
                return Ok(info.len());
            }
        }
    }
    let mut offset: u64 = 0;
    if !metadata.validator.is_empty() {
        if let Ok(info) = std::fs::metadata(&part_path) {
            if info.len() > 0 && (metadata.total <= 0 || (info.len() as i64) <= metadata.total) {
                offset = info.len();
            }
        }
    }
    let mut validator = metadata.validator.clone();
    for attempt in 0..2 {
        let mut request = client
            .agent_dl
            .get(address)
            .set("User-Agent", crate::hgapp::WEB_UA)
            .set("Referer", referer)
            .set("Accept-Encoding", "identity");
        if offset > 0 && attempt == 0 {
            request = request
                .set("Range", &format!("bytes={}-", offset))
                .set("If-Range", &validator);
        }
        let response = match request.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(code, _)) => return Err(format!("下载 HTTP {}", code)),
            Err(e) => return Err(format!("下载连接中断：{}", e)),
        };
        let status = response.status();
        if attempt == 0 && offset > 0 && (status == 416 || status == 412) {
            offset = 0;
            validator.clear();
            continue;
        }
        if status != 200 && status != 206 {
            return Err(format!("下载 HTTP {}", status));
        }
        let content_type = response
            .header("Content-Type")
            .unwrap_or("")
            .to_lowercase();
        if content_type.contains("text/html") || content_type.contains("application/json") {
            return Err("站源返回了错误页面，请刷新下载地址后重试".into());
        }
        if let Some(encoding) = response.header("Content-Encoding") {
            if encoding != "identity" && !encoding.is_empty() {
                return Err("服务器返回了压缩媒体响应，无法安全续传".into());
            }
        }
        let total: i64 = response
            .header("Content-Length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(-1);
        let mut full_total = total;
        if status == 206 {
            let range = response.header("Content-Range").unwrap_or("");
            // bytes start-end/total
            if let Some(rest) = range.strip_prefix("bytes ") {
                let mut parts = rest.splitn(2, '/');
                let span = parts.next().unwrap_or("");
                let declared = parts.next().unwrap_or("*");
                let mut span_parts = span.split('-');
                let start: i64 = span_parts.next().unwrap_or("").trim().parse().unwrap_or(-1);
                if start as u64 != offset {
                    return Err("服务器返回的续传范围不正确".into());
                }
                if let Ok(value) = declared.parse::<i64>() {
                    full_total = value;
                }
            }
        }
        let flags_start = if offset > 0 && status == 206 { offset } else { 0 };
        let mut file = if flags_start > 0 {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&part_path)
                .map_err(|e| format!("写入文件失败：{}", e))?
        } else {
            offset = 0;
            std::fs::File::create(&part_path).map_err(|e| format!("写入文件失败：{}", e))?
        };
        if flags_start == 0 {
            if let Some(etag) = response.header("ETag") {
                if !etag.starts_with("W/") {
                    validator = etag.to_string();
                }
            } else if let Some(lm) = response.header("Last-Modified") {
                validator = lm.to_string();
            }
        }
        let mut reader = response.into_reader();
        let mut buffer = [0u8; 64 << 10];
        let mut received = offset;
        let mut failed: Option<String> = None;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if file.write_all(&buffer[..n]).is_err() {
                        failed = Some("写入下载文件失败，请检查剩余空间".into());
                        break;
                    }
                    received += n as u64;
                    progress.report(received, full_total.max(0) as u64);
                    if full_total > 0 {
                        on_progress(received as f32 / full_total as f32);
                    }
                }
                Err(e) => {
                    failed = Some(format!("媒体文件未接收完整（{}），重试将续传", e));
                    break;
                }
            }
        }
        if failed.is_none() && received == 0 {
            failed = Some("媒体文件未接收完整".into());
        }
        if failed.is_none() && full_total >= 0 && received != full_total as u64 {
            failed = Some("媒体文件未接收完整，重试将续传".into());
        }
        if let Some(err) = failed {
            let _ = file.sync_all();
            return Err(err);
        }
        file.sync_all().ok();
        drop(file);
        std::fs::rename(&part_path, target).map_err(|e| format!("落盘失败：{}", e))?;
        let _ = std::fs::write(
            &state_path,
            serde_json::to_vec(&FileState {
                url: address.to_string(),
                validator: validator.clone(),
                total: received as i64,
                complete: true,
            })
            .unwrap_or_default(),
        );
        return Ok(received);
    }
    Err("服务器无法恢复下载，请重试".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variant_picks_best_and_rewrites() {
        let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nv1/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1920x1080\nv2/index.m3u8\n"
            .lines()
            .map(|l| l.to_string())
            .collect::<Vec<_>>();
        let (kept, height) = select_variant(&master, 0).expect("master");
        assert_eq!(height, 1080);
        let uri = kept
            .iter()
            .find(|l| l.ends_with(".m3u8") && !l.starts_with('#'))
            .expect("variant uri kept");
        assert_eq!(uri, "v2/index.m3u8");
    }
}

#[cfg(test)]
mod parallel_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn plan_segments_covers_exactly() {
        // 均分:餘數歸最後段
        let segs = plan_segments(100, 4);
        assert_eq!(segs, vec![(0, 25), (25, 50), (50, 75), (75, 100)]);
        // 不整除:最後段吃餘數
        let segs = plan_segments(10, 4);
        assert_eq!(segs, vec![(0, 2), (2, 4), (4, 6), (6, 10)]);
        // 覆蓋連續性+零重疊(不變式)
        for total in [1u64, 7, 1000, 12345679] {
            let segs = plan_segments(total, 4);
            let mut cursor = 0u64;
            for (s, e) in &segs {
                assert_eq!(*s, cursor, "段必須首尾相接 total={total}");
                cursor = *e;
            }
            assert_eq!(cursor, total, "段並集必須覆蓋全部 total={total}");
        }
        // 零長/退化
        assert!(plan_segments(0, 4).is_empty());
        assert_eq!(plan_segments(5, 8), vec![(0, 5)]); // 段數多於位元組
    }

    /// 本地最小 Range 伺服器:127.0.0.1 隨機埠,支援 bytes=START-END/START-,
    /// 回 206+Content-Range;無 Range 回 200 全量。回 (埠, 連線計數)。
    fn spawn_range_server(body: std::sync::Arc<Vec<u8>>) -> (u16, std::sync::Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let body = body.clone();
                let hits = hits2.clone();
                std::thread::spawn(move || {
                    hits.fetch_add(1, Ordering::Relaxed);
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
                    let range = request
                        .lines()
                        .find_map(|l| l.strip_prefix("Range: bytes="))
                        .map(|s| s.trim().to_string());
                    let total = body.len();
                    let (status, extra, slice): (&str, String, &[u8]) = match range
                        .as_deref()
                        .map(|r| r.trim_start_matches("bytes=").to_string())
                    {
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
                                    format!(
                                        "Content-Range: bytes {}-{}/{}\r\n",
                                        s,
                                        e - 1,
                                        total
                                    ),
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
                    let _ = stream.flush();
                });
            }
        });
        (port, hits)
    }

    /// 並行下載 E2E(僅本地迴環,無外網):內容逐位元組正確+確實用了多連線。
    #[test]
    fn parallel_download_assembles_and_uses_multiple_connections() {
        // ~10MB(> 6MiB 門檻)偽隨機內容
        let body: Vec<u8> = (0..10_000_000u32).map(|i| (i % 251) as u8).collect();
        let (port, hits) = spawn_range_server(std::sync::Arc::new(body.clone()));
        let client = HgClient::new();
        let dir = std::env::temp_dir().join(format!("guo-par-dl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("media.mp4");
        let got = download_direct_parallel(
            &client,
            &format!("http://127.0.0.1:{port}/f.bin"),
            "",
            &target,
            &mut |_| {},
        )
        .expect("並行下載應成功");
        assert_eq!(got as usize, body.len());
        assert_eq!(std::fs::read(&target).unwrap(), body, "組裝內容必須逐位元組一致");
        // probe(1) + 4 段(4)= 至少 5 連線;不足=沒走並行
        assert!(hits.load(Ordering::Relaxed) >= 5, "應使用多連線: {}", hits.load(Ordering::Relaxed));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 伺服器不支援 Range(一律回 200)→ 探測拒絕,由呼叫方回退單連線。
    #[test]
    fn parallel_download_rejects_when_no_range_support() {
        // 全 0 內容 1MB;此伺服器忽略 Range(回 200)
        let body = vec![0u8; 1 << 20];
        let (port, _hits) = spawn_range_server_force200(std::sync::Arc::new(body));
        let client = HgClient::new();
        let dir = std::env::temp_dir().join(format!("guo-par-norange-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("media.mp4");
        let r = download_direct_parallel(
            &client,
            &format!("http://127.0.0.1:{port}/f.bin"),
            "",
            &target,
            &mut |_| {},
        );
        assert!(r.is_err(), "無 Range 支援必須拒絕(回退單連線)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn spawn_range_server_force200(body: std::sync::Arc<Vec<u8>>) -> (u16, std::sync::Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(AtomicUsize::new(0));
        let hits2 = hits.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let body = body.clone();
                let hits = hits2.clone();
                std::thread::spawn(move || {
                    hits.fetch_add(1, Ordering::Relaxed);
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
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                });
            }
        });
        (port, hits)
    }
}
