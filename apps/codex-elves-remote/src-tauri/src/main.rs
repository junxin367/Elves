#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    if let Ok(executable) = std::env::current_exe() {
        codex_elves_core::remote_access::set_service_executable(executable);
    }
    // 用户要求只安装独立客户端：后台及升级退出 helper 由同一个 EXE 的专用模式承载。
    if std::env::args()
        .any(|arg| arg == "--prepare-remote-shutdown" || arg == "--remote-access-service")
    {
        let shutdown = std::env::args().any(|arg| arg == "--prepare-remote-shutdown");
        let result = tokio::runtime::Runtime::new()
            .map_err(anyhow::Error::from)
            .and_then(|runtime| {
                runtime.block_on(async {
                    if shutdown {
                        codex_elves_core::remote_access::prepare_service_shutdown("installer")
                            .await?;
                    } else {
                        #[cfg(windows)]
                        {
                            let root = codex_elves_remote::bundled_tools::prepare(
                                &codex_elves_core::paths::default_app_state_dir(),
                            )?;
                            codex_elves_core::tool_runtime::register_bundled_root(root);
                        }
                        codex_elves_manager_lib::register_management();
                        codex_elves_core::remote_access::run_service().await?;
                    }
                    Ok(())
                })
            });
        if let Err(error) = result {
            let _ = codex_elves_core::diagnostic_log::append_diagnostic_log(
                "remote_access.service_failed",
                serde_json::json!({"message":format!("{error:#}")}),
            );
            std::process::exit(1);
        }
        return;
    }
    #[cfg(windows)]
    if let Err(error) = codex_elves_remote::desktop::run() {
        codex_elves_remote::desktop::show_startup_error(&error.to_string());
        std::process::exit(1);
    }

    #[cfg(not(windows))]
    eprintln!("CodexElves Remote currently supports Windows.");
}
