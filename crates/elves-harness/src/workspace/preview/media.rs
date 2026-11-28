use super::*;
use axum::http::{Method, StatusCode};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncSeekExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(super) fn extension(ext: &str) -> bool {
    matches!(
        ext,
        "mp4"
            | "m4v"
            | "mov"
            | "webm"
            | "ogv"
            | "ogg"
            | "oga"
            | "opus"
            | "mp3"
            | "m4a"
            | "aac"
            | "wav"
            | "flac"
            | "mkv"
            | "avi"
            | "wmv"
    )
}
pub(super) fn mime(ext: &str, b: &[u8]) -> Option<&'static str> {
    match ext {
        "mp4" | "m4v" if b.get(4..8) == Some(b"ftyp") => Some("video/mp4"),
        "mov" if b.get(4..8) == Some(b"ftyp") => Some("video/quicktime"),
        "m4a" if b.get(4..8) == Some(b"ftyp") => Some("audio/mp4"),
        "webm" if b.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) => Some("video/webm"),
        "ogv" if b.starts_with(b"OggS") => Some("video/ogg"),
        "ogg" | "oga" | "opus" if b.starts_with(b"OggS") => Some("audio/ogg"),
        "mp3"
            if b.starts_with(b"ID3")
                || b.len() >= 4 && b[0] == 0xff && b[1] & 0xe0 == 0xe0 && b[1] & 6 != 0 =>
        {
            Some("audio/mpeg")
        }
        "aac" if b.len() >= 4 && b[0] == 0xff && b[1] & 0xf6 == 0xf0 => Some("audio/aac"),
        "wav" if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WAVE") => Some("audio/wav"),
        "flac" if b.starts_with(b"fLaC") => Some("audio/flac"),
        _ => None,
    }
}

#[derive(Clone, PartialEq)]
struct Lease {
    owner: String,
    desktop: bool,
    identity: Value,
    root: PathBuf,
    path: String,
    version: String,
    mime: String,
}
#[derive(Default)]
struct Entries {
    items: VecDeque<(String, Lease)>,
    slots: HashMap<String, std::sync::Weak<Semaphore>>,
}
#[derive(Default)]
pub(crate) struct Registry(Mutex<Entries>);
fn owner(a: &HarnessAuthorization) -> String {
    crate::hash(&format!(
        "{}:{}:{}",
        a.device.id, a.device.key_id, a.device.token_hash
    ))
}
pub(super) fn register(
    state: &WebState,
    auth: &HarnessAuthorization,
    base: &Value,
    scope: (PathBuf, String),
    value: &Value,
) -> Result<String, ApiError> {
    let mut identity = crate::workspace_identity(base)?;
    identity["path"] = base["path"].clone();
    let key = Lease {
        owner: owner(auth),
        desktop: state.desktop,
        identity,
        root: scope.0,
        path: scope.1,
        version: value["fileVersion"].as_str().unwrap_or_default().into(),
        mime: value["mime"].as_str().unwrap_or_default().into(),
    };
    let mut reg = state
        .controller
        .workspace_previews
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(pos) = reg.items.iter().position(|(_, item)| item == &key) {
        let (id, entry) = reg.items.remove(pos).unwrap();
        reg.items.push_back((id.clone(), entry));
        return Ok(id);
    }
    if reg
        .items
        .iter()
        .filter(|(_, e)| e.owner == key.owner)
        .count()
        >= 64
        && let Some(i) = reg.items.iter().position(|(_, e)| e.owner == key.owner)
    {
        reg.items.remove(i);
    }
    if reg.items.len() >= 512 {
        reg.items.pop_front();
    }
    let id = crate::random_token();
    reg.items.push_back((id.clone(), key));
    Ok(id)
}
fn lookup(state: &WebState, auth: &HarnessAuthorization, id: &str) -> Result<Lease, ApiError> {
    let key = owner(auth);
    state
        .controller
        .workspace_previews
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .items
        .iter()
        .find(|(i, v)| i == id && v.owner == key && v.desktop == state.desktop)
        .map(|(_, l)| l.clone())
        .ok_or_else(|| ApiError::new(404, "文件预览已失效，请重新打开"))
}
fn slot(state: &WebState, lease: &Lease) -> Result<Option<OwnedSemaphorePermit>, ApiError> {
    if state.desktop {
        return Ok(None);
    }
    let mut reg = state
        .controller
        .workspace_previews
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    reg.slots.retain(|_, s| s.strong_count() > 0);
    let cell = reg.slots.entry(lease.owner.clone()).or_default();
    let sem = cell.upgrade().unwrap_or_else(|| {
        let s = Arc::new(Semaphore::new(2));
        *cell = Arc::downgrade(&s);
        s
    });
    sem.try_acquire_owned()
        .map(Some)
        .map_err(|_| ApiError::new(429, "当前设备已有两个媒体请求，请先暂停其他播放"))
}

