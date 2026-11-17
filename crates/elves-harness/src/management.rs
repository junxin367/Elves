//! 统一工作区的管理入口。只调用独立后台登记的固定命令，不复用管理器进程或 Codex App。
mod commands;
mod dispatch;
pub use dispatch::register_management;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex as StdMutex, OnceLock};

use super::*;

pub type ManagementFuture = Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>>;
pub type ManagementHandler = fn(String, Value, HarnessAuthorization) -> ManagementFuture;
static HANDLER: OnceLock<ManagementHandler> = OnceLock::new();
static LOCAL_PROOFS: OnceLock<StdMutex<HashMap<String, (PathBuf, u64)>>> = OnceLock::new();

pub(super) fn issue_local_proof(directory: &FsPath) -> Value {
    let proof = random_token();
    let mut proofs = LOCAL_PROOFS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    proofs.retain(|_, (_, expires)| *expires > now_ms());
    // Elves 本机启动只使用一次，30 秒后失效；不能通过公开管理 API 申请此证明。
    proofs.insert(hash(&proof), (directory.to_owned(), now_ms() + 30_000));
    json!({"status":"ok","proof":proof})
}

pub(super) fn consume_local_proof(
    state: &WebState,
    headers: &HeaderMap,
    trust: RequestTrustContext,
    peer: SocketAddr,
    payload: &Value,
) -> Result<bool, ApiError> {
    let Some(value) = payload.get("localProof") else {
        return Ok(false);
    };
    if !state.desktop
        || trust.external_public
        || !peer.ip().is_loopback()
        || headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            != Some(format!("127.0.0.1:{}", state.settings.port).as_str())
    {
        return Err(ApiError::new(
            403,
            "本机授权只能在 Elves 的 Harness 本机入口使用",
        ));
    }
    let proof = value
        .as_str()
        .filter(|value| value.len() <= 256)
        .ok_or_else(|| ApiError::new(400, "本机管理授权无效"))?;
    let record = LOCAL_PROOFS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(&hash(proof));
    if record.is_none_or(|(directory, expires)| {
        directory != state.controller.state_dir || expires <= now_ms()
    }) {
        return Err(ApiError::new(
            401,
            "本机管理授权已失效，请在 Elves 中重试启动",
        ));
    }
    Ok(true)
}

pub fn register_handler(handler: ManagementHandler) {
    let _ = HANDLER.set(handler);
}

pub(super) fn available() -> bool {
    HANDLER.get().is_some()
}

pub fn publish_updated() {
    if let Some(controller) = CONTROLLER.get() {
        controller.publish_event(json!({"method":"remote/managementChanged","params":{}}));
    }
}

pub(super) async fn command(
    State(state): State<WebState>,
    Path(command): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Extension(trust): Extension<RequestTrustContext>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    // 用户要求网页、PWA、Android 不开放任何管理能力；远程密码和设备授权不能提升为本机管理权限。
    if !state.desktop {
        return Err(ApiError::new(403, "管理功能仅可在 Elves 独立客户端使用"));
    }
    let device = authorized(&state, &headers).await?;
    // 本机身份来自原生启动的一次性证明；同时核对实际监听、回环对端、Host 和设备身份。
    let native_owner = state.desktop
        && !trust.external_public
        && peer.ip().is_loopback()
        && headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            == Some(format!("127.0.0.1:{}", state.settings.port).as_str())
        && device.key_id == desktop::DEVICE_KEY_ID;
    if !native_owner {
        return Err(ApiError::new(403, "管理功能仅可在 Elves 独立客户端使用"));
    }
    let authorization = state.controller.authorization(device, None);
    authorization.check().await.map_err(authorization_error)?;
    let handler = HANDLER
        .get()
        .ok_or_else(|| ApiError::new(503, "当前后台未提供统一管理功能，请更新远程服务"))?;
    let result = handler(command, payload, authorization)
        .await
        .map_err(|error| {
            if let Some(error) = error.downcast_ref::<ApiError>() {
                error.clone()
            } else if let Some(error) = error.downcast_ref::<ManagementConflict>() {
                ApiError::new(409, error.0.clone())
            } else {
                ApiError::new(400, format!("{error:#}"))
            }
        })?;
    Ok(Json(result))
}

#[derive(Debug)]
pub struct ManagementConflict(pub String);

impl std::fmt::Display for ManagementConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ManagementConflict {}
