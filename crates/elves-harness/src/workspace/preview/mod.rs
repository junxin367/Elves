//! 工作区只读预览；旧文件接口保留原有响应，新能力不开放通用原文件下载。
use super::*;
use serde::Deserialize;
use std::io::{Seek, SeekFrom};

mod formats;
pub(crate) mod media;
mod text;

const SAMPLE: usize = 4096;
const CHUNK: usize = 256 * 1024;

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct Window {
    direction: Option<String>,
    offset: Option<u64>,
    version: Option<String>,
    page: Option<usize>,
    sheet: Option<usize>,
    representation: Option<String>,
}

fn io_error(_: std::io::Error) -> ApiError {
    ApiError::new(503, "无法读取文件，请检查原文件或重试")
}
fn changed() -> ApiError {
    ApiError::new(409, "原文件已变化，请重新打开")
}

fn version(file: &File) -> Result<String, ApiError> {
    let m = file.metadata().map_err(io_error)?;
    #[cfg(windows)]
    let identity = {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::{
            Foundation::HANDLE,
            Storage::FileSystem::{
                BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FileBasicInfo,
                GetFileInformationByHandle, GetFileInformationByHandleEx,
            },
        };
        let mut info = BY_HANDLE_FILE_INFORMATION::default();
        unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
            .map_err(|_| changed())?;
        let mut basic = FILE_BASIC_INFO::default();
        unsafe {
            GetFileInformationByHandleEx(
                HANDLE(file.as_raw_handle()),
                FileBasicInfo,
                (&mut basic as *mut FILE_BASIC_INFO).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        }
        .map_err(|_| changed())?;
        (
            info.dwVolumeSerialNumber as u64,
            ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64,
            basic.ChangeTime,
        )
    };
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        (m.dev(), m.ino(), m.ctime(), m.ctime_nsec())
    };
    let modified = m.modified().map_err(io_error)?;
    Ok(crate::hash(&format!(
        "{identity:?}:{}:{modified:?}",
        m.len()
    )))
}

fn sample(file: &mut File, size: u64) -> Result<Vec<u8>, ApiError> {
    file.rewind().map_err(io_error)?;
    let mut buf = vec![0; size.min(SAMPLE as u64) as usize];
    file.read_exact(&mut buf).map_err(io_error)?;
    Ok(buf)
}

fn inspect(root: &Path, path: &str, w: &Window, desktop: bool) -> Result<Value, ApiError> {
    let mut file =
        open_local(root, path)?.ok_or_else(|| ApiError::new(404, "文件不存在或已被删除"))?;
    let size = file.metadata().map_err(io_error)?.len();
    let ver = version(&file)?;
    if w.version.as_ref().is_some_and(|v| v != &ver) {
        return Err(changed());
    }
    let prefix = sample(&mut file, size)?;
    let ext = Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let delimited = matches!(ext.as_str(), "csv" | "tsv");
    let mut result =
        if delimited && (w.representation.as_deref() == Some("text") || w.direction.is_some()) {
            let enc = text::encoding(&prefix)
                .ok_or_else(|| ApiError::new(422, "表格不是可识别的文本编码"))?;
            text::window(&mut file, size, enc, &ext, w)?
        } else if let Some(mime) = media::mime(&ext, &prefix) {
            json!({"kind":"media","mime":mime})
        } else if media::extension(&ext) {
            json!({"kind":"unsupported","message":"音视频格式无法识别或文件已损坏"})
        } else if let Some(value) = formats::inspect(&mut file, size, &ext, &prefix, w, desktop)? {
            value
        } else if size <= MAX_TEXT_BYTES as u64 {
            file.rewind().map_err(io_error)?;
            let mut bytes = vec![0; size as usize];
            file.read_exact(&mut bytes).map_err(io_error)?;
            file_content_from_side(
                root,
                path,
                ext == "log",
                Side {
                    exists: true,
                    size,
                    bytes,
                    truncated: false,
                    unsupported: false,
                    stamp: None,
                },
            )?
        } else if let Some(enc) = text::encoding(&prefix) {
            text::window(&mut file, size, enc, &ext, w)?
        } else {
            json!({"kind":"unsupported","message":"此文件类型暂不支持内容预览"})
        };
    if version(&file)? != ver {
        return Err(changed());
    }
    result["ok"] = json!(true);
    result["root"] = json!(display_project_path(root));
    result["path"] = json!(path);
    result["size"] = json!(size);
    result["format"] = json!(if ext.is_empty() {
        "未指定格式".to_owned()
    } else {
        ext.chars()
            .filter(|c| !c.is_control())
            .take(24)
            .collect::<String>()
            .to_uppercase()
    });
    result["fileVersion"] = json!(ver);
    if ext == "pdf" && w.representation.as_deref() == Some("text") {
        result["pdfTextMode"] = json!(true);
    }
    if delimited
        && matches!(result["kind"].as_str(), Some("spreadsheet" | "unsupported"))
        && (result["truncated"] == true || result["kind"] == "unsupported")
        && text::encoding(&prefix).is_some()
    {
        result["tableTextAvailable"] = json!(true);
    }
    if result.get("truncated").is_none() {
        result["truncated"] = json!(false);
    }
    Ok(result)
}

