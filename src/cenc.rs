//! Progressive MP4 CENC decryptor (ISO/IEC 23001-7, AES-128-CTR full-sample).
//! Decrypts `mdat` sample data in place and flips the sample-entry fourcc from
//! `encv`/`enca` to the clear codec from `frma`. Box sizes are left untouched
//! so absolute chunk offsets stay valid.

use aes::cipher::{BlockEncrypt, KeyInit};
use std::io::{Read, Seek, SeekFrom, Write};

struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn u32(&self, off: usize) -> u32 {
        u32::from_be_bytes(self.data[off..off + 4].try_into().unwrap())
    }
    fn u64(&self, off: usize) -> u64 {
        u64::from_be_bytes(self.data[off..off + 8].try_into().unwrap())
    }
    fn fourcc(&self, off: usize) -> [u8; 4] {
        self.data[off..off + 4].try_into().unwrap()
    }
}

struct Track {
    /// fourcc offset of the sample entry inside stsd (for the flip).
    entry_fourcc_off: usize,
    clear_fourcc: [u8; 4],
    iv_size: usize,
    is_encrypted: bool,
    aux_offset: u64,
    aux_sizes: Vec<u8>,
    aux_default_size: u8,
    sample_sizes: Vec<u32>,
    chunk_offsets: Vec<u64>,
    stsc: Vec<(u32, u32)>,
}

/// Locate all boxes named `name` between start..end, returning (body_start,
/// body_end) pairs without descending.
fn boxes_named(data: &[u8], start: usize, end: usize, name: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let reader = Reader { data };
    let mut off = start;
    while off + 8 <= end {
        let size = reader.u32(off) as usize;
        let fourcc = reader.fourcc(off + 4);
        let mut header = 8;
        let mut extent = size;
        if size == 1 {
            header = 16;
            extent = reader.u64(off + 8) as usize;
        } else if size == 0 {
            extent = end - off;
        }
        if extent < header || off + extent > end {
            break;
        }
        if fourcc == name {
            out.push((off + header, off + extent));
        }
        off += extent;
    }
    out
}

/// Find the first box `name` whose body spans (start, end) inside parent.
fn box_body(data: &[u8], start: usize, end: usize, name: &[u8]) -> Option<(usize, usize)> {
    boxes_named(data, start, end, name).into_iter().next()
}

