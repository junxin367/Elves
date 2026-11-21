mod commands;

use anyhow::Context;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::fs;
use std::sync::OnceLock;
use tauri::Manager;

static COMMAND_SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

fn command_serial() -> &'static tokio::sync::Mutex<()> {
    COMMAND_SERIAL.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn revision() -> anyhow::Result<String> {
    let path = elves_core::paths::default_settings_path();
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    let settings = elves_core::settings::SettingsStore::default().load()?;
    let home = elves_core::codex_home::codex_home_dir_for_settings(&settings);
    let mut hash = sha2::Sha256::new();
    use sha2::Digest;
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
    for name in ["config.toml", "auth.json"] {
        let contents = match fs::read(home.join(name)) {
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

fn is_write(command: &str) -> bool {
    matches!(
        command,
        "save_settings"
            | "save_relay_file"
            | "switch_relay_profile"
            | "import_ccs_providers"
            | "probe_relay_profile_responses_websocket"
            | "delete_local_session"
    )
}

#[tauri::command]
async fn manager_command(command: String, payload: Value) -> Result<Value, String> {
    let _serial = if is_write(&command) || command == "load_settings" {
        Some(command_serial().lock().await)
    } else {
        None
    };
    let before = revision().map_err(|error| format!("读取配置版本失败：{error:#}"))?;
    if is_write(&command) && payload["revision"].as_str() != Some(before.as_str()) {
        return Err("配置已由其他客户端修改，请刷新后合并本次草稿。".to_string());
    }
    let args = &payload["args"];
    let result = match command.as_str() {
        "load_settings" => serde_json::to_value(commands::load_settings()),
        "settings_revision" => Ok(json!({"status":"ok"})),
        "save_settings" => {
            let settings = argument(args, "settings").map_err(|error| error.to_string())?;
            serde_json::to_value(commands::save_settings(settings).await)
        }
        "read_relay_files" => serde_json::to_value(commands::read_relay_files()),
        "load_ccs_providers" => serde_json::to_value(commands::load_ccs_providers()),
        "import_ccs_providers" => serde_json::to_value(commands::import_ccs_providers()),
        "list_local_sessions" => serde_json::to_value(commands::list_local_sessions()),
        "delete_local_session" => {
            let request = argument(args, "request").map_err(|error| error.to_string())?;
            serde_json::to_value(commands::delete_local_session(request))
        }
        "check_env_conflicts" => serde_json::to_value(commands::check_env_conflicts()),
        "remove_env_conflicts" => {
            let request = argument(args, "request").map_err(|error| error.to_string())?;
            serde_json::to_value(commands::remove_env_conflicts(request))
        }
        "save_relay_file" => {
            let request = argument(args, "request").map_err(|error| error.to_string())?;
            serde_json::to_value(commands::save_relay_file(request))
        }
        "switch_relay_profile" => {
            let request = argument(args, "request").map_err(|error| error.to_string())?;
            serde_json::to_value(commands::switch_relay_profile(request).await)
        }
        "local_proxy_status" => serde_json::to_value(commands::local_proxy_status().await),
        "read_local_proxy_logs" => {
            let request = argument(args, "request").map_err(|error| error.to_string())?;
            serde_json::to_value(commands::read_local_proxy_logs(request).await)
        }
        "read_local_proxy_log_detail" => {
            let request = argument(args, "request").map_err(|error| error.to_string())?;
            serde_json::to_value(commands::read_local_proxy_log_detail(request).await)
        }
        "clear_local_proxy_logs" => serde_json::to_value(commands::clear_local_proxy_logs().await),
        "test_relay_profile" => {
            let profile = argument(args, "profile").map_err(|error| error.to_string())?;
            serde_json::to_value(
                commands::test_relay_profile(
                    profile,
                    args["model"].as_str().map(str::to_owned),
                    args["speedTest"].as_bool(),
                )
                .await,
            )
        }
        "fetch_relay_profile_models" => {
            let profile = argument(args, "profile").map_err(|error| error.to_string())?;
            serde_json::to_value(commands::fetch_relay_profile_models(profile).await)
        }
        "probe_relay_profile_responses_websocket" => {
            let profile = argument(args, "profile").map_err(|error| error.to_string())?;
            serde_json::to_value(
                commands::probe_relay_profile_responses_websocket(profile).await,
            )
        }
        _ => Err(serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("不支持的管理命令：{command}"),
        ))),
    }
    .map_err(|error| error.to_string())?;
    let mut result = result;
    if result.is_object() {
        result["revision"] = json!(
            revision().map_err(|error| format!("读取配置版本失败：{error:#}"))?
        );
    }
    Ok(result)
}

#[tauri::command]
async fn ensure_proxy_service() -> Result<Value, String> {
    let executable =
        std::env::current_exe().map_err(|error| format!("读取管理器路径失败：{error}"))?;
    match elves_core::local_proxy_service::ensure_service_process(&executable)
        .await
        .map_err(|error| format!("{error:#}"))?
    {
        elves_core::local_proxy_service::EnsureServiceResult::Disabled => {
            Ok(json!({"status":"disabled","managed":false}))
        }
        elves_core::local_proxy_service::EnsureServiceResult::AlreadyRunning => {
            Ok(json!({"status":"running","managed":false}))
        }
        elves_core::local_proxy_service::EnsureServiceResult::Started { process_id } => {
            Ok(json!({"status":"started","managed":true,"processId":process_id}))
        }
    }
}

pub fn run_proxy_service() -> anyhow::Result<()> {
    elves_core::local_proxy_service::run_service_process()
}

pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![manager_command, ensure_proxy_service])
        .setup(|app| {
            // Tauri 配置已声明 main 窗口；启动钩子只能复用它，不能再次创建同一 label。
            let window = app
                .get_webview_window("main")
                .ok_or(tauri::Error::WindowNotFound)?;
            window.show()?;
            window.set_focus()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Elves 代理管理器启动失败");
}