pub(in crate::workspace) async fn resolve(
    state: &WebState,
    base: &Value,
    auth: &HarnessAuthorization,
) -> Result<(PathBuf, String), ApiError> {
    let path = base["path"]
        .as_str()
        .filter(|p| !p.is_empty() && p.len() <= 16 * 1024)
        .ok_or_else(|| ApiError::new(400, "文件路径无效"))?;
    let (cwd, ctx) = crate::workspace_file_context(state, base, auth).await?;
    resolve_file_target(cwd, &ctx, path, false, auth).await
}

pub(crate) async fn read(
    State(state): State<WebState>,
    headers: HeaderMap,
    Json(mut base): Json<Value>,
) -> Result<Response, ApiError> {
    let device = crate::authorized(&state, &headers).await?;
    let auth = state.controller.authorization(device, None);
    auth.check().await.map_err(crate::authorization_error)?;
    let obj = base
        .as_object_mut()
        .ok_or_else(|| ApiError::new(400, "文件查看请求无效"))?;
    let w: Window = serde_json::from_value(obj.remove("window").unwrap_or(json!({})))
        .map_err(|_| ApiError::new(400, "文件预览范围无效"))?;
    if obj
        .keys()
        .any(|k| !matches!(k.as_str(), "path" | "threadId" | "projectId" | "ifRevision"))
        || w.direction
            .as_deref()
            .is_some_and(|s| !matches!(s, "head" | "tail" | "next" | "prev"))
        || w.offset.is_some() != matches!(w.direction.as_deref(), Some("next" | "prev"))
        || w.representation.as_deref().is_some_and(|s| s != "text")
        || w.direction.is_some()
            && (w.representation.is_some() || w.page.is_some() || w.sheet.is_some())
        || w.version
            .as_ref()
            .is_some_and(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
        || (w.direction.is_some()
            || w.offset.is_some()
            || w.page.is_some()
            || w.sheet.is_some()
            || w.representation.is_some())
            && w.version.is_none()
    {
        return Err(ApiError::new(400, "文件预览参数或版本无效"));
    }
    let revision = requested_revision(&base)?;
    let result = tokio::time::timeout(REQUEST_TIMEOUT, async {
        let (root, path) = resolve(&state, &base, &auth).await?;
        let branch_dir = root.join(&path).parent().unwrap_or(&root).to_path_buf();
        let scope = (root.clone(), path.clone());
        // 取消等待不会杀掉已进入阻塞线程的解析；将许可随解析任务持有，避免远程反复取消堆积解码。
        let permit = state
            .controller
            .workspace_preview_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::new(429, "文件预览繁忙，请稍后重试"))?;
        let desktop = state.desktop;
        let value = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            inspect(&root, &path, &w, desktop)
        })
        .await
        .map_err(|_| ApiError::new(503, "文件预览任务失败"))??;
        let mut value = value;
        if matches!(value["kind"].as_str(), Some("media" | "picture")) {
            let id = media::register(&state, &auth, &base, scope, &value)?;
            value["resource"] = json!(format!("/api/workspace/media/{id}"));
        }
        value["git"] = branch_metadata(&branch_dir, &auth).await;
        Ok::<_, ApiError>(value)
    })
    .await
    .map_err(|_| ApiError::new(504, "文件读取超时"))??;
    auth.check().await.map_err(crate::authorization_error)?;
    Ok(no_store(revision_payload(result, revision)?))
}
