//! 只在 Elves 本机 Codex 会话使用的路径图片。模型读取原图，HTTP 预览不参与模型输入。
use std::io::{BufReader, Seek};
use std::path::{Path as FsPath, PathBuf};

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Response};
use image::{ImageFormat, ImageReader, Limits};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use super::{ApiError, HarnessAuthorization, WebState, desktop, input_files};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Slot {
    id: String,
    index: usize,
    entries: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    name: String,
    mime: String,
    size: u64,
    staged: Slot,
}

fn invalid(message: &str) -> ApiError {
    ApiError::new(400, message)
}

fn image_ref(value: &Value) -> Result<Image, ApiError> {
    let image: Image = serde_json::from_value(value.clone())
        .map_err(|_| invalid("本机图片引用无效，请重新添加"))?;
    if image.size == 0
        || image.staged.entries != 1
        || image.name.is_empty()
        || image.name.len() > 255
        || !matches!(
            image.mime.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        )
    {
        return Err(invalid("本机图片引用无效，请重新添加"));
    }
    Ok(image)
}

pub(super) fn validate_ref(value: &Value) -> Result<(), ApiError> {
    image_ref(value).map(|_| ())
}

pub(super) fn has_refs(payload: &Value) -> bool {
    payload["images"]
        .as_array()
        .is_some_and(|images| images.iter().any(|image| image.get("staged").is_some()))
}

fn inspect_original(path: &FsPath) -> Result<(&'static str, ImageFormat, std::fs::File), ApiError> {
    let (eligible, extension_mime) =
        super::eligible_image_path(path.to_str().ok_or_else(|| invalid("本机图片路径无效"))?)
            .ok_or_else(|| invalid("本机图片格式不支持，请重新添加"))?;
    let actual = eligible
        .canonicalize()
        .map_err(|_| invalid("本机图片已不可读，请重新添加"))?;
    if !super::canonical_path_is_local(&actual) {
        return Err(ApiError::new(403, "图片不在本机固定磁盘"));
    }
    let file = std::fs::File::open(actual).map_err(|_| invalid("本机图片已不可读，请重新添加"))?;
    let mut reader = ImageReader::new(BufReader::new(
        file.try_clone().map_err(|_| invalid("本机图片已不可读"))?,
    ))
    .with_guessed_format()
    .map_err(|_| invalid("本机图片内容无效"))?;
    let format = reader.format().ok_or_else(|| invalid("本机图片内容无效"))?;
    let mime = super::image_format_mime(format)
        .filter(|mime| *mime == extension_mime)
        .ok_or_else(|| invalid("本机图片的格式与实际内容不一致"))?;
    // 本机原图不套内联字节额度；仍在读取完整像素前核验格式和解码尺寸。
    let mut limits = Limits::default();
    limits.max_image_width = Some(super::MAX_REMOTE_IMAGE_DIMENSION);
    limits.max_image_height = Some(super::MAX_REMOTE_IMAGE_DIMENSION);
    limits.max_alloc = Some(128 * 1024 * 1024);
    reader.limits(limits);
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| invalid("本机图片内容损坏或尺寸无效"))?;
    if width == 0
        || height == 0
        || width > super::MAX_REMOTE_IMAGE_DIMENSION
        || height > super::MAX_REMOTE_IMAGE_DIMENSION
        || u64::from(width) * u64::from(height) > super::MAX_REMOTE_IMAGE_PIXELS
    {
        return Err(ApiError::new(413, "本机图片尺寸过大"));
    }
    Ok((mime, format, file))
}

