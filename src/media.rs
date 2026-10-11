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
    let mut errors = Vec::new();
    match client.media(store, video_id) {
        Ok(media) => return Ok(media),
        Err(err) => errors.push(format!("App 取流：{}", err)),
    }
    match client.web_media(series_id, video_id) {
        Ok(media) => return Ok(media),
        Err(err) => errors.push(format!("网页取流：{}", err)),
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
                while let Some(start) = working.find("URI=\"") {
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
                            name = format!("{}.key", &hex::encode(
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
                    working = replaced;
                    break; // one URI per line in practice
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

fn attribute<'a>(line: &'a str, key: &str) -> Option<String> {
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
        for next in index + 1..lines.len() {
            let text = lines[next].trim();
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
        let size =
            download_direct(client, &media.url, &media.referer, &target, &mut progress, on_progress)?;
        progress.done();
        if let Some(key) = media.cenc_key {
            crate::cenc::decrypt_file(&target, &key)?;
        }
        let _ = size;
        Ok(("media.mp4".into(), "mp4".into(), media.quality))
    }
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
