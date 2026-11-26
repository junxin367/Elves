//! Elves 本机工作区的远程会话管理接口。
//! 用户要求「远程会话」配置保留在 Elves Harness 中，不放进独立代理管理器，
//! 且仅允许 Elves 本机客户端经由回环桌面入口访问，网页、PWA 与 Android 一律拒绝。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::OnceLock;

use anyhow::Context;
use axum::Json;
use axum::extract::{ConnectInfo, Extension, Path, State};
use axum::http::{HeaderMap, header};
use serde_json::{Value, json};

use super::{
    ApiError, RequestTrustContext, WebState, access_credentials_status, authorization_error,
    authorized, configure_access_key, desktop, frp_tunnel, ngrok_tunnel, random_token,
};
use crate::settings::{RemoteAccessSettings, SettingsStore};

fn settings_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn open_frp_directory(state_dir: &std::path::Path) -> anyhow::Result<Value> {
    let directory = crate::frp_program_directory(state_dir);
    std::fs::create_dir_all(&directory)?;
    #[cfg(windows)]
    {
        // 只打开服务端固定的 frp 目录，不接受前端传入任意路径。
        std::process::Command::new("explorer.exe")
            .arg(&directory)
            .spawn()
            .context("无法在服务端打开 frp 程序目录")?;
        Ok(json!({"status": "ok", "message": "已在服务端打开 frp 程序目录。"}))
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!(
            "当前服务不提供桌面文件管理器，请在服务端打开 {}",
            directory.display()
        )
    }
}

async fn read_credentials(state_dir: PathBuf) -> anyhow::Result<Value> {
    tokio::task::spawn_blocking(move || {
        let mut credentials = access_credentials_status(&state_dir)?;
        let (configured, token) = ngrok_tunnel::local_credentials(&state_dir);
        credentials["ngrokTokenConfigured"] = json!(configured);
        if let Some(token) = token {
            credentials["ngrokToken"] = json!(token);
        }
        let frp = frp_tunnel::Saved::load(&state_dir, false).unwrap_or_default();
        credentials["frpToken"] = json!(frp.token);
        Ok(credentials)
    })
    .await
    .context("无法读取本机远程访问凭据")?
}

async fn save_access_key(state: &WebState, key: String) -> anyhow::Result<Value> {
    let state_dir = state.controller.state_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        configure_access_key(&state_dir, &key)?;
        access_credentials_status(&state_dir)
    })
    .await
    .context("无法保存本机远程访问密钥")??;
    // 保存密钥后立即通知运行中的 Harness 重载，避免登录锁定状态或旧设备授权延迟失效。
    let _ = state.controller.reload_access_key().await;
    Ok(result)
}

async fn save_settings(state: &WebState, payload: Value) -> anyhow::Result<Value> {
    let _serial = settings_lock().lock().await;
    let settings_value = payload
        .get("settings")
        .cloned()
        .context("缺少远程会话设置")?;
    let next: RemoteAccessSettings =
        serde_json::from_value(settings_value).context("远程会话设置格式无效")?;
    next.validate()?;
    let apply = payload
        .get("apply")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let store = SettingsStore::new(state.controller.state_dir.join("settings.json"));
    let saved = tokio::task::spawn_blocking(move || store.update(json!({ "remoteAccess": next })))
        .await
        .context("保存远程会话设置任务异常")??;
    if !apply {
        return Ok(json!({
            "status": "ok",
            "settings": saved.remote_access,
        }));
    }
    state
        .controller
        .reconcile(saved.remote_access.clone())
        .await;
    Ok(state.controller.admin_status(&saved.remote_access).await)
}

pub(super) async fn handle(
    State(state): State<WebState>,
    Path(action): Path<String>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Extension(trust): Extension<RequestTrustContext>,
    headers: HeaderMap,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    // 网页、PWA、Android 不开放远程会话管理接口；远程密码和设备授权不能提升为本机管理权限。
    if !state.desktop {
        return Err(ApiError::new(403, "远程会话管理仅可在 Elves 本机客户端使用"));
    }
    let device = authorized(&state, &headers).await?;
    let native_owner = state.desktop
        && !trust.external_public
        && peer.ip().is_loopback()
        && headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            == Some(format!("127.0.0.1:{}", state.settings.port).as_str())
        && device.key_id == desktop::DEVICE_KEY_ID;
    if !native_owner {
        return Err(ApiError::new(403, "远程会话管理仅可在 Elves 本机客户端使用"));
    }
    state
        .controller
        .authorization(device, None)
        .check()
        .await
        .map_err(authorization_error)?;

    let outcome = match action.as_str() {
        "saveSettings" => save_settings(&state, payload).await,
        "credentials" => read_credentials(state.controller.state_dir.clone()).await,
        "setKey" => match payload["key"].as_str() {
            Some(key) => save_access_key(&state, key.to_owned()).await,
            None => Err(anyhow::anyhow!("缺少访问密钥")),
        },
        "generateKey" => Ok(json!({"status": "ok", "key": random_token()})),
        "openFrpDirectory" => open_frp_directory(&state.controller.state_dir),
        "status" | "apply" | "revoke" | "downloadZrok" | "registerZrok" | "startZrok"
        | "testZrok" | "saveNgrok" | "saveFrp" | "testPrimaryRoute" | "applyPrimaryRoute" => {
            state.controller.admin(&action, payload).await
        }
        _ => return Err(ApiError::new(404, "不支持的远程会话管理操作")),
    };

    let response = outcome
        .unwrap_or_else(|error| json!({"status": "failed", "message": format!("{error:#}")}));
    Ok(Json(response))
}