/// stsd sample entries begin after version/flags + entry_count.
fn parse_track(data: &[u8], trak_start: usize, trak_end: usize) -> Option<Track> {
    let (mdia_start, mdia_end) = box_body(data, trak_start, trak_end, b"mdia")?;
    let (minf_start, minf_end) = box_body(data, mdia_start, mdia_end, b"minf")?;
    let (stbl_start, stbl_end) = box_body(data, minf_start, minf_end, b"stbl")?;
    let (stsd_start, stsd_end) = box_body(data, stbl_start, stbl_end, b"stsd")?;
    // Sample entries start 8 bytes into stsd body.
    let entries_start = stsd_start + 8;
    let reader = Reader { data };
    let mut track: Option<Track> = None;
    let mut off = entries_start;
    while off + 8 <= stsd_end {
        let entry_size = reader.u32(off) as usize;
        if entry_size < 8 || off + entry_size > stsd_end {
            break;
        }
        let fourcc = reader.fourcc(off + 4);
        if fourcc == *b"encv" || fourcc == *b"enca" {
            // Child boxes sit after a fixed sample-entry header (78 bytes for
            // visual, 28 for audio); locate frma/tenc by scanning for their
            // fourccs instead of walking from a guessed offset.
            let find = |needle: &[u8; 4]| -> Option<usize> {
                (off + 8..off + entry_size).find(|p| p + 4 <= off + entry_size && &data[*p..*p + 4] == needle)
            };
            let frma_pos = find(b"frma");
            let tenc_pos = find(b"tenc");
            if let (Some(frma_pos), Some(tenc_pos)) = (frma_pos, tenc_pos) {
                // frma_pos is the tag location; the clear codec fourcc follows.
                let clear_fourcc = reader.fourcc(frma_pos + 4);
                // tenc body: version/flags(4) then either one or two reserved
                // bytes before isEncrypted; detect by locating a sane layout.
                let tenc_end = (tenc_pos - 4 + reader.u32(tenc_pos - 4) as usize).min(off + entry_size);
                let mut is_encrypted = false;
                let mut iv_size = 0usize;
                for reserved in [2usize, 1] {
                    // tenc_pos is the fourcc; body = +4 (version/flags) +4.
                    let probe = tenc_pos + 8 + reserved;
                    if probe + 2 > tenc_end {
                        break;
                    }
                    let enc = data[probe];
                    let ivs = data[probe + 1];
                    if (ivs == 0 || ivs == 8 || ivs == 16) && (enc == 0 || enc == 1) {
                        is_encrypted = enc == 1;
                        iv_size = ivs as usize;
                        break;
                    }
                }
                let mut aux_offset: u64 = 0;
                let mut aux_sizes: Vec<u8> = Vec::new();
                let mut aux_default_size: u8 = 0;
                if let Some((saio_body, saio_end)) =
                    box_body(data, stbl_start, stbl_end, b"saio")
                {
                    let version = data[saio_body];
                    let mut cursor = saio_body + 4;
                    if version & 1 != 0 {
                        cursor += 8; // aux_info_type + parameter
                    }
                    let count = reader.u32(cursor) as usize;
                    cursor += 4;
                    if count > 0 {
                        aux_offset = if version & 1 != 0 {
                            reader.u64(cursor)
                        } else {
                            reader.u32(cursor) as u64
                        };
                    }
                    let _ = saio_end;
                }
                if let Some((saiz_body, _)) = box_body(data, stbl_start, stbl_end, b"saiz") {
                    let flags = reader.u32(saiz_body) & 0xffffff;
                    let mut cursor = saiz_body + 4;
                    if flags & 1 != 0 {
                        cursor += 8;
                    }
                    aux_default_size = data[cursor];
                    let count = reader.u32(cursor + 1) as usize;
                    cursor += 5;
                    if aux_default_size == 0 {
                        aux_sizes = data[cursor..cursor + count].to_vec();
                    }
                }
                let mut sample_sizes: Vec<u32> = Vec::new();
                if let Some((stsz_body, _)) = box_body(data, stbl_start, stbl_end, b"stsz") {
                    let fixed = reader.u32(stsz_body + 4) as usize;
                    let count = reader.u32(stsz_body + 8) as usize;
                    if fixed != 0 {
                        sample_sizes = vec![fixed as u32; count];
                    } else {
                        sample_sizes = data[stsz_body + 12..stsz_body + 12 + count * 4]
                            .chunks_exact(4)
                            .map(|c| u32::from_be_bytes(c.try_into().unwrap()))
                            .collect();
                    }
                }
                let mut chunk_offsets: Vec<u64> = Vec::new();
                if let Some((stco_body, _)) =
                    box_body(data, stbl_start, stbl_end, b"stco")
                {
                    let count = reader.u32(stco_body + 4) as usize;
                    for index in 0..count {
                        chunk_offsets.push(reader.u32(stco_body + 8 + index * 4) as u64);
                    }
                } else if let Some((co64_body, _)) =
                    box_body(data, stbl_start, stbl_end, b"co64")
                {
                    let count = reader.u32(co64_body + 4) as usize;
                    for index in 0..count {
                        chunk_offsets.push(reader.u64(co64_body + 8 + index * 8));
                    }
                }
                let mut stsc: Vec<(u32, u32)> = Vec::new();
                if let Some((stsc_body, _)) = box_body(data, stbl_start, stbl_end, b"stsc") {
                    let count = reader.u32(stsc_body + 4) as usize;
                    for index in 0..count {
                        let base = stsc_body + 8 + index * 12;
                        let first_chunk = reader.u32(base);
                        let samples = reader.u32(base + 4);
                        stsc.push((first_chunk, samples));
                    }
                }
                track = Some(Track {
                    entry_fourcc_off: off + 4,
                    clear_fourcc,
                    iv_size,
                    is_encrypted,
                    aux_offset,
                    aux_sizes,
                    aux_default_size,
                    sample_sizes,
                    chunk_offsets,
                    stsc,
                });
            }
        }
        off += entry_size;
    }
    track
}

