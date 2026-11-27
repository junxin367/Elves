//! 后台命令构造与数据写入前的只读进程检查，不启动、关闭或注入 Codex App。

/// 用户要求非交互命令从创建时就不显示控制台；可见终端的 Open in 入口不能使用此构造器。
pub fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(crate::windows_create_hidden_process());
    }
    command
}

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
