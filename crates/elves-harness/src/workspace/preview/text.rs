use super::*;

#[derive(Clone, Copy)]
pub(super) enum Encoding {
    Utf8(usize),
    Utf16(bool),
}
pub(super) fn encoding(prefix: &[u8]) -> Option<Encoding> {
    if prefix.starts_with(&[0xff, 0xfe]) {
        return Some(Encoding::Utf16(true));
    }
    if prefix.starts_with(&[0xfe, 0xff]) {
        return Some(Encoding::Utf16(false));
    }
    if prefix.iter().any(|&b| b < 0x20 && !matches!(b, 9..=13)) {
        return None;
    }
    if std::str::from_utf8(prefix).is_err_and(|e| e.error_len().is_some()) {
        return None;
    }
    Some(Encoding::Utf8(if prefix.starts_with(&[0xef, 0xbb, 0xbf]) {
        3
    } else {
        0
    }))
}

// 用户要求大文本不按总大小拒读；前进用上一段 end，后退用 start，避免跨字符重复或遗漏。
pub(super) fn window(
    f: &mut File,
    size: u64,
    enc: Encoding,
    ext: &str,
    w: &Window,
) -> Result<Value, ApiError> {
    let bom = match enc {
        Encoding::Utf8(n) => n as u64,
        Encoding::Utf16(_) => 2,
    };
    let direction = w
        .direction
        .as_deref()
        .unwrap_or(if ext == "log" { "tail" } else { "head" });
    let (start, end) = match direction {
        "head" => (bom, size.min(bom + CHUNK as u64)),
        "tail" => (size.saturating_sub(CHUNK as u64).max(bom), size),
        dir @ ("next" | "prev") => {
            let pos = w
                .offset
                .filter(|n| *n >= bom && *n <= size)
                .ok_or_else(|| ApiError::new(400, "片段位置无效"))?;
            if dir == "next" {
                (pos, size.min(pos.saturating_add(CHUNK as u64)))
            } else {
                (pos.saturating_sub(CHUNK as u64).max(bom), pos)
            }
        }
        _ => return Err(ApiError::new(400, "片段方向无效")),
    };
    let invalid = || ApiError::new(422, "此片段不是有效文本，无法按所选编码预览");
    f.seek(SeekFrom::Start(start)).map_err(io_error)?;
    let mut b = vec![0; (end - start) as usize];
    f.read_exact(&mut b).map_err(io_error)?;
    let (mut lo, mut hi) = (0, b.len());
    let mut text = match enc {
        Encoding::Utf8(_) => {
            if start > bom {
                while lo < hi && lo < 3 && b[lo] & 0xc0 == 0x80 {
                    lo += 1;
                }
            }
            match std::str::from_utf8(&b[lo..hi]) {
                Ok(s) => s.to_owned(),
                Err(e) if end < size && e.error_len().is_none() => {
                    hi = lo + e.valid_up_to();
                    std::str::from_utf8(&b[lo..hi])
                        .map_err(|_| invalid())?
                        .to_owned()
                }
                _ => return Err(invalid()),
            }
        }
        Encoding::Utf16(le) => {
            if (size - bom) % 2 != 0 || (start - bom) % 2 != 0 || (end - bom) % 2 != 0 {
                return Err(invalid());
            }
            let unit = |i| {
                let a = [b[i], b[i + 1]];
                if le {
                    u16::from_le_bytes(a)
                } else {
                    u16::from_be_bytes(a)
                }
            };
            if hi >= 2 && start > bom && (0xdc00..=0xdfff).contains(&unit(0)) {
                lo += 2;
            }
            if hi >= lo + 2 && end < size && (0xd800..=0xdbff).contains(&unit(hi - 2)) {
                hi -= 2;
            }
            let units: Vec<_> = (lo..hi).step_by(2).map(unit).collect();
            String::from_utf16(&units).map_err(|_| invalid())?
        }
    };
    if text.contains('\0') {
        return Err(invalid());
    }
    // UTF-16 中文转成 UTF-8 后会变长。正文仍限 256 KiB，移动原始游标时按保留字符的 UTF-16 单元数换算。
    if text.len() > CHUNK {
        if matches!(direction, "tail" | "prev") {
            let mut cut = text.len() - CHUNK;
            while !text.is_char_boundary(cut) {
                cut += 1;
            }
            lo += text[..cut].encode_utf16().count() * 2;
            text.drain(..cut);
        } else {
            let mut cut = CHUNK;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            hi = lo + text.encode_utf16().count() * 2;
        }
    }
    Ok(
        json!({"kind":"textWindow","text":text,"start":start+lo as u64,"end":start+hi as u64,"first":bom,"truncated":start+lo as u64>bom || start+(hi as u64)<size}),
    )
}
