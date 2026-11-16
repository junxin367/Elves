use std::fs::File;
use std::net::{TcpListener, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use fs2::FileExt;

pub fn select_platform_loopback_port(requested: u16) -> u16 {
    select_platform_loopback_port_with(
        requested,
        cfg!(windows),
        can_bind_loopback_port,
        find_available_loopback_port,
    )
}

pub fn select_platform_loopback_port_with(
    requested: u16,
    is_windows: bool,
    can_bind: impl Fn(u16) -> bool,
    find_available: impl Fn() -> u16,
) -> u16 {
    if !is_windows || can_bind(requested) {
        requested
    } else {
        find_available()
    }
}

pub fn can_bind_loopback_port(port: u16) -> bool {
    if port == 0 {
        return true;
    }
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

pub fn find_available_loopback_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .unwrap_or(0)
}

pub fn can_connect_loopback_port(port: u16) -> bool {
    ("127.0.0.1", port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addresses| addresses.next())
        .and_then(|address| {
            std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(200))
                .ok()
        })
        .is_some()
}

pub async fn loopback_listener_is_running(port: u16) -> bool {
    for attempt in 0..3 {
        #[cfg(windows)]
        let listener_state = tokio::task::spawn_blocking(move || {
            let listeners = crate::windows_integration::loopback_tcp_listeners().ok()?;
            let owners = listeners
                .iter()
                .filter(|listener| listener.port == port)
                .map(|listener| listener.process_id)
                .collect::<Vec<_>>();
            if owners.is_empty() {
                return Some(false);
            }
            let processes = crate::windows_integration::enumerate_processes()
                .into_iter()
                .map(|process| process.process_id)
                .collect::<Vec<_>>();
            listener_owner_state(&owners, &processes)
        })
        .await
        .ok()
        .flatten();
        #[cfg(not(windows))]
        let listener_state: Option<bool> = None;

        match listener_state {
            Some(true) => return true,
            Some(false) => {}
            None => {
                if matches!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        tokio::net::TcpStream::connect(("127.0.0.1", port)),
                    )
                    .await,
                    Ok(Ok(_))
                ) {
                    return true;
                }
            }
        }
        if attempt < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    false
}

#[cfg(any(windows, test))]
fn listener_owner_state(owners: &[u32], processes: &[u32]) -> Option<bool> {
    if owners.is_empty() {
        Some(false)
    } else if processes.is_empty() {
        None
    } else {
        Some(owners.iter().any(|owner| processes.contains(owner)))
    }
}

fn guard_listener_is_active(port: u16) -> bool {
    if !can_connect_loopback_port(port) {
        return false;
    }
    #[cfg(windows)]
    if let Ok(listeners) = crate::windows_integration::loopback_tcp_listeners() {
        let owners = listeners
            .iter()
            .filter(|listener| listener.port == port)
            .map(|listener| listener.process_id)
            .collect::<Vec<_>>();
        let processes = crate::windows_integration::enumerate_processes()
            .into_iter()
            .map(|process| process.process_id)
            .collect::<Vec<_>>();
        if listener_owners_are_gone(&owners, &processes) {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "port_guard.stale_listener",
                serde_json::json!({"port": port, "owner_process_ids": owners}),
            );
            return false;
        }
    }
    true
}

#[cfg(any(windows, test))]
fn listener_owners_are_gone(owners: &[u32], processes: &[u32]) -> bool {
    !owners.is_empty()
        && !processes.is_empty()
        && owners.iter().all(|owner| !processes.contains(owner))
}

pub fn acquire_loopback_port_guard(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(("127.0.0.1", port))
}

#[derive(Debug)]
pub struct LoopbackPortGuard {
    _lock_file: Option<File>,
    lock_path: Option<PathBuf>,
    _listener: Option<Arc<TcpListener>>,
    using_fallback_lock: bool,
}

impl LoopbackPortGuard {
    pub fn listener(listener: TcpListener) -> Self {
        Self {
            _lock_file: None,
            lock_path: None,
            _listener: Some(Arc::new(listener)),
            using_fallback_lock: false,
        }
    }

    fn locked_listener(file: File, path: PathBuf, listener: TcpListener) -> Self {
        Self {
            _lock_file: Some(file),
            lock_path: Some(path),
            _listener: Some(Arc::new(listener)),
            using_fallback_lock: false,
        }
    }

    fn fallback_lock(file: File, path: PathBuf) -> Self {
        Self {
            _lock_file: Some(file),
            lock_path: Some(path),
            _listener: None,
            using_fallback_lock: true,
        }
    }

    pub fn fallback_path(&self) -> Option<&Path> {
        self.using_fallback_lock
            .then_some(())
            .and_then(|_| self.lock_path.as_deref())
    }

    pub fn shared_listener(&self) -> Option<Arc<TcpListener>> {
        // Windows 的复制 socket 可能可继承；唤醒线程必须共享原有监听器。
        self._listener.as_ref().map(Arc::clone)
    }
}

pub fn acquire_resilient_loopback_port_guard(port: u16) -> std::io::Result<LoopbackPortGuard> {
    acquire_resilient_loopback_port_guard_at(port, &crate::paths::default_app_state_dir())
}

fn acquire_resilient_loopback_port_guard_at(
    port: u16,
    state_dir: &Path,
) -> std::io::Result<LoopbackPortGuard> {
    acquire_resilient_loopback_port_guard_with(
        port,
        state_dir,
        acquire_loopback_port_guard,
        guard_listener_is_active,
    )
}

