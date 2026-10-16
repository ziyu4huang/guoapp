//! Backup playback API (`djapi.999888456.xyz`) — v2 encrypted response
//! decryption plus the spade_a content-key decoder. Ports
//! provider_hongguo_playback.go.

use crate::hgapp::HgClient;
use crate::hgweb::urlencode;
use crate::model::{is_http_media_url, Media, Variant};
use base64::Engine;
use cbc::cipher::KeyIvInit;
use serde_json::Value;
use std::time::Duration;

const PLAYBACK_API: &str = "https://djapi.999888456.xyz/api/hongguo/play";

pub fn decode_b64(value: &str) -> Result<Vec<u8>, String> {
    let value = value.trim();
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(value))
        .map_err(|_| "base64 解码失败".into())
}

fn decode_b64_lenient(value: &str) -> Result<Vec<u8>, String> {
    let value = value.trim();
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(value.trim_end_matches('=')))
        .map_err(|_| "base64 解码失败".into())
}

pub(crate) fn aes_cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    use cbc::cipher::BlockDecryptMut;
    type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
    let mut decryptor = Aes128CbcDec::new_from_slices(key, iv).map_err(|e| e.to_string())?;
    let mut plain = ciphertext.to_vec();
    for block in plain.chunks_mut(16) {
        decryptor.decrypt_block_mut(block.into());
    }
    pkcs7_unpad(&plain).ok_or_else(|| "PKCS7 填充无效".to_string())
}

fn pkcs7_unpad(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() || !data.len().is_multiple_of(16) {
        return None;
    }
    let padding = *data.last()? as usize;
    if padding == 0 || padding > 16 || padding > data.len() {
        return None;
    }
    if data[data.len() - padding..].iter().any(|&b| b as usize != padding) {
        return None;
    }
    Some(data[..data.len() - padding].to_vec())
}

/// Decode `v2.<hex>.<base64>` responses: the hex section carries a masked key
/// material, the base64 section is AES-128-CBC ciphertext.
pub fn decode_playback_response(body: &str) -> Result<Vec<u8>, String> {
    let text = body.trim();
    if !text.starts_with("v2.") {
        return Ok(text.as_bytes().to_vec());
    }
    let parts: Vec<&str> = text.splitn(3, '.').collect();
    if parts.len() != 3 || parts[1].len() <= 4 || parts[1].len() > 1028 {
        return Err("红果备用接口响应密钥无效".into());
    }
    let encoded = hex::decode(&parts[1][4..]).map_err(|_| "红果备用接口响应密钥无效".to_string())?;
    if encoded.len() < 32 {
        return Err("红果备用接口响应密钥无效".into());
    }
    const MASK: [u8; 20] = [
        104, 64, 70, 166, 190, 168, 143, 130, 225, 254, 251, 217, 196, 34, 45, 60, 29, 20, 103,
        105,
    ];
    let mut material = vec![0u8; encoded.len()];
    for (index, &current) in encoded.iter().enumerate() {
        let previous = if index == 0 { 109u8 } else { encoded[index - 1] };
        let slot = index % MASK.len();
        let salt = MASK[slot] ^ ((90 + 13 * slot) as u8) ^ 85;
        let shifted = (current as u32).wrapping_add(215).wrapping_sub(11 * index as u32) as u8;
        material[index] = previous ^ salt ^ shifted.rotate_left(3);
    }
    let ciphertext = decode_b64_lenient(parts[2])?;
    if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
        return Err("红果备用接口加密响应无效".into());
    }
    aes_cbc_decrypt(&material[..16], &material[16..32], &ciphertext)
}

/// Decode the spade_a blob into the 16-byte AES content key.
pub fn content_key(value: &str) -> Result<[u8; 16], String> {
    if value.len() > 1024 {
        return Err("红果媒体密钥数据过长".into());
    }
    let raw = decode_b64(value)?;
    if raw.len() < 3 {
        return Err("红果媒体密钥编码无效".into());
    }
    let len = raw.len();
    let tag_length = (raw[0] ^ raw[1] ^ raw[2]).wrapping_sub(48) as usize;
    let content_length = len - tag_length - 1;
    if tag_length < 1 || content_length < 33 || content_length >= len {
        return Err("红果媒体密钥结构无效".into());
    }
    let seed = raw[len - tag_length - 2] ^ raw[len - tag_length - 1];
    let tag: Vec<u8> = raw[len - tag_length..].iter().map(|b| b ^ seed).collect();
    if tag == b"app_v2" || tag == b"web_v2" {
        return Err("红果媒体密钥版本暂不支持".into());
    }
    let mut decoded = vec![0u8; content_length];
    let mut previous_even: u8 = 250;
    let mut previous_odd: u8 = 85;
    for (index, &current) in raw[1..1 + content_length].iter().enumerate() {
        let previous = if index % 2 == 0 {
            let prev = previous_even;
            previous_even = current;
            prev
        } else {
            let prev = previous_odd;
            previous_odd = current;
            prev
        };
        decoded[index] = (previous ^ current).wrapping_sub(21).wrapping_sub(index.count_ones() as u8);
    }
    // First decoded char is the padding count in base-36 (Go: ParseUint(s, 36, 8)).
    let padding_char = *decoded.first().ok_or("红果媒体密钥内容无效")?;
    let padding = (padding_char as char).to_digit(36).ok_or("红果媒体密钥内容无效")? as usize;
    if content_length.checked_sub(padding).and_then(|v| v.checked_sub(1)) != Some(32) {
        return Err("红果媒体密钥内容无效".into());
    }
    let key_text = std::str::from_utf8(&decoded[1..33]).map_err(|_| "红果媒体密钥内容无效".to_string())?;
    let key = hex::decode(key_text.trim()).map_err(|_| "红果媒体密钥不是有效的 AES-128 密钥".to_string())?;
    key.try_into().map_err(|_| "红果媒体密钥不是有效的 AES-128 密钥".to_string())
}

