#!/usr/bin/env python3
"""Claude Agent SDK 传输的 WSL 内常驻宿主：让执行中的 Claude 任务不受 Harness 退出和安装影响。

用户要求：退出 Elves 或运行安装程序时，正在执行的会话继续执行，不被打断；没有执行中的会话则一并关闭。
Claude CLI 的 stream-json 模式把标准输入输出绑在启动它的进程上，Harness 一退出管道就断；所以由本脚本
在 WSL 内独立成会话（setsid）持有 CLI，并通过 unix 套接字把同一条 stream-json 通道转交给当前连接的 Harness。

两种运行方式（同一个文件）：
- 连接端（默认）：由 Harness 经 wsl.exe 启动，标准输入输出就是 Harness 的通道。第一行必须是
  {"type":"host_attach","spec":{...}}；宿主不存在就先拉起，然后在标准输入输出与套接字之间透传。
- 宿主（--serve）：持有 CLI 子进程；只接受一个连接，断线后按“有无工作”决定继续或收尾。

宿主不解释对话内容，只跟踪判断“是否有工作”所需的结构化帧，并在 Harness 重新连接时补发仍然有效的状态
（初始化、会话状态、后台任务、尚未回答的审批/问答），CLI 在断线期间卡住的请求不会丢失。
"""
import argparse
import fcntl
import json
import os
import re
import signal
import socket
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path

MAX_LINE = 64 * 1024 * 1024
# 无人连接且没有任何工作时，宿主等待多久再收尾；覆盖 Harness 重启或安装后立刻重新连接的间隙。
IDLE_GRACE = 3.0
# 近期仍有 CLI 输出就视为有工作；老版本 CLI 不发会话状态帧时靠它避免误停。
ACTIVITY_WINDOW = 5.0
# 只剩未回答的审批/问答且无人连接时，最长保留多久；用户长时间不回来就收尾，不无限占用进程。
BLOCKED_LIMIT = 12 * 3600
STOP_TIMEOUT = 30.0
# 宿主向 Harness 写数据最多等待多久：Harness 不再读取时据此丢弃连接，不让 CLI 的输出一直被卡住。
# 只限制发送方向；读取方向不能有超时（见 limit_sending）。
CLIENT_SEND_TIMEOUT = 60.0
SESSION_PATTERN = re.compile(r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}")


def runtime_dir():
    folder = Path(f"/tmp/elves-claude-sdk-{os.getuid()}")
    folder.mkdir(mode=0o700, exist_ok=True)
    info = folder.lstat()
    if folder.is_symlink() or info.st_uid != os.getuid() or info.st_mode & 0o077:
        raise RuntimeError("宿主目录权限不安全")
    return folder


def socket_path(session):
    # 套接字路径有长度上限，用会话 UUID 的前 16 位十六进制即可区分同一用户下的会话。
    return str(runtime_dir() / (session.replace("-", "")[:16] + ".sock"))


def emit(connection, value):
    connection.sendall(json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode() + b"\n")


def limit_sending(connection):
    """只限制发送方向的等待时间，读取保持阻塞。

    曾经用 connection.settimeout(60) 同时限制了两个方向：Harness 在等用户批准审批、或任务长时间没有输出而
    一直没有写入时，读取 60 秒就超时，被当成断线，宿主丢掉连接；网页随即显示会话空闲、审批请求消失，CLI 却仍在
    等待回答（真实联调里计划确认等待超过一分钟时复现）。发送方向用 SO_SNDTIMEO 限制，对端卡住不读时仍会丢弃连接。"""
    connection.settimeout(None)
    seconds = float(CLIENT_SEND_TIMEOUT)
    connection.setsockopt(socket.SOL_SOCKET, socket.SO_SNDTIMEO,
                          struct.pack("ll", int(seconds), int(seconds % 1 * 1_000_000)))