fn acquire_resilient_loopback_port_guard_with(
    port: u16,
    state_dir: &Path,
    bind: impl Fn(u16) -> std::io::Result<TcpListener>,
    can_connect: impl Fn(u16) -> bool,
) -> std::io::Result<LoopbackPortGuard> {
    if port == 0 {
        return bind(port).map(LoopbackPortGuard::listener);
    }

    let (file, path) = acquire_lock_guard(port, state_dir)?;
    match bind(port) {
        Ok(listener) => Ok(LoopbackPortGuard::locked_listener(file, path, listener)),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse && can_connect(port) => {
            Err(error)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            Ok(LoopbackPortGuard::fallback_lock(file, path))
        }
        Err(error) => Err(error),
    }
}

fn acquire_lock_guard(port: u16, state_dir: &Path) -> std::io::Result<(File, PathBuf)> {
    let dir = state_dir.join("locks");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("loopback-port-{port}.lock"));
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    file.try_lock_exclusive().map_err(normalize_lock_error)?;
    Ok((file, path))
}

fn normalize_lock_error(error: std::io::Error) -> std::io::Error {
    match error.raw_os_error() {
        Some(33) => std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "loopback port guard lock is already held",
        ),
        _ => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listener_owner_state_distinguishes_exit_from_unavailable_process_snapshot() {
        assert_eq!(listener_owner_state(&[42], &[42, 100]), Some(true));
        assert_eq!(listener_owner_state(&[42], &[100]), Some(false));
        assert_eq!(listener_owner_state(&[42], &[]), None);
        assert_eq!(listener_owner_state(&[], &[42]), Some(false));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn listener_status_stays_running_when_the_accept_queue_is_full() {
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        socket.bind(([127, 0, 0, 1], 0).into()).unwrap();
        let listener = socket.listen(1).unwrap();
        let address = listener.local_addr().unwrap();
        let mut queued = Vec::new();
        let mut probe_failed = false;
        for _ in 0..8 {
            match std::net::TcpStream::connect_timeout(
                &address,
                std::time::Duration::from_millis(200),
            ) {
                Ok(stream) => queued.push(stream),
                Err(_) => {
                    probe_failed = true;
                    break;
                }
            }
        }

        assert!(probe_failed, "接入队列应已占满，旧 TCP 探测必须失败");
        assert!(loopback_listener_is_running(address.port()).await);
        drop(queued);
        drop(listener);
        assert!(!loopback_listener_is_running(address.port()).await);
    }

    #[tokio::test]
    async fn listener_status_retries_a_transiently_missing_listener() {
        let port = find_available_loopback_port();
        let start_listener = async {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            TcpListener::bind(("127.0.0.1", port)).unwrap()
        };
        let (running, listener) = tokio::join!(loopback_listener_is_running(port), start_listener);
        assert!(running);
        drop(listener);
        assert!(!loopback_listener_is_running(port).await);
    }

    #[test]
    fn resilient_guard_holds_lock_and_listener_when_requested_port_is_available() {
        let temp = tempfile::tempdir().unwrap();
        let port = find_available_loopback_port();

        let guard = acquire_resilient_loopback_port_guard_at(port, temp.path()).unwrap();

        assert!(guard.lock_path.is_some());
        assert!(guard._listener.is_some());
        assert!(guard.fallback_path().is_none());
    }

    #[test]
    fn resilient_guard_reports_lock_conflict_when_instance_lock_is_held() {
        let temp = tempfile::tempdir().unwrap();
        let port = find_available_loopback_port();
        let _guard = acquire_resilient_loopback_port_guard_at(port, temp.path()).unwrap();

        let second = acquire_resilient_loopback_port_guard_at(port, temp.path()).unwrap_err();

        assert_eq!(second.kind(), std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn resilient_guard_reports_conflict_when_requested_port_is_connectable() {
        let temp = tempfile::tempdir().unwrap();
        let error = acquire_resilient_loopback_port_guard_with(
            45219,
            temp.path(),
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "port busy",
                ))
            },
            |_| true,
        )
        .unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AddrInUse);
    }

    #[test]
    fn listener_owner_check_requires_complete_evidence_of_exit() {
        assert!(listener_owners_are_gone(&[17328], &[100, 200]));
        assert!(!listener_owners_are_gone(&[17328], &[100, 17328]));
        assert!(!listener_owners_are_gone(&[17328, 21980], &[21980]));
        assert!(!listener_owners_are_gone(&[], &[100, 200]));
        assert!(!listener_owners_are_gone(&[17328], &[]));
    }

    #[test]
    fn resilient_guard_ignores_exited_listener_owner_but_keeps_instance_lock() {
        let temp = tempfile::tempdir().unwrap();
        let acquire = || {
            acquire_resilient_loopback_port_guard_with(
                45219,
                temp.path(),
                |_| {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::AddrInUse,
                        "stale listener still accepts TCP connections",
                    ))
                },
                |_| !listener_owners_are_gone(&[17328], &[100, 200]),
            )
        };
        let guard = acquire().unwrap();

        assert!(guard.fallback_path().is_some());
        assert_eq!(
            acquire().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );

        drop(guard);
        assert!(acquire().unwrap().fallback_path().is_some());
    }

    #[test]
    fn resilient_guard_uses_lock_fallback_when_requested_port_is_not_connectable() {
        let temp = tempfile::tempdir().unwrap();
        let guard = acquire_resilient_loopback_port_guard_with(
            45219,
            temp.path(),
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "stale port",
                ))
            },
            |_| false,
        )
        .unwrap();

        assert!(guard._listener.is_none());
        assert!(guard.fallback_path().is_some());

        let second = acquire_resilient_loopback_port_guard_with(
            45219,
            temp.path(),
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    "stale port",
                ))
            },
            |_| false,
        )
        .unwrap_err();
        assert_eq!(second.kind(), std::io::ErrorKind::WouldBlock);
    }
}
