#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
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
                        elves_harness::prepare_service_shutdown("installer").await?;
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
    if let Err(error) = elves::desktop::run() {
        elves::desktop::show_startup_error(&error.to_string());
        std::process::exit(1);
    }

    #[cfg(not(windows))]
    eprintln!("Elves currently supports Windows.");
}