impl HgClient {
    /// Third fallback: the public playback relay. Returns direct media plus
    /// per-quality variants and their CENC keys.
    pub fn backup_media(&self, series_id: &str, video_id: &str) -> Result<Media, String> {
        use crate::model::is_numeric_id;
        if !is_numeric_id(series_id) || !is_numeric_id(video_id) {
            return Err("红果播放请求缺少有效的剧集 ID".into());
        }
        let reference = serde_json::json!({
            "content_type": 1004,
            "from_video_id": "",
            "series_id": series_id,
            "vid": video_id,
            "video_platform": 3
        });
        let encoded = base64::engine::general_purpose::STANDARD.encode(reference.to_string());
        let body = self.fetch_text(
            &format!("{}?id={}", PLAYBACK_API, urlencode(&encoded)),
            "https://hongguoduanju.com/",
        )?;
        let decoded = decode_playback_response(&body)?;
        let response: Value =
            serde_json::from_slice(&decoded).map_err(|_| "红果备用播放接口返回了无效数据".to_string())?;
        for flag in ["parse", "jx"] {
            match response.get(flag) {
                None | Some(Value::Null) => {}
                Some(Value::Bool(false)) => {}
                Some(Value::Number(n)) if n.as_i64() == Some(0) => {}
                Some(Value::String(s)) if s == "0" || s.is_empty() => {}
                _ => return Err("红果备用播放接口没有返回直接媒体地址".into()),
            }
        }
        let mut selected: Option<Media> = None;
        let mut best_quality = -1;
        let mut key_err = String::new();
        let mut variants: Vec<Variant> = Vec::new();
        for option in response
            .get("key_urls")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let media_url = crate::jsonx::jstr(&option, &["src"]).trim().to_string();
            if media_url.len() > 8192 || !is_http_media_url(&media_url) {
                continue;
            }
            let key_id = hex::decode(crate::jsonx::jstr(&option, &["kid"]).trim())
                .map_err(|_| "红果媒体密钥标识无效".to_string())?;
            if key_id.len() != 16 {
                key_err = "红果媒体密钥标识无效".into();
                continue;
            }
            let key = match content_key(&crate::jsonx::jstr(&option, &["spade_a"])) {
                Ok(key) => key,
                Err(err) => {
                    key_err = err;
                    continue;
                }
            };
            let quality = crate::hgapp::parse_quality_pub(&crate::jsonx::jstr(&option, &["name"]));
            let variant = Variant {
                url: media_url.clone(),
                referer: "https://novel.snssdk.com/".into(),
                quality,
                cenc_key: Some(key),
            };
            variants.push(variant.clone());
            if selected.is_none() || quality > best_quality {
                best_quality = quality;
                selected = Some(Media {
                    url: media_url,
                    referer: "https://novel.snssdk.com/".into(),
                    quality,
                    cenc_key: Some(key),
                    variants: Vec::new(),
                });
            }
        }
        if let Some(mut media) = selected {
            media.variants = variants;
            return Ok(media);
        }
        if !key_err.is_empty() {
            return Err(key_err);
        }
        Err("红果备用播放接口未返回该集可用的媒体和密钥".into())
    }

    pub fn fetch_text(&self, url: &str, referer: &str) -> Result<String, String> {
        let response = self
            .agent
            .get(url)
            .timeout(Duration::from_secs(20))
            .set("User-Agent", crate::hgapp::WEB_UA)
            .set("Referer", referer)
            .call();
        match response {
            Ok(r) => r.into_string().map_err(|_| "响应读取失败".to_string()),
            Err(ureq::Error::Status(code, _)) => Err(format!("HTTP {}", code)),
            Err(e) => Err(format!("请求失败：{}", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkcs7_round_trip() {
        assert!(pkcs7_unpad(&[0u8; 16]).is_none());
        let mut block = [15u8; 16];
        block[15] = 1;
        assert_eq!(pkcs7_unpad(&block), Some(vec![15u8; 15]));
    }

    #[test]
    fn plain_body_passes_through() {
        assert_eq!(decode_playback_response("{\"ok\":1}").unwrap(), b"{\"ok\":1}");
    }

    #[test]
    fn v2_body_requires_three_parts() {
        assert!(decode_playback_response("v2.abc").is_err());
    }
}