class Host:
    def __init__(self, session, spec):
        self.session, self.spec = session, spec
        self.write_lock = threading.Lock()
        self.stdin_lock = threading.Lock()
        self.client = None
        self.cli = None
        self.failure = None
        self.stopping = False
        self.stop_at = None
        self.last = {}
        self.started = {}
        self.requests = {}
        self.state = "idle"
        self.compacting = False
        self.flight = set()
        self.activity = 0.0
        self.detached_at = time.monotonic()
        self.stderr_tail = []
        self.done = threading.Event()

    # ------------------------------------------------------------------ CLI

    def start_cli(self):
        argv, cwd = self.spec.get("argv"), self.spec.get("cwd")
        if not (isinstance(argv, list) and argv and all(isinstance(item, str) for item in argv)
                and isinstance(cwd, str) and cwd.startswith("/")):
            raise RuntimeError("宿主启动参数无效")
        env = dict(os.environ)
        extra = self.spec.get("env") or {}
        if isinstance(extra, dict):
            env.update({str(key): str(value) for key, value in extra.items()})
        self.cli = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                    stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        threading.Thread(target=self.pump_stdout, daemon=True).start()
        threading.Thread(target=self.pump_stderr, daemon=True).start()

    def write_cli(self, data):
        with self.stdin_lock:
            if self.cli is None or self.cli.stdin is None or self.cli.stdin.closed:
                return False
            try:
                self.cli.stdin.write(data)
                self.cli.stdin.flush()
                return True
            except (OSError, ValueError):
                return False

    def close_cli_input(self):
        with self.stdin_lock:
            if self.cli is not None and self.cli.stdin is not None and not self.cli.stdin.closed:
                try:
                    self.cli.stdin.close()
                except OSError:
                    pass

    def stop(self):
        """优雅收尾：关闭 CLI 标准输入，CLI 完成手头的一轮后自行退出。"""
        if not self.stopping:
            self.stopping = True
            self.stop_at = time.monotonic()
        self.close_cli_input()

    def pump_stdout(self):
        stream = self.cli.stdout
        while True:
            raw = stream.readline(MAX_LINE + 1)
            if not raw:
                break
            if len(raw) > MAX_LINE:
                self.cli.kill()
                break
            self.activity = time.monotonic()
            try:
                frame = json.loads(raw)
            except ValueError:
                frame = None
            if isinstance(frame, dict):
                self.observe(frame)
            self.forward(raw if raw.endswith(b"\n") else raw + b"\n")

    def pump_stderr(self):
        for raw in self.cli.stderr:
            line = raw.decode("utf-8", "replace").rstrip()
            self.stderr_tail = (self.stderr_tail + [line])[-40:]
            self.forward_value({"type": "host_stderr", "line": line[:2000]})

    def observe(self, frame):
        kind, sub = frame.get("type"), frame.get("subtype")
        if kind == "system":
            if sub == "init":
                self.last["init"] = frame
            elif sub == "session_state_changed":
                self.state = frame.get("state") or "idle"
                if self.state == "idle":
                    # 会话已空闲，不可能还有命令在进行；清掉可能因缺少终态帧而残留的记录。
                    self.flight.clear()
                self.last["state"] = frame
            elif sub == "status":
                self.compacting = frame.get("status") == "compacting"
                self.last["status"] = frame
            elif sub == "background_tasks_changed":
                self.last["tasks"] = frame
                live = {task.get("task_id") for task in frame.get("tasks") or []
                        if isinstance(task, dict) and task.get("ambient") is not True}
                self.started = {key: value for key, value in self.started.items() if key in live}
            elif sub == "task_started" and isinstance(frame.get("task_id"), str):
                self.started[frame["task_id"]] = frame
            elif sub == "task_notification":
                self.started.pop(frame.get("task_id"), None)
        elif kind == "command_lifecycle" and isinstance(frame.get("command_uuid"), str):
            if frame.get("state") in ("queued", "started"):
                self.flight.add(frame["command_uuid"])
            else:
                # queued、started 之外的状态都是终态：被打断的命令以 cancelled 结束（CLI 2.1.329 实测），
                # 只认 completed 会让它永远留在“进行中”，宿主就再也不会在空闲时收尾。
                self.flight.discard(frame["command_uuid"])
        elif kind == "control_request" and isinstance(frame.get("request_id"), str) \
                and (frame.get("request") or {}).get("subtype") == "can_use_tool":
            self.requests[frame["request_id"]] = frame
        elif kind == "control_cancel_request":
            self.requests.pop(frame.get("request_id"), None)

    def has_work(self):
        tasks = {task.get("task_id") for task in (self.last.get("tasks") or {}).get("tasks") or []
                 if isinstance(task, dict) and task.get("ambient") is not True}
        return bool(self.requests or self.flight or self.compacting or self.state != "idle"
                    or tasks or self.started
                    or time.monotonic() - self.activity < ACTIVITY_WINDOW)

    # ------------------------------------------------------------------ 客户端

    def forward(self, data):
        with self.write_lock:
            connection = self.client
            if connection is None:
                return
            try:
                connection.sendall(data)
            except OSError:
                self.drop_client(connection)

    def forward_value(self, value):
        self.forward(json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode() + b"\n")

    def drop_client(self, connection):
        if self.client is connection:
            self.client = None
            self.detached_at = time.monotonic()
        try:
            connection.close()
        except OSError:
            pass

    def attach(self, connection):
        with self.write_lock:
            if self.client is not None:
                busy = True
            else:
                busy = False
                self.client = connection
        if busy:
            try:
                emit(connection, {"type": "host_error", "code": "busy",
                                  "message": "该 Claude 会话已被另一个 Elves 连接占用"})
            finally:
                connection.close()
            return
        limit_sending(connection)
        try:
            if self.failure is not None or self.cli is None or self.cli.poll() is not None:
                code = None if self.cli is None else self.cli.poll()
                emit(connection, {"type": "host_exit", "code": code, "error": self.failure or "",
                                  "stderr": "\n".join(self.stderr_tail)[-4000:]})
                self.drop_client(connection)
                return
            with self.write_lock:
                emit(connection, {"type": "host_hello", "pid": self.cli.pid, "state": self.state,
                                  "tasks": len(self.started), "requests": len(self.requests)})
                # 补发仍然有效的状态；CLI 在断线期间发出的未决审批也一并重放。
                for name in ("init", "state", "status", "tasks"):
                    if name in self.last:
                        emit(connection, self.last[name])
                for frame in list(self.started.values()):
                    emit(connection, frame)
                for frame in list(self.requests.values()):
                    emit(connection, frame)
        except OSError:
            self.drop_client(connection)
            return
        threading.Thread(target=self.pump_client, args=(connection,), daemon=True).start()

    def pump_client(self, connection):
        try:
            stream = connection.makefile("rb")
            while True:
                raw = stream.readline(MAX_LINE + 1)
                if not raw or len(raw) > MAX_LINE:
                    break
                try:
                    frame = json.loads(raw)
                except ValueError:
                    frame = None
                if isinstance(frame, dict):
                    kind = frame.get("type")
                    if kind == "host_stop":
                        self.stop()
                        continue
                    if isinstance(kind, str) and kind.startswith("host_"):
                        continue
                    if kind == "control_response":
                        self.requests.pop((frame.get("response") or {}).get("request_id"), None)
                if not raw.endswith(b"\n"):
                    raw += b"\n"
                if not self.write_cli(raw):
                    break
        except (OSError, ValueError):
            pass
        finally:
            with self.write_lock:
                self.drop_client(connection)

    def accept_loop(self, listener):
        while not self.done.is_set():
            try:
                connection, _ = listener.accept()
            except OSError:
                return
            threading.Thread(target=self.attach, args=(connection,), daemon=True).start()

    # ------------------------------------------------------------------ 主循环

    def supervise(self):
        while True:
            time.sleep(0.25)
            code = None if self.cli is None else self.cli.poll()
            if self.failure is not None and self.client is None \
                    and time.monotonic() - self.detached_at > 15:
                return
            if self.failure is not None and self.client is not None:
                time.sleep(1)
                return
            if self.cli is not None and code is not None:
                if self.client is not None:
                    self.forward_value({"type": "host_exit", "code": code, "error": "",
                                        "stderr": "\n".join(self.stderr_tail)[-4000:]})
                    time.sleep(0.3)
                return
            if self.stopping:
                if time.monotonic() - self.stop_at > STOP_TIMEOUT and self.cli is not None:
                    self.cli.terminate()
                continue
            if self.client is not None:
                continue
            away = time.monotonic() - self.detached_at
            if not self.has_work():
                if away >= IDLE_GRACE:
                    self.stop()
            elif self.state == "requires_action" and away >= BLOCKED_LIMIT:
                self.stop()

    def run(self):
        path = socket_path(self.session)
        lock = open(path + ".lock", "w")
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError:
            return
        try:
            try:
                os.unlink(path)
            except FileNotFoundError:
                pass
            listener = socket.socket(socket.AF_UNIX)
            listener.bind(path)
            os.chmod(path, 0o600)
            listener.listen(4)
            threading.Thread(target=self.accept_loop, args=(listener,), daemon=True).start()
            try:
                self.start_cli()
            except (OSError, RuntimeError, ValueError) as failure:
                self.failure = str(failure)
            self.detached_at = time.monotonic()
            self.supervise()
        finally:
            self.done.set()
            try:
                os.unlink(path)
            except FileNotFoundError:
                pass
            if self.cli is not None and self.cli.poll() is None:
                self.cli.terminate()