pub(super) async fn resolve(
    directory: PathBuf,
    payload: &mut Value,
    authorization: &HarnessAuthorization,
) -> anyhow::Result<()> {
    let images = payload["images"].as_array().cloned().unwrap_or_default();
    let owner = authorization.device.id.clone();
    authorization.check().await?;
    let images = tokio::task::spawn_blocking(move || -> Result<Vec<Value>, ApiError> {
        images
            .into_iter()
            .map(|entry| {
                if entry.get("staged").is_none() {
                    return Ok(entry);
                }
                let image = image_ref(&entry)?;
                let (path, size) = input_files::staged_image_file(
                    &directory,
                    &owner,
                    &image.staged.id,
                    image.staged.index,
                )?;
                if path.file_name().and_then(|name| name.to_str()) != Some(&image.name)
                    || size != image.size
                    || inspect_original(&path)?.0 != image.mime
                {
                    return Err(invalid("本机图片与原选择不一致，请重新添加"));
                }
                // localPath 只由 Harness 解析授权下的暂存引用，绝不接受页面指定的绝对路径。
                Ok(json!({"name":image.name,"mime":image.mime,"size":size,"localPath":path}))
            })
            .collect()
    })
    .await
    .map_err(|_| ApiError::new(503, "读取本机图片的任务已中断"))??;
    authorization.check().await?;
    payload["images"] = json!(images);
    Ok(())
}

pub(super) async fn preview(
    State(state): State<WebState>,
    Path((id, index)): Path<(String, usize)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let device = super::authorized(&state, &headers).await?;
    response(&state, device, id, index, None, false).await
}

pub(super) async fn thumbnail(
    State(state): State<WebState>,
    Path((id, index)): Path<(String, usize)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let device = super::authorized(&state, &headers).await?;
    response(&state, device, id, index, None, true).await
}

pub(super) async fn response(
    state: &WebState,
    device: super::Device,
    id: String,
    index: usize,
    expected_path: Option<PathBuf>,
    thumbnail: bool,
) -> Result<Response, ApiError> {
    if !state.desktop || device.key_id != desktop::DEVICE_KEY_ID {
        return Err(ApiError::new(
            403,
            "本机暂存图片只能在 Elves 独立客户端查看",
        ));
    }
    let permit = if thumbnail {
        Some(
            state
                .controller
                .image_preview_slots
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ApiError::new(503, "图片预览暂不可用"))?,
        )
    } else {
        None
    };
    let directory = state.controller.state_dir.clone();
    let owner = device.id.clone();
    let (mime, size, file, preview) = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let (path, size) = input_files::staged_image_file(&directory, &owner, &id, index)?;
        if expected_path.is_some_and(|expected| expected != path) {
            return Err(invalid("本机图片已变化，请重新添加"));
        }
        let (mime, format, mut file) = inspect_original(&path)?;
        let preview = if thumbnail {
            super::image_preview::file_thumbnail(&file, format, size)
        } else {
            None
        };
        // try_clone 共享文件游标；原图响应（含小文件回退）必须从原始文件开头读取。
        if preview.is_none() {
            file.rewind()
                .map_err(|_| invalid("本机图片已不可读，请重新添加"))?;
        }
        Ok::<_, ApiError>((mime, size, file, preview))
    })
    .await
    .map_err(|_| ApiError::new(503, "无法读取本机图片"))??;
    let authorization = state.controller.authorization(device, None);
    authorization
        .check()
        .await
        .map_err(super::authorization_error)?;
    if let Some(preview) = preview {
        return Ok(([(header::CONTENT_TYPE, preview.mime)], preview.bytes).into_response());
    }
    if thumbnail && size > 1024 * 1024 {
        // 派生图超出解码预算时，小图位显示占位，不为 40px 的缩略图自动加载巨幅原图。
        return Ok(([(header::CONTENT_TYPE, "image/svg+xml")],
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 40"><rect x="8" y="8" width="24" height="24" rx="3" fill="none" stroke="#888" stroke-width="2"/><circle cx="16" cy="16" r="2" fill="#888"/><path d="M9 28l7-7 5 5 4-5 6 7" fill="none" stroke="#888" stroke-width="2"/></svg>"##).into_response());
    }
    // 每次只读取固定大小的数据块；大图查看不需要把原图一次性放进 HTTP 响应缓冲区。
    let stream = futures_util::stream::try_unfold(
        tokio::fs::File::from_std(file).take(size),
        |mut file| async {
            let mut bytes = vec![0; 64 * 1024];
            let count = file.read(&mut bytes).await?;
            bytes.truncate(count);
            Ok::<_, std::io::Error>((count != 0).then(|| (Bytes::from(bytes), file)))
        },
    );
    Ok(([(header::CONTENT_TYPE, mime)], Body::from_stream(stream)).into_response())
}
