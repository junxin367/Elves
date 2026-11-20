#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    if std::env::args().any(|arg| arg == "--proxy-service") {
        if let Err(error) = elves_manager_lib::run_proxy_service() {
            eprintln!("独立本地代理服务启动失败：{error:#}");
            std::process::exit(1);
        }
        return;
    }

    elves_manager_lib::run();
}
