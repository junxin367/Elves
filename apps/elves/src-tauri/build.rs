fn main() {
    #[cfg(windows)]
    {
        // 用户要求唯一 Elves 程序直接打开本机工作区；客户端和后台沿用 asInvoker，不主动请求管理员权限。
        let windows = tauri_build::WindowsAttributes::new()
            .app_manifest(include_str!("windows-app-manifest.xml"));
        let attributes = tauri_build::Attributes::new()
            .windows_attributes(windows)
            .app_manifest(tauri_build::AppManifest::new().commands(&[
                "client_startup_state",
                "client_retry_startup",
                "client_recover_authentication",
                "client_titlebar_theme",
                "client_unread_badge",
                "client_cache_status",
                "client_cache_refresh",
                "client_cache_activity",
                "client_open_session_window",
                "client_choose_inputs",
                "client_paste_inputs",
                "client_project_navigation",
                "client_close_about",
            ]));
        tauri_build::try_build(attributes).expect("failed to build Elves");
    }
}