async fn stream_authorized(a: &HarnessAuthorization, last: &mut Instant) -> bool {
    use std::sync::atomic::Ordering;
    let c = &a.controller;
    if c.closing.load(Ordering::Acquire) {
        return false;
    }
    if a.device.key_id == crate::desktop::DEVICE_KEY_ID {
        return a.check().await.is_ok();
    }
    if !c.running.load(Ordering::Acquire)
        || !c
            .access_policy
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .enabled
    {
        return false;
    }
    {
        let auth = c.auth.lock().await;
        if !auth.devices.iter().any(|d| {
            d.id == a.device.id
                && d.key_id == a.device.key_id
                && d.token_hash == a.device.token_hash
                && d.expires_at_ms > crate::now_ms()
        }) || auth
            .access_key
            .as_ref()
            .is_none_or(|key| key.id != a.device.key_id)
        {
            return false;
        }
    }
    // 热路径每块查内存中的设备/服务状态，外部密钥存储最多每秒复核一次，避免整部视频反复占用配置磁盘和授权锁。
    if last.elapsed() >= Duration::from_secs(1) {
        *last = Instant::now();
        return a.check().await.is_ok();
    }
    true
}

// 仅暴露类型已核验的音视频/位图，资源标识不含凭据，不能代替当前设备和原项目授权。
pub(crate) async fn content(
    State(state): State<WebState>,
    AxumPath(id): AxumPath<String>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let device = crate::authorized(&state, &headers).await?;
    let auth = state.controller.authorization(device, None);
    let lease = lookup(&state, &auth, &id)?;
    let (root, path) =
        tokio::time::timeout(REQUEST_TIMEOUT, resolve(&state, &lease.identity, &auth))
            .await
            .map_err(|_| ApiError::new(504, "文件身份核对超时"))??;
    if root != lease.root || path != lease.path {
        return Err(changed());
    }
    let expected = lease.version.clone();
    let (file, size) = tokio::task::spawn_blocking(move || {
        let f = open_local(&root, &path)?.ok_or_else(|| ApiError::new(404, "原文件不存在"))?;
        if version(&f)? != expected {
            return Err(changed());
        }
        let size = f.metadata().map_err(io_error)?.len();
        Ok::<_, ApiError>((f, size))
    })
    .await
    .map_err(|_| ApiError::new(503, "无法打开媒体"))??;
    let etag = format!("\"{}\"", lease.version);
    let mut bounds = None;
    if method != Method::HEAD
        && let Some(range) = headers.get(header::RANGE)
        && headers
            .get(header::IF_RANGE)
            .is_none_or(|v| v == etag.as_str())
    {
        bounds = range
            .to_str()
            .ok()
            .and_then(|v| crate::video::byte_range(v, size));
        if bounds.is_none() {
            return Ok((
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{size}"))],
            )
                .into_response());
        }
    }
    let (start, len) = bounds.map_or((0, size), |(a, b)| (a, b - a + 1));
    auth.check().await.map_err(crate::authorization_error)?;
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        let permit = slot(&state, &lease)?;
        let original_stamp = stamp(&file.metadata().map_err(io_error)?);
        let mut file = tokio::fs::File::from_std(file);
        file.seek(SeekFrom::Start(start)).await.map_err(io_error)?;
        // 每块 64 KiB、两条远程响应；每批次复核授权，不让撤权/停服前已建立的长响应继续读盘。
        Body::from_stream(futures_util::stream::try_unfold(
            (file, len, auth, permit, Instant::now(), original_stamp),
            |(mut f, left, a, p, mut checked, expected)| async move {
                if left == 0 {
                    return Ok::<_, std::io::Error>(None);
                }
                let mut buf = vec![0; left.min(64 * 1024) as usize];
                f.read_exact(&mut buf).await?;
                if stamp(&f.metadata().await?) != expected {
                    return Err(std::io::Error::other("源文件已变化"));
                }
                if !stream_authorized(&a, &mut checked).await {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "预览授权已失效",
                    ));
                }
                let next = left - buf.len() as u64;
                Ok(Some((buf, (f, next, a, p, checked, expected))))
            },
        ))
    };
    let mut res = Response::new(body);
    *res.status_mut() = if bounds.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let h = res.headers_mut();
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&lease.mime).map_err(|_| changed())?,
    );
    h.insert(
        header::ETAG,
        HeaderValue::from_str(&etag).map_err(|_| changed())?,
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("inline"),
    );
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        "cross-origin-resource-policy",
        HeaderValue::from_static("same-origin"),
    );
    if let Some((a, b)) = bounds {
        h.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {a}-{b}/{size}")).map_err(|_| changed())?,
        );
    }
    Ok(res)
}
