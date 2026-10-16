//! 浏览器登录 cookie 自动提取（INC-20261015 上游锁匿名取流后的自动化解锁路径）。
//! 移植自主 repo `safari-cookies` skill 已实证的 chrome 主路：直读 Chrome 家族
//! cookie 数据库（SQLite）+ v10 AES-128-CBC 解密 + 2026-07+ 明文 32B host 绑定
//! 前缀处理；Safari `Cookies.binarycookies` 二进制解析兜底。
//!
//! 安全铁则：cookie 值**永不**全文打印/写日志（界面与 CLI 一律走 [`mask`]）；
//! 提取结果只落本机数据目录 state.json，不上传。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

/// 红果网页登录域（与 hgweb::WEB_BASE 同源）。
pub const WEB_LOGIN_DOMAIN: &str = "hongguoduanju.com";

/// 提取结果：拼好的 Cookie 请求头 + 来源描述（供收据显示，不含值）。
pub struct Extraction {
    pub header: String,
    pub source: String,
    pub names: Vec<String>,
}

/// 值脱敏显示：前 4 字符 + 长度（skill 铁则：值永不全文印出）。
pub fn mask(value: &str) -> String {
    let head: String = value.chars().take(4).collect();
    format!("{}****(len {})", head, value.chars().count())
}

/// 级联提取：Chrome 家族（Chrome→Edge→Brave→Arc→Chromium，全部 profile）
/// → Safari binarycookies。任一路拿到目标域 cookie 即返回。
#[cfg(target_os = "macos")]
pub fn extract_login_cookie(domain: &str) -> Result<Extraction, String> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    let mut errors: Vec<String> = Vec::new();

    for browser in chrome_browsers() {
        let profiles = chrome_profiles(&home.join(browser.dir));
        if profiles.is_empty() {
            continue;
        }
        // 钥匙圈是弹对话框的一次性授权动作——确认该 profile 真有目标域
        // cookie 后才去取；没有则整浏览器零打扰跳过。
        let mut cached_pass: Option<String> = None;
        for profile in profiles {
            let db = profile.join("Cookies");
            let rows = match chrome_query_rows(&db, domain) {
                Ok(rows) => rows,
                Err(err) => {
                    errors.push(format!("{}({}):{}", browser.label, profile_label(&profile), err));
                    continue;
                }
            };
            if rows.is_empty() {
                continue;
            }
            if cached_pass.is_none() {
                cached_pass = Some(keychain_password(browser.service)?);
            }
            let pass = cached_pass.as_deref().unwrap_or_default();
            match rows_to_header(&rows, pass) {
                Ok((header, names)) => {
                    let label = profile_label(&profile);
                    return Ok(Extraction {
                        header,
                        source: format!("{}·{}", browser.label, label),
                        names,
                    });
                }
                Err(err) => errors.push(format!("{}({}):{}", browser.label, profile_label(&profile), err)),
            }
        }
    }
    match safari_cookie_header(&home, domain) {
        Ok((header, names)) => {
            return Ok(Extraction {
                header,
                source: "Safari".into(),
                names,
            });
        }
        Err(err) => errors.push(format!("Safari:{}", err)),
    }

    Err(format!(
        "未能从本机浏览器取得 {} 的登录 cookie——{}。\
         请先在浏览器登录该站点后重试；或改用手动粘贴（GUI 🔑 对话框）",
        domain,
        if errors.is_empty() {
            "各浏览器都没有此域的 cookie".to_string()
        } else {
            errors.join("；")
        }
    ))
}

/// 非 macOS：自动提取未支持（Chrome 系 Windows 解密走 DPAPI、Linux 走
/// gnome-keyring，均为另案），保持手动粘贴路径。
#[cfg(not(target_os = "macos"))]
pub fn extract_login_cookie(_domain: &str) -> Result<Extraction, String> {
    Err("自动提取目前仅支持 macOS（Chrome 家族+Safari）；请在浏览器登录后于 GUI 🔑 对话框手动粘贴整行 Cookie".into())
}

