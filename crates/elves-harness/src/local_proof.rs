//! Elves 本机工作区的一次性授权证明。
//! 该证明只用于建立本机工作区 Cookie，不承载代理或会话管理能力。
use std::sync::{Mutex as StdMutex, OnceLock};

use super::*;

static LOCAL_PROOFS: OnceLock<StdMutex<HashMap<String, (PathBuf, u64)>>> = OnceLock::new();

pub(super) fn issue_local_proof(directory: &FsPath) -> Value {
    let proof = random_token();
    let mut proofs = LOCAL_PROOFS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    proofs.retain(|_, (_, expires)| *expires > now_ms());
    // Elves 本机启动只使用一次，30 秒后失效；不能通过公开 HTTP 接口申请此证明。
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
        .ok_or_else(|| ApiError::new(400, "本机授权证明无效"))?;
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
            "本机授权已失效，请在 Elves 中重试启动",
        ));
    }
    Ok(true)
}
