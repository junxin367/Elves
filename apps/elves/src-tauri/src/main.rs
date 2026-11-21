#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    if std::env::args().any(|arg| arg == elves_core::local_proxy_service::PROXY_SERVICE_ARG) {
        if let Err(error) = elves_core::local_proxy_service::run_service_process() {
            eprintln!("独立本地代理服务启动失败：{error:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Ok(executable) = std::env::current_exe() {
        elves_harness::set_service_executable(executable);
    }
    // 用户要求只安装独立客户端：后台及升级退出 helper 由同一个 EXE 的专用模式承载。
    if std::env::args().any(|arg| arg == "--prepare-shutdown" || arg == "--harness-service") {
        let shutdown = std::env::args().any(|arg| arg == "--prepare-shutdown");
        let result = tokio::runtime::Runtime::new()
            .map_err(anyhow::Error::from)
            .and_then(|runtime| {
                runtime.block_on(async {
                    if shutdown {
                        let response =
                            elves_harness::prepare_service_shutdown_status("installer").await?;
                        match response["status"].as_str() {
                            Some("stopped" | "not_running") => {}
                            Some("busy") => {
                                // 安装器可等待明确的执行中状态；未知、旧服务和连接错误不进入重试。
                                let _ = elves_core::diagnostic_log::append_diagnostic_log(
                                    "harness.installer_shutdown_busy",
                                    serde_json::json!({
                                        "status":"busy",
                                        "message":response["message"]
                                    }),
                                );
                                std::process::exit(elves_harness::SERVICE_SHUTDOWN_BUSY_EXIT_CODE);
                            }
                            _ => {
                                let message = response["message"]
                                    .as_str()
                                    .unwrap_or("远程后台未确认安全退出，请等待当前任务完成后重试");
                                anyhow::bail!("{message}");
                            }
                        }
                    } else {
                        #[cfg(windows)]
                        {
                            let root = elves::bundled_tools::prepare(
                                &elves_core::paths::default_app_state_dir(),
                            )?;
                            elves_core::tool_runtime::register_bundled_root(root);
                        }
                        elves_harness::run_service().await?;
                    }
                    Ok(())
                })
            });
        if let Err(error) = result {
            let _ = elves_core::diagnostic_log::append_diagnostic_log(
                "harness.service_failed",
                serde_json::json!({"message":format!("{error:#}")}),
            );
            std::process::exit(1);
        }
        return;
    }
    #[cfg(windows)]
    {
        // 用户要求增加“随 Elves 启动”策略：代理先作为独立子进程启动，
        // 不嵌入 Harness，也不依赖管理器窗口存活；关闭策略时由共享设置直接禁止拉起。
        if let Ok(executable) = std::env::current_exe()
            && let Ok(runtime) = tokio::runtime::Runtime::new()
            && let Err(error) = runtime.block_on(
                elves_core::local_proxy_service::ensure_service_process(&executable),
            )
        {
            let _ = elves_core::diagnostic_log::append_diagnostic_log(
                "elves.local_proxy_start_failed",
                serde_json::json!({"message":format!("{error:#}")}),
            );
        }
    }
    #[cfg(windows)]
    if let Err(error) = elves::desktop::run() {
        elves::desktop::show_startup_error(&error.to_string());
        std::process::exit(1);
    }

    #[cfg(not(windows))]
    eprintln!("Elves currently supports Windows.");
}