// ---------------------------------------------------------------------------
// Chrome 家族：SQLite 直读 + v10 解密
// ---------------------------------------------------------------------------

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct ChromeBrowser {
    label: &'static str,
    /// 相对 $HOME 的 user-data 目录。
    dir: &'static str,
    /// 钥匙圈 generic-password 服务名。
    service: &'static str,
}

#[cfg(target_os = "macos")]
fn chrome_browsers() -> Vec<ChromeBrowser> {
    vec![
        ChromeBrowser { label: "Chrome", dir: "Library/Application Support/Google/Chrome", service: "Chrome Safe Storage" },
        ChromeBrowser { label: "Edge", dir: "Library/Application Support/Microsoft Edge", service: "Microsoft Edge Safe Storage" },
        ChromeBrowser { label: "Brave", dir: "Library/Application Support/BraveSoftware/Brave-Browser", service: "Brave Safe Storage" },
        ChromeBrowser { label: "Arc", dir: "Library/Application Support/Arc/User Data", service: "Arc Safe Storage" },
        ChromeBrowser { label: "Chromium", dir: "Library/Application Support/Chromium", service: "Chromium Safe Storage" },
    ]
}

/// 该 user-data 目录下所有带 cookie 库的 profile（Default 优先，其余按名排序）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn chrome_profiles(user_data: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(user_data) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("Cookies").is_file())
        .collect();
    dirs.sort_by_key(|p| {
        let is_default = p.file_name().is_some_and(|n| n == "Default");
        let is_guestish = p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.contains("Guest") || n.contains("System"));
        (usize::from(!is_default), usize::from(is_guestish))
    });
    dirs
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn profile_label(profile: &Path) -> String {
    profile
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("profile")
        .to_string()
}

/// Chrome 运行中会锁库——复制（含 WAL/journal）到系统临时目录后只读查询，
/// 查完即删。返回 (name, 明文 value 列, encrypted_value blob)。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn chrome_query_rows(
    db: &Path,
    domain: &str,
) -> Result<Vec<(String, String, Vec<u8>)>, String> {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!(
        "guo-cookies-{}-{}.db",
        std::process::id(),
        seq
    ));
    let tmp_wal = PathBuf::from(format!("{}-wal", tmp.display()));
    let tmp_shm = PathBuf::from(format!("{}-shm", tmp.display()));
    std::fs::copy(db, &tmp).map_err(|e| format!("复制 cookie 库失败:{}", e))?;
    for side in [format!("{}-wal", db.display()), format!("{}-shm", db.display())] {
        let side = PathBuf::from(side);
        if side.is_file() {
            let _ = std::fs::copy(&side, if side.extension().is_some_and(|e| e == "wal") {
                &tmp_wal
            } else {
                &tmp_shm
            });
        }
    }
    let result = (|| {
        let conn = rusqlite::Connection::open_with_flags(
            &tmp,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|e| format!("打开 cookie 库失败:{}", e))?;
        let mut stmt = conn
            .prepare("SELECT name, value, encrypted_value FROM cookies WHERE host_key LIKE ?1")
            .map_err(|e| format!("cookie 库结构不可读:{}", e))?;
        let rows = stmt
            .query_map([format!("%{}%", domain)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            })
            .map_err(|e| format!("查询 cookie 失败:{}", e))?
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>();
        Ok(rows)
    })();
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file(&tmp_wal);
    let _ = std::fs::remove_file(&tmp_shm);
    result
}

/// 同名 cookie 去重（多 host 命中同名是常态，如 .google.com 与 accounts.google.com
/// 的 SID）——与 skill 原版 dict 赋值语义一致：后者覆盖前者。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn dedup_pairs(pairs: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out: Vec<(String, String)> = Vec::new();
    for (name, value) in pairs {
        match seen.get(&name) {
            Some(&index) => out[index].1 = value,
            None => {
                seen.insert(name.clone(), out.len());
                out.push((name, value));
            }
        }
    }
    out
}

