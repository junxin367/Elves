//! 原管理器迁移后的共享后台。仅供独立远程服务登记管理命令，不创建窗口或启动桌面管理器。
pub mod commands;

use anyhow::Context;
use codex_elves_core::remote_access::{ManagementConflict, ManagementFuture, RemoteAuthorization};
use codex_elves_core::settings::SettingsStore;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::OnceLock;

pub fn register_management() {
    codex_elves_core::remote_access::register_management_handler(dispatch);
}

fn serial() -> &'static tokio::sync::Mutex<()> {
    static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    SERIAL.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn revision() -> anyhow::Result<String> {
    let path = codex_elves_core::paths::default_settings_path();
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let mut hash = Sha256::new();
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
    let settings = SettingsStore::default().load()?;
    let home = codex_elves_core::codex_home::codex_home_dir_for_settings(&settings);
    for name in ["config.toml", "auth.json"] {
        let contents = match std::fs::read(home.join(name)) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        hash.update((contents.len() as u64).to_le_bytes());
        hash.update(contents);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn argument<T: DeserializeOwned>(args: &Value, key: &str) -> anyhow::Result<T> {
    serde_json::from_value(
        args.get(key)
            .cloned()
            .with_context(|| format!("缺少参数：{key}"))?,
    )
    .with_context(|| format!("参数无效：{key}"))
}

fn open_frp_directory() -> anyhow::Result<Value> {
    let directory = codex_elves_core::remote_access::frp_program_directory(
        &codex_elves_core::paths::default_app_state_dir(),
    );
    std::fs::create_dir_all(&directory)?;
    #[cfg(windows)]
    {
        // 此入口只打开服务自身的固定程序目录，不接受网页传入本机路径，也不调用管理器或 Codex App。
        std::process::Command::new("explorer.exe")
            .arg(&directory)
            .spawn()
            .context("无法在服务端打开 frp 程序目录")?;
        Ok(json!({"status":"ok","message":"已在服务端打开 frp 程序目录。"}))
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!(
            "当前服务不提供桌面文件管理器，请在服务端打开 {}",
            directory.display()
        )
    }
}

fn dispatch(
    command: String,
    payload: Value,
    authorization: RemoteAuthorization,
) -> ManagementFuture {
    Box::pin(async move {
        let writes_settings = matches!(
            command.as_str(),
            "save_settings"
                | "save_relay_file"
                | "switch_relay_profile"
                | "import_ccs_providers"
                | "probe_relay_profile_responses_websocket"
        );
        let _serial = if writes_settings || command == "load_settings" {
            Some(serial().lock().await)
        } else {
            None
        };
        authorization.check().await?;
        // 同一服务多端共用配置；旧草稿不能带着其他客户端的新版本覆盖服务器。
        let before = revision()?;
        if writes_settings && payload["revision"].as_str() != Some(before.as_str()) {
            return Err(ManagementConflict(
                "配置已由其他客户端修改，请刷新后合并本次草稿。".into(),
            )
            .into());
        }
        let args = &payload["args"];
        macro_rules! value {
            ($result:expr) => {
                serde_json::to_value($result)?
            };
        }
        let mut result = match command.as_str() {
            "load_settings" => value!(commands::load_settings()),
            "settings_revision" => json!({"status":"ok"}),
            "save_settings" => value!(commands::save_settings(argument(args, "settings")?).await),
            "read_relay_files" => value!(commands::read_relay_files()),
            "load_ccs_providers" => value!(commands::load_ccs_providers()),
            "import_ccs_providers" => value!(commands::import_ccs_providers()),
            "list_local_sessions" => value!(commands::list_local_sessions()),
            "check_env_conflicts" => value!(commands::check_env_conflicts()),
            "remove_env_conflicts" => {
                value!(commands::remove_env_conflicts(argument(args, "request")?))
            }
            "save_relay_file" => value!(commands::save_relay_file(argument(args, "request")?)),
            "switch_relay_profile" => {
                value!(commands::switch_relay_profile(argument(args, "request")?).await)
            }
            "local_proxy_status" => value!(commands::local_proxy_status().await),
            "read_local_proxy_logs" => {
                value!(commands::read_local_proxy_logs(argument(args, "request")?).await)
            }
            "read_local_proxy_log_detail" => {
                value!(commands::read_local_proxy_log_detail(argument(args, "request")?).await)
            }
            "clear_local_proxy_logs" => value!(commands::clear_local_proxy_logs().await),
            "test_relay_profile" => value!(
                commands::test_relay_profile(
                    argument(args, "profile")?,
                    args["model"].as_str().map(str::to_owned),
                    args["speedTest"].as_bool()
                )
                .await
            ),
            "fetch_relay_profile_models" => {
                value!(commands::fetch_relay_profile_models(argument(args, "profile")?).await)
            }
            "probe_relay_profile_responses_websocket" => value!(
                commands::probe_relay_profile_responses_websocket(argument(args, "profile")?).await
            ),
            "remote_access_command" => {
                let action: String = argument(args, "action")?;
                let body = args.get("payload").cloned().unwrap_or_else(|| json!({}));
                match action.as_str() {
                    "openFrpDirectory" => open_frp_directory()?,
                    "credentials" | "setKey" | "generateKey" => {
                        codex_elves_core::remote_access::local_settings_command(&action, body)
                            .await?
                    }
                    "status" | "apply" | "revoke" | "diagnostics" | "downloadZrok"
                    | "registerZrok" | "startZrok" | "testZrok" | "saveNgrok" | "saveFrp"
                    | "testPrimaryRoute" | "applyPrimaryRoute" => {
                        codex_elves_core::remote_access::admin_request(&action, body).await
                    }
                    _ => anyhow::bail!("不支持的远程服务管理操作"),
                }
            }
            _ => anyhow::bail!("不支持的管理命令"),
        };
        if result.is_object() {
            let after = revision()?;
            result["revision"] = json!(after);
            if (writes_settings || command == "load_settings") && after != before {
                codex_elves_core::remote_access::publish_management_updated();
            }
        }
        Ok(result)
    })
}
