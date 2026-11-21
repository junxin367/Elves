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
                        // 安装与卸载共用这个入口，两者都一律放行；卸载只改提示文字（任务同样在后台执行完，
                        // 但卸载后不会再有 Elves 接上）。NSIS 卸载时传 --shutdown-mode=uninstall，旧版忽略该参数。
                        let reason =
                            if std::env::args().any(|arg| arg == "--shutdown-mode=uninstall") {
                                "uninstall"
                            } else {
                                "installer"
                            };
                        let response =
                            elves_harness::prepare_service_shutdown_status(reason).await?;
                        // 安装器需要看到真实的关停边界；不能把 stdio handoff、旧服务版本、
                        // active 会话和未知状态都折叠成“Codex 正在执行”。
                        report_installer_shutdown(&response);
                        match response["status"].as_str() {
                            Some("stopped" | "not_running") => {}
                            Some("busy") => {
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
            if shutdown {
                installer_output(&format!("Elves 关停检查失败：{error:#}"));
            }
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

/// 用户要求（2026-11-21）：安装器只做一次关停检查，不再轮询 150 次，并把 Harness 返回的状态、每条说明
/// 和完整日志位置直接写进安装日志。新版 Harness 一律放行（第二次确认）：执行中的 Codex 留在私有服务里
/// 继续，没有执行中的会话时 Codex 一并关闭；等待审批、随退出中断和已保存待续的工作逐条列出。
/// busy 只会来自尚未升级的旧版 Harness。NSIS 详情列表会截断长行，所以每条说明单独一行。
/// 第三次确认：安装程序自己关闭 Elves，不论这里的结论如何都不再中止安装（见 Elves.nsi `PrepareShutdown`）；
/// 后台端点失效时列出的剩余进程来自 helper 自己的检查，对应诊断事件 `harness.shutdown_not_running`。
fn report_installer_shutdown(response: &serde_json::Value) {
    let status = response["status"].as_str().unwrap_or("unknown");
    let summary = match status {
        "stopped" if response["codex"] == "continuing" => {
            "Harness 已停止，执行中的 Codex 任务留在后台继续".to_owned()
        }
        "stopped" if response["message"].is_string() => {
            "Harness 已停止，Codex 已一并关闭".to_owned()
        }
        "stopped" => "已确认空闲，Harness 与 Codex 已一并关闭".to_owned(),
        "not_running" => "Elves 后台未运行".to_owned(),
        "busy" => format!(
            "正在运行的旧版 Elves 未停止（{}，Codex 连接：{}）",
            response["reason"].as_str().unwrap_or("busy"),
            response["transport"].as_str().unwrap_or("未知")
        ),
        other => format!("未确认（{other}）"),
    };
    installer_output(&format!("Elves 关停检查：{summary}"));
    for line in response["message"].as_str().unwrap_or("").lines() {
        if !line.trim().is_empty() {
            installer_output(line);
        }
    }
    let event = match status {
        "stopped" if response["message"].is_string() => "harness.shutdown_confirmed",
        "not_running" if response["message"].is_string() => "harness.shutdown_not_running",
        "stopped" | "not_running" => return,
        _ => "harness.shutdown_blocked",
    };
    installer_output(&format!(
        "完整记录：{}（事件 {event}）",
        elves_core::diagnostic_log::diagnostic_log_path().display()
    ));
}

/// NSIS 的 nsExec::ExecToLog 按系统 ANSI 代码页解码子进程输出，直接写 UTF-8 会出现
/// “鍏冲仠璇婃柇”这类乱码；输出到管道时先转换为 CP_ACP，在终端运行时保持 UTF-8。
fn installer_output(line: &str) {
    use std::io::{IsTerminal, Write};
    let mut stderr = std::io::stderr();
    #[cfg(windows)]
    if !stderr.is_terminal() {
        let mut bytes = ansi_code_page_bytes(line);
        bytes.extend_from_slice(b"\r\n");
        let _ = stderr.write_all(&bytes);
        let _ = stderr.flush();
        return;
    }
    let _ = writeln!(stderr, "{line}");
}

#[cfg(windows)]
fn ansi_code_page_bytes(text: &str) -> Vec<u8> {
    use windows_sys::Win32::Globalization::{CP_ACP, WideCharToMultiByte};
    let wide: Vec<u16> = text.encode_utf16().collect();
    let Ok(wide_len) = i32::try_from(wide.len()) else {
        return text.as_bytes().to_vec();
    };
    if wide_len == 0 {
        return Vec::new();
    }
    // 第一次调用只取所需长度，第二次写入；任一步失败都退回原始 UTF-8，至少保留可复制的内容。
    let size = unsafe {
        WideCharToMultiByte(
            CP_ACP,
            0,
            wide.as_ptr(),
            wide_len,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    if size <= 0 {
        return text.as_bytes().to_vec();
    }
    let mut bytes = vec![0u8; size as usize];
    let written = unsafe {
        WideCharToMultiByte(
            CP_ACP,
            0,
            wide.as_ptr(),
            wide_len,
            bytes.as_mut_ptr(),
            size,
            std::ptr::null(),
            std::ptr::null_mut(),
        )
    };
    if written <= 0 {
        return text.as_bytes().to_vec();
    }
    bytes.truncate(written as usize);
    bytes
}