def serve(session):
    spec = json.loads(sys.stdin.readline())
    # 宿主独立于启动它的连接端：忽略挂断信号，收到终止信号时优雅收尾。
    signal.signal(signal.SIGHUP, signal.SIG_IGN)
    host = Host(session, spec)
    signal.signal(signal.SIGTERM, lambda *_: host.stop())
    host.run()


def start_host(session, spec):
    child = subprocess.Popen([sys.executable, os.path.abspath(__file__), "--serve", "--session", session],
                             stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                             start_new_session=True, close_fds=True)
    child.stdin.write(json.dumps(spec).encode() + b"\n")
    child.stdin.close()


def read_first_line():
    """直接从文件描述符读第一行，并把同一次读取里多出来的字节留给转发线程。

    不能用带缓冲的 sys.stdin.buffer.readline：Harness 在 host_attach 之后立刻写入的第一帧（initialize）会与它
    一起进入缓冲，而转发线程用 os.read(0) 直读，缓冲里的帧就丢了。真实 WSL 联调时 initialize 因此无人应答。"""
    data = bytearray()
    while b"\n" not in data:
        chunk = os.read(0, 65536)
        if not chunk:
            break
        data += chunk
        if b"\n" not in data and len(data) > MAX_LINE:
            raise RuntimeError("宿主连接请求过长")
    line, _, rest = bytes(data).partition(b"\n")
    return line, rest


