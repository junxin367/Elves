//! 统一工作区的管理入口。只调用独立后台登记的固定命令，不复用管理器进程或 Codex App。
use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex as StdMutex, OnceLock};

use super::*;

pub type ManagementFuture = Pin<Box<dyn Future<Output = anyhow::Result<Value>> + Send>>;
pub type ManagementHandler = fn(String, Value, RemoteAuthorization) -> ManagementFuture;
static HANDLER: OnceLock<ManagementHandler> = OnceLock::new();
static LOCAL_PROOFS: OnceLock<StdMutex<HashMap<String, (PathBuf, u64)>>> = OnceLock::new();

#[derive(Serialize, Deserialize)]
struct LocalOwner {
    id: String,
    key_id: String,
    token_hash: String,
}

fn owners_path(directory: &FsPath) -> PathBuf {
    directory.join("remote-access-local-owners.json")
}

pub(super) fn issue_local_proof(directory: &FsPath) -> Value {
    let proof = random_token();
    let mut proofs = LOCAL_PROOFS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    proofs.retain(|_, (_, expires)| *expires > now_ms());
    // 本机内置连接页只使用一次，30 秒后失效；不能通过公开管理 API 申请此证明。
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
    if trust.external_public
        || !peer.ip().is_loopback()
        || headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            != Some(format!("127.0.0.1:{}", state.settings.port).as_str())
    {
        return Err(ApiError::new(403, "本机管理授权只能在本机连接页使用"));
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
        return Err(ApiError::new(401, "本机管理授权已失效，请重新连接本机服务"));
    }
    Ok(true)
}

pub(super) fn local_owner(directory: &FsPath, device: &Device) -> bool {
    let owners: Vec<LocalOwner> = read_json_or_default(&owners_path(directory)).unwrap_or_default();
    owners.iter().any(|owner| {
        owner.id == device.id
            && owner.key_id == device.key_id
            && constant_time_eq(&owner.token_hash, &device.token_hash)
    })
}

pub(super) fn remember_local_owner(
    directory: &FsPath,
    device: &Device,
    active: &[Device],
) -> anyhow::Result<()> {
    let mut owners: Vec<LocalOwner> = read_json_or_default(&owners_path(directory))?;
    owners.retain(|owner| {
        owner.id != device.id
            && active.iter().any(|current| {
                current.id == owner.id
                    && current.key_id == owner.key_id
                    && constant_time_eq(&current.token_hash, &owner.token_hash)
            })
    });
    owners.push(LocalOwner {
        id: device.id.clone(),
        key_id: device.key_id.clone(),
        token_hash: device.token_hash.clone(),
    });
    crate::settings::atomic_write(&owners_path(directory), &serde_json::to_vec(&owners)?)
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
    let device = authorized(&state, &headers).await?;
    // 代理配置和日志可能包含凭据；只读设备不能借管理页面取得供应商或访问密钥。
    // 本机管理授权来自内置连接页的一次性证明。仅回环地址可解除只读；普通网页及公网 Cookie 不享有例外。
    let native_owner = !trust.external_public
        && peer.ip().is_loopback()
        && headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            == Some(format!("127.0.0.1:{}", state.settings.port).as_str())
        && local_owner(&state.controller.state_dir, &device);
    let authorization = state.controller.authorization(
        device,
        if native_owner {
            None
        } else {
            Some("management")
        },
    );
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