/// 明文/可解密行 → Cookie 请求头。部分解密失败仍可用（skill 同款语义）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn rows_to_header(
    rows: &[(String, String, Vec<u8>)],
    key_pass: &str,
) -> Result<(String, Vec<String>), String> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut failures = 0usize;
    for (name, plain, encrypted) in rows {
        let value = if !plain.is_empty() {
            plain.clone()
        } else {
            match decrypt_chrome_v10(encrypted, key_pass) {
                Ok(v) => v,
                Err(_) => {
                    failures += 1;
                    continue;
                }
            }
        };
        if !value.is_empty() {
            pairs.push((name.clone(), value));
        }
    }
    if pairs.is_empty() {
        return Err(if failures > 0 {
            "cookie 解密失败（可能为 v20 app-bound 新加密格式）".into()
        } else {
            "cookie 值全为空".into()
        });
    }
    let pairs = dedup_pairs(pairs);
    let names = pairs.iter().map(|(n, _)| n.clone()).collect();
    let header = pairs
        .iter()
        .map(|(n, v)| format!("{}={}", n, v))
        .collect::<Vec<_>>()
        .join("; ");
    Ok((header, names))
}

/// PBKDF2-HMAC-SHA1(密码, "saltysalt", 1003 轮, 16B)——Chrome v10 派生密钥。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn derive_v10_key(password: &str) -> [u8; 16] {
    let mut key = [0u8; 16];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password.as_bytes(), b"saltysalt", 1003, &mut key);
    key
}

/// v10 明文含 0/16/32B host 绑定前缀（2026-07+）——取最长的全可打印后缀。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn printable_suffix(dec: &[u8]) -> Option<String> {
    for skip in [0usize, 16, 32] {
        if skip > dec.len() {
            continue;
        }
        let cand = &dec[skip..];
        if !cand.is_empty() && cand.iter().all(|&b| (0x20..0x7f).contains(&b)) {
            return Some(String::from_utf8_lossy(cand).into_owned());
        }
    }
    None
}

/// 解密单颗 `v10` cookie：AES-128-CBC（IV=16×0x20）+ PKCS7 + 前缀剥离。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn decrypt_chrome_v10(blob: &[u8], password: &str) -> Result<String, String> {
    if blob.len() < 3 || &blob[..3] != b"v10" {
        return Err(format!(
            "非 v10 加密（前缀 {:?}）",
            String::from_utf8_lossy(&blob[..blob.len().min(3)])
        ));
    }
    let ciphertext = &blob[3..];
    if ciphertext.len() < 16 || !ciphertext.len().is_multiple_of(16) {
        return Err("密文长度不是 16 字节倍数（损坏或非 v10）".into());
    }
    let key = derive_v10_key(password);
    let dec = crate::backup::aes_cbc_decrypt(&key, &[0x20; 16], ciphertext)
        .map_err(|_| "AES 解密失败".to_string())?;
    printable_suffix(&dec)
        .ok_or_else(|| "解密结果非可打印文本（0/16/32 前缀皆不符；可能为 v20 app-bound 新加密）".into())
}