/// Increment the 128-bit big-endian counter block by one.
fn increment(counter: &mut [u8; 16]) {
    for byte in counter.iter_mut().rev() {
        let (value, carry) = byte.overflowing_add(1);
        *byte = value;
        if !carry {
            break;
        }
    }
}

struct StreamCtr {
    cipher: aes::Aes128,
    counter: [u8; 16],
    keystream: [u8; 16],
    pos: usize,
}

impl StreamCtr {
    fn new(key: &[u8; 16], iv: &[u8]) -> StreamCtr {
        let mut counter = [0u8; 16];
        let take = iv.len().min(16);
        counter[..take].copy_from_slice(&iv[..take]);
        StreamCtr {
            cipher: aes::Aes128::new_from_slice(key).expect("valid key"),
            counter,
            keystream: [0u8; 16],
            pos: 16,
        }
    }
    fn xor(&mut self, data: &mut [u8]) {
        use aes::cipher::generic_array::GenericArray;
        for byte in data.iter_mut() {
            if self.pos == 16 {
                let mut block = self.counter;
                self.cipher
                    .encrypt_block(GenericArray::from_mut_slice(&mut block));
                self.keystream = block;
                increment(&mut self.counter);
                self.pos = 0;
            }
            *byte ^= self.keystream[self.pos];
            self.pos += 1;
        }
    }
}

fn sample_offsets(track: &Track) -> Vec<u64> {
    let mut offsets = Vec::with_capacity(track.sample_sizes.len());
    if track.chunk_offsets.is_empty() || track.stsc.is_empty() {
        return offsets;
    }
    let mut sample_index = 0usize;
    let total_chunks = track.chunk_offsets.len();
    for (entry, (first_chunk, samples_per_chunk)) in track.stsc.iter().enumerate() {
        let first = *first_chunk as usize;
        if first == 0 || sample_index >= track.sample_sizes.len() {
            break;
        }
        let last = if entry + 1 < track.stsc.len() {
            (track.stsc[entry + 1].0 as usize - 1).min(total_chunks)
        } else {
            total_chunks
        };
        for chunk in first..=last {
            if chunk > total_chunks {
                break;
            }
            let mut offset = track.chunk_offsets[chunk - 1];
            for _ in 0..*samples_per_chunk {
                if sample_index >= track.sample_sizes.len() {
                    break;
                }
                offsets.push(offset);
                offset += track.sample_sizes[sample_index] as u64;
                sample_index += 1;
            }
        }
    }
    offsets
}

