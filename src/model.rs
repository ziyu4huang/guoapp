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

/// 观看数字段转数值（排序用）：非数字/空串 = 0。
pub fn count_value(raw: &str) -> f64 {
    raw.trim().parse::<f64>().unwrap_or(0.0)
}

/// 观看数/热度展示格式：963 → "963"，946535 → "94.7万"，40329652 → "4032.97万"，
/// 1.2e9 → "12亿"。非数字/空串原样返回（可能是 API 给的带单位文本）。
pub fn format_count(raw: &str) -> String {
    let Ok(v) = raw.trim().parse::<f64>() else {
        return raw.trim().to_string();
    };
    if !v.is_finite() || v < 0.0 {
        return String::new();
    }
    // 先修数再接单位（"4033.0" → "4033" + "万"）；1 位小数的 .0 去尾
    if v >= 1e8 {
        format!("{}亿", format!("{:.2}", v / 1e8).trim_end_matches('0').trim_end_matches('.'))
    } else if v >= 1e4 {
        format!("{}万", format!("{:.1}", v / 1e4).trim_end_matches('0').trim_end_matches('.'))
    } else {
        format!("{}", v as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::format_count;

    #[test]
    fn format_count_units() {
        assert_eq!(format_count("963"), "963");
        assert_eq!(format_count("946535"), "94.7万");
        assert_eq!(format_count("40329652"), "4033万");
        assert_eq!(format_count("123456789"), "1.23亿");
        assert_eq!(format_count("1200000000"), "12亿");
        assert_eq!(format_count(""), "");
        assert_eq!(format_count("3.2亿"), "3.2亿"); // 非数字文本原样
        assert_eq!(format_count("abc"), "abc");
    }

    #[test]
    fn count_value_parses_and_defaults_zero() {
        assert_eq!(super::count_value("946535"), 946535.0);
        assert_eq!(super::count_value(" 86837 "), 86837.0);
        assert_eq!(super::count_value(""), 0.0);
        assert_eq!(super::count_value("3.2亿"), 0.0);
    }
}