def connect(session):
    line, rest = read_first_line()
    attach = json.loads(line)
    if attach.get("type") != "host_attach" or not isinstance(attach.get("spec"), dict):
        raise RuntimeError("缺少宿主连接请求")
    path, started = socket_path(session), False
    attach_only = attach["spec"].get("attach_only") is True
    deadline = time.monotonic() + 20
    while True:
        connection = socket.socket(socket.AF_UNIX)
        try:
            connection.connect(path)
            break
        except (FileNotFoundError, ConnectionRefusedError):
            connection.close()
            if attach_only:
                # 只接入已有宿主：没有就如实报告“不存在”，不启动新的 CLI。
                sys.stdout.buffer.write(json.dumps({"type": "host_exit", "code": None, "error": "none",
                                                    "stderr": ""}).encode() + b"\n")
                sys.stdout.buffer.flush()
                return
            if time.monotonic() > deadline:
                raise RuntimeError("Claude 宿主未能启动")
            if not started:
                start_host(session, attach["spec"])
                started = True
            time.sleep(0.15)

    def upstream():
        try:
            if rest:
                # 与 host_attach 同一次读到的后续帧，先于之后读到的内容转交。
                connection.sendall(rest)
            while True:
                chunk = os.read(0, 65536)
                if not chunk:
                    break
                connection.sendall(chunk)
        except OSError:
            pass
        finally:
            try:
                connection.shutdown(socket.SHUT_WR)
            except OSError:
                pass

    threading.Thread(target=upstream, daemon=True).start()
    try:
        while True:
            data = connection.recv(65536)
            if not data:
                break
            view = memoryview(data)
            while view:
                view = view[os.write(1, view):]
    except OSError:
        pass


def main():
    arguments = argparse.ArgumentParser()
    arguments.add_argument("--session", required=True)
    arguments.add_argument("--serve", action="store_true")
    options = arguments.parse_args()
    if not SESSION_PATTERN.fullmatch(options.session):
        raise SystemExit("会话标识无效")
    if options.serve:
        serve(options.session)
    else:
        connect(options.session)


if __name__ == "__main__":
    main()