#[cfg(target_os = "macos")]
fn keychain_password(service: &str) -> Result<String, String> {
    let out = std::process::Command::new("security")
        .args(["find-generic-password", "-w", "-s", service])
        .output()
        .map_err(|e| format!("无法执行 security：{}", e))?;
    if !out.status.success() {
        return Err(format!(
            "钥匙圈读取「{}」被拒——首次访问会弹授权对话框，请点「允许」后重试（{})",
            service,
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

// ---------------------------------------------------------------------------
// Safari 兜底：Cookies.binarycookies 二进制解析
//   版面 = skill 对真实 macOS 26 数据库逐字节验证的格式（混合字节序：
//   文件头/页 magic 大端；记录小端 48B 头 + NUL 结尾字符串）
// ---------------------------------------------------------------------------

pub struct RawCookie {
    pub url: String,
    pub name: String,
    pub value: String,
}

pub fn parse_binary_cookies(buf: &[u8]) -> Result<Vec<RawCookie>, String> {
    if buf.len() < 8 || &buf[..4] != b"cook" {
        return Err("非 binarycookies 格式（魔法码不符）".into());
    }
    let num_pages = u32::from_be_bytes(buf[4..8].try_into().unwrap()) as usize;
    if num_pages == 0 || num_pages > 65536 {
        return Err(format!("页数不合理:{}", num_pages));
    }
    let be32 = |off: usize| -> Result<u32, String> {
        buf.get(off..off + 4)
            .map(|s| u32::from_be_bytes(s.try_into().unwrap()))
            .ok_or_else(|| format!("越界读取 off={}", off))
    };
    let le32 = |off: usize| -> Result<u32, String> {
        buf.get(off..off + 4)
            .map(|s| u32::from_le_bytes(s.try_into().unwrap()))
            .ok_or_else(|| format!("越界读取 off={}", off))
    };
    let mut out = Vec::new();
    let mut page_start = 8 + 4 * num_pages;
    for p in 0..num_pages {
        let page_size = be32(8 + p * 4)? as usize;
        if page_start + 8 > buf.len() {
            return Err(format!("page {} 越界(start={}, len={})", p, page_start, buf.len()));
        }
        if be32(page_start)? != 0x100 {
            return Err(format!("page {} 页头魔法码不符（版面/编码假设错误）", p));
        }
        let num_cookies = le32(page_start + 4)? as usize;
        if num_cookies > 100_000 {
            return Err(format!("page {} 记录数不合理:{}", p, num_cookies));
        }
        for c in 0..num_cookies {
            let rec_off = page_start + le32(page_start + 8 + c * 4)? as usize;
            if rec_off + 48 > buf.len() {
                return Err(format!("page {} 记录 {} 越界", p, c));
            }
            let rec_end = (rec_off + le32(rec_off)? as usize).min(buf.len());
            let cstr = |off: usize| -> Result<String, String> {
                let start = rec_off + off;
                let mut end = start;
                while end < rec_end && buf.get(end).copied().unwrap_or(0) != 0 {
                    end += 1;
                }
                if end >= rec_end {
                    return Err(format!("page {} 记录 {} 字符串无 NUL 结尾", p, c));
                }
                Ok(String::from_utf8_lossy(&buf[start..end]).into_owned())
            };
            out.push(RawCookie {
                url: cstr(le32(rec_off + 16)? as usize)?,
                name: cstr(le32(rec_off + 20)? as usize)?,
                value: cstr(le32(rec_off + 28)? as usize)?,
            });
        }
        page_start += page_size;
    }
    Ok(out)
}

/// Safari cookie 库候选路径（新版 macOS 在 sandbox container 内，需完全磁盘访问）。
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn safari_paths(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join("Library/Containers/com.apple.Safari/Data/Library/Cookies/Cookies.binarycookies"),
        home.join("Library/Cookies/Cookies.binarycookies"),
    ]
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn safari_cookie_header(home: &Path, domain: &str) -> Result<(String, Vec<String>), String> {
    let mut read_err: Option<String> = None;
    let mut parse_err: Option<String> = None;
    let mut tried = 0usize;
    for path in safari_paths(home) {
        if !path.is_file() {
            continue;
        }
        tried += 1;
        let buf = match std::fs::read(&path) {
            Ok(buf) => buf,
            Err(e) => {
                read_err = Some(format!("{}:{}", path.display(), e));
                continue;
            }
        };
        match parse_binary_cookies(&buf) {
            Ok(cookies) => {
                let pairs: Vec<(String, String)> = dedup_pairs(
                    cookies
                        .iter()
                        .filter(|c| c.url.contains(domain) && !c.value.is_empty())
                        .map(|c| (c.name.clone(), c.value.clone()))
                        .collect(),
                );
                if !pairs.is_empty() {
                    let names = pairs.iter().map(|(n, _)| n.clone()).collect();
                    let header = pairs
                        .iter()
                        .map(|(n, v)| format!("{}={}", n, v))
                        .collect::<Vec<_>>()
                        .join("; ");
                    return Ok((header, names));
                }
            }
            Err(e) => parse_err = Some(format!("{}:{}", path.display(), e)),
        }
    }
    if tried == 0 {
        return Err("找不到 Safari cookie 库（本机从未用 Safari 打开过网页）".into());
    }
    if let Some(err) = parse_err {
        return Err(format!("binarycookies 解析失败（{}）", err));
    }
    Err(format!(
        "读取被挡（{}）——需给本 App 完全磁盘访问：系统设置→隐私与安全性→完全磁盘访问权限",
        read_err.unwrap_or_default()
    ))
}

// ---------------------------------------------------------------------------
// 测试（全部 hermetic：合成加密 blob + 合成 binarycookies fixture + 内存 SQLite）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit};

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    fn v10_encrypt(plaintext: &[u8], password: &str) -> Vec<u8> {
        let key = derive_v10_key(password);
        let iv = [0x20u8; 16];
        let padded_len = (plaintext.len() / 16 + 1) * 16;
        let mut buf = vec![0u8; padded_len];
        buf[..plaintext.len()].copy_from_slice(plaintext);
        let enc = Aes128CbcEnc::new_from_slices(&key, &iv).unwrap();
        let out = enc
            .encrypt_padded_mut::<cbc::cipher::block_padding::Pkcs7>(
                &mut buf,
                plaintext.len(),
            )
            .expect("PKCS7 加密失败");
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(out);
        blob
    }

    #[test]
    fn v10_roundtrip() {
        let blob = v10_encrypt(b"sid=abcdef0123456789", "keypass");
        assert_eq!(
            decrypt_chrome_v10(&blob, "keypass").unwrap(),
            "sid=abcdef0123456789"
        );
    }

    #[test]
    fn v10_strips_32byte_host_prefix() {
        let mut plain = vec![0x01u8; 32]; // 2026-07+ host 绑定前缀（非可打印）
        plain.extend_from_slice(b"sessionid=zzz");
        let blob = v10_encrypt(&plain, "kp");
        assert_eq!(decrypt_chrome_v10(&blob, "kp").unwrap(), "sessionid=zzz");
    }

    #[test]
    fn v10_rejects_wrong_password_and_non_v10() {
        let blob = v10_encrypt(b"sid=abc", "right");
        assert!(decrypt_chrome_v10(&blob, "wrong").is_err());
        assert!(decrypt_chrome_v10(b"v20junkjunkjunkjunk", "right").is_err());
        assert!(decrypt_chrome_v10(b"ab", "right").is_err());
    }

    #[test]
    fn printable_suffix_picks_longest() {
        let mut dec = vec![0x02u8; 16];
        dec.extend_from_slice(b"abc");
        assert_eq!(printable_suffix(&dec).unwrap(), "abc");
        assert_eq!(printable_suffix(b"all-printable").unwrap(), "all-printable");
        assert!(printable_suffix(&[0x01, 0x02]).is_none());
    }

    #[test]
    fn rows_to_header_merges_and_skips_failures() {
        let ok_blob = v10_encrypt(b"v1=ok", "pw");
        let rows = vec![
            ("a".to_string(), "plain-value".to_string(), Vec::new()),
            ("b".to_string(), String::new(), ok_blob),
            ("bad".to_string(), String::new(), b"v10garbage!!".to_vec()),
            ("empty".to_string(), String::new(), Vec::new()),
        ];
        let (header, names) = rows_to_header(&rows, "pw").unwrap();
        assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
        assert!(header.starts_with("a=plain-value; b=v1=ok") || header.contains("a=plain-value"));
        assert!(header.contains("b=v1=ok"));
        // 全失败 → 报错（不假装成功）
        let bad = vec![("x".to_string(), String::new(), b"v10xx".to_vec())];
        assert!(rows_to_header(&bad, "pw").is_err());
    }

    #[test]
    fn host_filter_only_matches_domain() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE cookies (name TEXT, value TEXT, encrypted_value BLOB, host_key TEXT);",
        )
        .unwrap();
        for (host, name) in [
            (".hongguoduanju.com", "target1"),
            ("www.hongguoduanju.com", "target2"),
            (".example.com", "other"),
        ] {
            conn.execute(
                "INSERT INTO cookies VALUES (?1, '', X'763130616161', ?2)",
                rusqlite::params![name, host],
            )
            .unwrap();
        }
        let mut stmt = conn
            .prepare("SELECT name FROM cookies WHERE host_key LIKE ?1")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([format!("%{}%", WEB_LOGIN_DOMAIN)], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(names.len(), 2, "只命中红果域: {:?}", names);
    }

    // -- binarycookies fixture 构造（移植 skill fixture.ts，版面=真实验证版）--

    struct FxCookie {
        url: &'static str,
        name: &'static str,
        value: &'static str,
        path: &'static str,
    }

    fn enc_str(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }

    fn build_record(c: &FxCookie) -> Vec<u8> {
        let url_b = enc_str(c.url);
        let name_b = enc_str(c.name);
        let path_b = enc_str(c.path);
        let value_b = enc_str(c.value);
        let size = 48 + url_b.len() + name_b.len() + path_b.len() + value_b.len();
        let mut rec = vec![0u8; size];
        rec[..4].copy_from_slice(&(size as u32).to_le_bytes());
        let (url_off, name_off) = (48usize, 48 + url_b.len());
        let path_off = name_off + name_b.len();
        let value_off = path_off + path_b.len();
        rec[16..20].copy_from_slice(&(url_off as u32).to_le_bytes());
        rec[20..24].copy_from_slice(&(name_off as u32).to_le_bytes());
        rec[24..28].copy_from_slice(&(path_off as u32).to_le_bytes());
        rec[28..32].copy_from_slice(&(value_off as u32).to_le_bytes());
        rec[url_off..url_off + url_b.len()].copy_from_slice(&url_b);
        rec[name_off..name_off + name_b.len()].copy_from_slice(&name_b);
        rec[path_off..path_off + path_b.len()].copy_from_slice(&path_b);
        rec[value_off..value_off + value_b.len()].copy_from_slice(&value_b);
        rec
    }

    fn build_page(cookies: &[FxCookie]) -> Vec<u8> {
        let records: Vec<Vec<u8>> = cookies.iter().map(build_record).collect();
        let header_size = 8 + records.len() * 4;
        let mut offsets = Vec::new();
        let mut acc = header_size;
        for rec in &records {
            offsets.push(acc);
            acc += rec.len();
        }
        let mut page = vec![0u8; acc];
        page[..4].copy_from_slice(&0x100u32.to_be_bytes());
        page[4..8].copy_from_slice(&(records.len() as u32).to_le_bytes());
        for (i, off) in offsets.iter().enumerate() {
            page[8 + i * 4..12 + i * 4].copy_from_slice(&(*off as u32).to_le_bytes());
        }
        let mut pos = header_size;
        for rec in &records {
            page[pos..pos + rec.len()].copy_from_slice(rec);
            pos += rec.len();
        }
        page
    }

    fn build_fixture(pages: &[Vec<FxCookie>]) -> Vec<u8> {
        let page_bufs: Vec<Vec<u8>> = pages.iter().map(|p| build_page(p)).collect();
        let table_end = 8 + 4 * page_bufs.len();
        let total = table_end + page_bufs.iter().map(|p| p.len()).sum::<usize>();
        let mut out = vec![0u8; total];
        out[..4].copy_from_slice(b"cook");
        out[4..8].copy_from_slice(&(page_bufs.len() as u32).to_be_bytes());
        for (i, pb) in page_bufs.iter().enumerate() {
            out[8 + i * 4..12 + i * 4].copy_from_slice(&(pb.len() as u32).to_be_bytes());
        }
        let mut pos = table_end;
        for pb in &page_bufs {
            out[pos..pos + pb.len()].copy_from_slice(pb);
            pos += pb.len();
        }
        out
    }

    #[test]
    fn binarycookies_single_page() {
        let fx = vec![
            FxCookie { url: "https://hongguoduanju.com/", name: "sid", value: "aaa.bbb", path: "/" },
            FxCookie { url: "https://example.com/", name: "other", value: "zz", path: "/" },
        ];
        let cookies = parse_binary_cookies(&build_fixture(&[fx])).unwrap();
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].name, "sid");
        assert_eq!(cookies[0].value, "aaa.bbb");
        assert!(cookies[0].url.contains("hongguoduanju.com"));
    }

    #[test]
    fn binarycookies_multi_page_layout() {
        let pages = vec![
            vec![FxCookie { url: "https://a.com/", name: "x", value: "1", path: "/" }],
            vec![
                FxCookie { url: "https://hongguoduanju.com/", name: "y", value: "2", path: "/" },
                FxCookie { url: "https://hongguoduanju.com/", name: "z", value: "3", path: "/" },
            ],
        ];
        let cookies = parse_binary_cookies(&build_fixture(&pages)).unwrap();
        assert_eq!(cookies.len(), 3);
        let hits: Vec<&RawCookie> = cookies
            .iter()
            .filter(|c| c.url.contains("hongguoduanju.com"))
            .collect();
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].value.as_str(), hits[1].value.as_str()), ("2", "3"));
    }

    #[test]
    fn binarycookies_rejects_bad_magic_and_page_count() {
        assert!(parse_binary_cookies(b"nope12345678").is_err());
        let mut bad = build_fixture(&[vec![]]);
        bad[4..8].copy_from_slice(&70000u32.to_be_bytes());
        assert!(parse_binary_cookies(&bad).is_err());
    }

    #[test]
    fn mask_never_shows_full_value() {
        assert_eq!(mask("abcdefgh"), "abcd****(len 8)");
        assert_eq!(mask("ab"), "ab****(len 2)");
        assert_eq!(mask(""), "****(len 0)");
    }

    #[test]
    fn dedup_keeps_last_value_per_name() {
        let pairs = vec![
            ("SID".to_string(), "old".to_string()),
            ("other".to_string(), "x".to_string()),
            ("SID".to_string(), "new".to_string()),
        ];
        let out = dedup_pairs(pairs);
        assert_eq!(
            out,
            vec![
                ("SID".to_string(), "new".to_string()),
                ("other".to_string(), "x".to_string())
            ]
        );
    }

    #[test]
    fn profiles_default_first() {
        let dir = std::env::temp_dir().join(format!("guo-profiles-test-{}", std::process::id()));
        for name in ["Profile 2", "Default", "Guest Profile", "Profile 1"] {
            let p = dir.join(name);
            std::fs::create_dir_all(&p).unwrap();
            std::fs::write(p.join("Cookies"), b"stub").unwrap();
        }
        let profiles = chrome_profiles(&dir);
        let names: Vec<String> = profiles
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.first().map(String::as_str), Some("Default"));
        assert_eq!(
            names.last().map(String::as_str),
            Some("Guest Profile"),
            "Guest 排最尾: {:?}",
            names
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