/// Decrypt a CENC-encrypted progressive MP4 held in `data`.
pub fn decrypt_buffer(data: &mut [u8], key: &[u8; 16]) -> Result<usize, String> {
    let (moov_body, moov_end) = box_body(data, 0, data.len(), b"moov")
        .ok_or_else(|| "MP4 缺少 moov，无法解密".to_string())?;
    let mut decrypted = 0usize;
    let mut tracks: Vec<Track> = Vec::new();
    for (trak_start, trak_end) in boxes_named(data, moov_body, moov_end, b"trak") {
        if let Some(track) = parse_track(data, trak_start, trak_end) {
            tracks.push(track);
        }
    }
    if tracks.is_empty() {
        return Err("MP4 没有加密轨道".into());
    }
    for track in tracks {
        if !track.is_encrypted || track.iv_size == 0 {
            continue;
        }
        let offsets = sample_offsets(&track);
        if offsets.len() != track.sample_sizes.len() {
            return Err(format!(
                "样表不一致（{} 样本 / {} 偏移），跳过该轨道",
                track.sample_sizes.len(),
                offsets.len()
            ));
        }
        let mut aux_cursor = track.aux_offset as usize;
        for (index, &offset) in offsets.iter().enumerate() {
            let aux_size = if track.aux_default_size != 0 {
                track.aux_default_size as usize
            } else {
                *track
                    .aux_sizes
                    .get(index)
                    .ok_or("加密辅助信息不足")? as usize
            };
            if aux_size == 0 {
                continue;
            }
            let aux_at = aux_cursor;
            aux_cursor += aux_size;
            if aux_at + aux_size > data.len() {
                return Err("加密辅助信息越界".into());
            }
            let iv = data[aux_at..aux_at + track.iv_size].to_vec();
            let mut cursor = aux_at + track.iv_size;
            let mut ranges: Vec<(usize, usize)> = Vec::new();
            if cursor + 2 <= aux_at + aux_size {
                let subsamples = u16::from_be_bytes([data[cursor], data[cursor + 1]]) as usize;
                cursor += 2;
                for _ in 0..subsamples {
                    if cursor + 6 > aux_at + aux_size {
                        return Err("子样本信息越界".into());
                    }
                    let clear = u16::from_be_bytes([data[cursor], data[cursor + 1]]) as usize;
                    let ciphered =
                        u32::from_be_bytes(data[cursor + 2..cursor + 6].try_into().unwrap())
                            as usize;
                    cursor += 6;
                    ranges.push((clear, ciphered));
                }
            }
            let sample_start = offset as usize;
            let sample_size = track.sample_sizes[index] as usize;
            if sample_start + sample_size > data.len() {
                return Err("样本数据越界".into());
            }
            let mut ctr = StreamCtr::new(key, &iv);
            let sample = &mut data[sample_start..sample_start + sample_size];
            if ranges.is_empty() {
                ctr.xor(sample);
                decrypted += sample_size;
            } else {
                let mut position = 0usize;
                for (clear, ciphered) in ranges {
                    position += clear;
                    if position + ciphered > sample.len() {
                        return Err("子样本范围越界".into());
                    }
                    ctr.xor(&mut sample[position..position + ciphered]);
                    position += ciphered;
                }
                decrypted += position;
            }
        }
        // Flip encv/enca to the clear fourcc so players treat it as plain
        // media. The leftover sinf box is ignored by decoders for clear types.
        data[track.entry_fourcc_off..track.entry_fourcc_off + 4]
            .copy_from_slice(&track.clear_fourcc);
    }
    Ok(decrypted)
}

/// Dump parsed track info for debugging (used by the hidden cenc-debug command).
pub fn inspect(path: &std::path::Path) -> String {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) => return format!("读取失败：{}", e),
    };
    let mut report = String::new();
    let (moov_body, moov_end) = match box_body(&data, 0, data.len(), b"moov") {
        Some(span) => span,
        None => return "no moov".into(),
    };
    for (ti, (trak_start, trak_end)) in
        boxes_named(&data, moov_body, moov_end, b"trak").into_iter().enumerate()
    {
        report.push_str(&format!("trak {}\n", ti));
        if let Some(track) = parse_track(&data, trak_start, trak_end) {
            let offsets = sample_offsets(&track);
            report.push_str(&format!(
                "  fourcc={:?} enc={} iv={} samples={} chunks={} stsc={} aux@{} defsz={} offs={:?}..\n",
                std::str::from_utf8(&track.clear_fourcc).unwrap_or("?"),
                track.is_encrypted,
                track.iv_size,
                track.sample_sizes.len(),
                track.chunk_offsets.len(),
                track.stsc.len(),
                track.aux_offset,
                track.aux_default_size,
                &offsets[..offsets.len().min(4)],
            ));
        } else {
            report.push_str("  (clear track)\n");
        }
    }
    report
}

/// Decrypt a CENC-encrypted progressive MP4 on disk in place.
pub fn decrypt_file(path: &std::path::Path, key: &[u8; 16]) -> Result<(), String> {
    let mut file = std::fs::File::options()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("打开文件失败：{}", e))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|e| format!("读取文件失败：{}", e))?;
    let decrypted = decrypt_buffer(&mut data, key)?;
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(&data))
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("写回文件失败：{}", e))?;
    eprintln!("  CENC 解密完成（{} 样本字节）", decrypted);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_increments_big_endian() {
        let mut counter = [0u8; 16];
        increment(&mut counter);
        assert_eq!(counter[15], 1);
        counter[..15].fill(0xff);
        counter[15] = 0xff;
        increment(&mut counter);
        // 128-bit wrap-around.
        assert_eq!(counter, [0u8; 16]);
    }

}
