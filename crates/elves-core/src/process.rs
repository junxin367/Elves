//! 数据写入前的只读进程检查，不启动、关闭或注入 Codex App。
#[cfg(windows)]
pub fn process_id_is_running(id: u32) -> bool {
    crate::windows_integration::enumerate_processes()
        .iter()
        .any(|process| process.process_id == id)
}

#[cfg(not(windows))]
pub fn process_id_is_running(id: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &id.to_string()])
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(windows)]
pub fn find_codex_processes() -> Vec<u32> {
    crate::windows_integration::enumerate_processes()
        .into_iter()
        .filter(|process| {
            process.exe_file.eq_ignore_ascii_case("Codex.exe")
                || process.exe_file.eq_ignore_ascii_case("ChatGPT.exe")
        })
        .map(|process| process.process_id)
        .collect()
}

#[cfg(not(windows))]
pub fn find_codex_processes() -> Vec<u32> {
    Vec::new()
}
