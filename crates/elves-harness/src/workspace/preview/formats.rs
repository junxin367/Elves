use super::*;
use std::io::{BufReader, Cursor};

mod pdf;
const PAGE: usize = 50;
fn unsupported(s: &str) -> Value {
    json!({"kind":"unsupported","message":s})
}
pub(super) fn inspect(
    f: &mut File,
    size: u64,
    ext: &str,
    head: &[u8],
    w: &Window,
    desktop: bool,
) -> Result<Option<Value>, ApiError> {
    if let Some(mime) = raster_mime(head) {
        f.rewind().map_err(io_error)?;
        // 限量读取图片头；文件很小不代表解码后的像素和内存也很小。SVG 不进入原件资源路由。
        let mut info = Vec::new();
        Read::by_ref(f)
            .take(256 * 1024)
            .read_to_end(&mut info)
            .map_err(io_error)?;
        let format =
            image::guess_format(&info).map_err(|_| ApiError::new(422, "图片格式无法识别"))?;
        let Ok((width, height)) =
            image::ImageReader::with_format(Cursor::new(info), format).into_dimensions()
        else {
            return Ok(Some(unsupported(
                "无法在有限的文件头内确认图片尺寸，未加载原图",
            )));
        };
        if width == 0
            || height == 0
            || width > 16384
            || height > 16384
            || u64::from(width) * u64::from(height) > 16_000_000
            || size > 64 * 1024 * 1024
        {
            return Ok(Some(unsupported(
                "图片超过安全解码预算（1600 万像素、单边 16384 或原件 64 MiB），未加载原图",
            )));
        }
        let thumbnail = if desktop {
            None
        } else {
            crate::image_preview::workspace_thumbnail(f, format, size)
                .map(|im| format!("data:{};base64,{}", im.mime, STANDARD.encode(im.bytes)))
        };
        return Ok(Some(
            json!({"kind":"picture","mime":mime,"width":width,"height":height,"thumbnail":thumbnail,"localOriginal":desktop}),
        ));
    }
    if ext == "svg" && size > MAX_IMAGE_BYTES as u64 {
        return Ok(Some(unsupported("SVG 超出安全校验预算，未作为图像加载")));
    }
    if ext == "pdf" {
        if !head.starts_with(b"%PDF-") {
            return Ok(Some(unsupported("文件不是可识别的 PDF")));
        }
        if w.representation.as_deref() == Some("text") && size <= MAX_TEXT_BYTES as u64 {
            return Ok(None);
        }
        let mut value = pdf::page(f, size, w.page.unwrap_or(0)).unwrap_or_else(|e| unsupported(&e));
        if value["kind"] == "unsupported" && size <= MAX_TEXT_BYTES as u64 {
            value["pdfTextAvailable"] = json!(true);
        }
        return Ok(Some(value));
    }
    if matches!(
        ext,
        "zip" | "xlsx" | "xlsm" | "xltx" | "ods" | "docx" | "odt" | "pptx"
    ) {
        // 先校验中央目录自身的尺寸和实际条目数，再让 ZIP 解析器建立索引；不按声明分配无界内存。
        let admission = zip_admission(f, size);
        if let Err(err) = admission {
            return Ok(Some(unsupported(&err)));
        }
        f.rewind().map_err(io_error)?;
        if ext == "zip" {
            let Ok(mut z) = zip::ZipArchive::new(BufReader::new(f)) else {
                return Ok(Some(unsupported("压缩包目录无法解析")));
            };
            let page = w.page.unwrap_or(0);
            let pages = z.len().div_ceil(PAGE).max(1);
            if page >= pages {
                return Err(ApiError::new(400, "压缩包目录页无效"));
            }
            let mut items = Vec::new();
            for i in page * PAGE..((page + 1) * PAGE).min(z.len()) {
                let e = z
                    .by_index_raw(i)
                    .map_err(|_| ApiError::new(422, "压缩包条目无法读取"))?;
                items.push(json!({"name":e.name().chars().filter(|c|!c.is_control()).take(512).collect::<String>(),"size":e.size(),"directory":e.is_dir()}));
            }
            return Ok(Some(
                json!({"kind":"archive","items":items,"page":page,"pages":pages,"count":z.len()}),
            ));
        }
        return Ok(Some(
            crate::document_preview::file_window(
                f,
                size,
                ext,
                w.sheet.unwrap_or(0),
                w.page.unwrap_or(0),
            )
            .unwrap_or_else(unsupported),
        ));
    }
    if matches!(ext, "xls" | "doc" | "ppt") {
        return Ok(Some(unsupported(
            "旧版 Office 二进制文件请另存为 XLSX、DOCX 或 PPTX 后查看",
        )));
    }
    if matches!(
        ext,
        "7z" | "rar"
            | "tar"
            | "gz"
            | "dwg"
            | "dxf"
            | "psd"
            | "sqlite"
            | "db"
            | "exe"
            | "dll"
            | "msi"
    ) {
        return Ok(Some(unsupported(
            "仅提供文件信息，不展开或执行此类型的文件",
        )));
    }
    Ok(None)
}

fn zip_admission(f: &mut File, size: u64) -> Result<(), String> {
    let bad = || "压缩包目录无效、格式不支持或超过安全预算".to_string();
    let tail_len = size.min(65_557);
    f.seek(SeekFrom::Start(size - tail_len))
        .map_err(|_| bad())?;
    let mut tail = vec![0; tail_len as usize];
    f.read_exact(&mut tail).map_err(|_| bad())?;
    let u16at = |b: &[u8], i| u16::from_le_bytes([b[i], b[i + 1]]) as usize;
    let u32at = |b: &[u8], i| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
    let i = tail
        .windows(4)
        .enumerate()
        .rev()
        .find(|(i, b)| {
            *b == b"PK\x05\x06"
                && i + 22 <= tail.len()
                && i + 22 + u16at(&tail, i + 20) == tail.len()
        })
        .map(|a| a.0)
        .ok_or_else(bad)?;
    let c = &tail[i..];
    let count = u16at(c, 10);
    let length = u32at(c, 12);
    let offset = u32at(c, 16);
    if i >= 20 && &tail[i - 20..i - 16] == b"PK\x06\x07" {
        return Err("ZIP64 目录暂不提供预览".into());
    }
    if c[4..8] != [0; 4]
        || u16at(c, 8) != count
        || count > 4096
        || length > 2 * 1024 * 1024
        || offset as u64 + length as u64 != size - tail_len + i as u64
    {
        return Err(bad());
    }
    let mut dir = vec![0; length];
    f.seek(SeekFrom::Start(offset as u64))
        .and_then(|_| f.read_exact(&mut dir))
        .map_err(|_| bad())?;
    let mut at = 0;
    for _ in 0..count {
        if at + 46 > dir.len() || dir[at..at + 4] != *b"PK\x01\x02" {
            return Err(bad());
        }
        if u16at(&dir, at + 8) & 1 != 0 {
            return Err("压缩包已加密，暂不提供预览".into());
        }
        at += 46 + u16at(&dir, at + 28) + u16at(&dir, at + 30) + u16at(&dir, at + 32);
    }
    if at != dir.len() {
        return Err(bad());
    }
    Ok(())
}
