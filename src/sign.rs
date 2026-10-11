use md5::Digest;

/// Signed headers for the Hongguo App API (X-Gorgon / X-Khronos family).
pub struct Signature {
    pub gorgon: String,
    pub khronos: String,
    pub ticket: String,
    pub stub: Option<String>,
}

/// Port of the original Go `signHongguoRequest`: MD5(query)+MD5(body) short
/// hashes, fixed XOR key, in-place rotate/xor mixing, then the `84 04 40 1c`
/// prefix. The mixing loop is in-place in the original, so byte 19 mixes the
/// already-modified byte 0 — preserved exactly here.
pub fn sign_request(query: &str, body: Option<&[u8]>, unix_secs: u32, unix_ms: i64) -> Signature {
    let mut payload = [0u8; 20];
    let qhash = md5::Md5::digest(query.as_bytes());
    payload[..4].copy_from_slice(&qhash[..4]);
    let mut stub = None;
    if let Some(bytes) = body {
        let bhash = md5::Md5::digest(bytes);
        payload[4..8].copy_from_slice(&bhash[..4]);
        stub = Some(hex::encode_upper(bhash));
    }
    payload[12..16].copy_from_slice(&[0, 6, 11, 28]);
    payload[16..20].copy_from_slice(&unix_secs.to_be_bytes());
    const KEY: [u8; 20] = [
        0x44, 0xb9, 0xb9, 0xd9, 0xa4, 0xae, 0xf9, 0xfc, 0xa4, 0x93, 0xaa, 0x75, 0x7c, 0xa3, 0xc2,
        0xc4, 0xa4, 0x96, 0x93, 0x8f,
    ];
    for i in 0..20 {
        payload[i] ^= KEY[i];
    }
    for i in 0..20 {
        let mixed = payload[i].rotate_left(4) ^ payload[(i + 1) % 20];
        payload[i] = mixed.reverse_bits() ^ 0xff ^ 20;
    }
    let mut sig = Vec::with_capacity(26);
    sig.extend_from_slice(&[0x84, 0x04, 0x40, 0x1c, 0, 0]);
    sig.extend_from_slice(&payload);
    Signature {
        gorgon: hex::encode(sig),
        khronos: unix_secs.to_string(),
        ticket: unix_ms.to_string(),
        stub,
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Random numeric device id in the same range as the original.
pub fn new_device_id() -> String {
    let mut raw = [0u8; 8];
    getrandom(&mut raw);
    let n = u64::from_be_bytes(raw) % 8_000_000_000_000_000_000;
    (1_000_000_000_000_000_000 + n).to_string()
}

fn getrandom(buf: &mut [u8]) {
    // Read from /dev/urandom directly; no external rng dependency.
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(buf);
        return;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e3779b97f4a7c15);
    let mut state = nanos | 1;
    for slot in buf.iter_mut() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *slot = state as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Expected value computed with an independent Python port of the same
    /// Go algorithm (see README 开发笔记).
    #[test]
    fn gorgon_matches_reference() {
        let body = b"{\"series_id\":\"7632003395225668670\"}".to_vec();
        let sig = sign_request(
            "_rticket=1728650000000&aid=8662&device_id=123456789012345678",
            Some(&body),
            1728650000,
            1728650000000,
        );
        assert_eq!(
            sig.gorgon,
            "8404401c0000507a4cd3da2810997022103fad22c9992e65037e"
        );
        assert_eq!(sig.khronos, "1728650000");
        assert_eq!(sig.ticket, "1728650000000");
        assert_eq!(
            sig.stub.as_deref(),
            Some("B8E3EE5204E67145CF999187E05AAA2B")
        );
    }

    #[test]
    fn get_request_has_no_stub() {
        let sig = sign_request("a=1", None, 100, 100_000);
        assert_eq!(sig.stub, None);
        assert_eq!(sig.gorgon.len(), 52);
    }
}
