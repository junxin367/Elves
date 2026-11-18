# 参与 Elves 开发

Elves 是唯一的 Windows 桌面程序，Harness 是独立执行与管理产品，远程会话只是网页和 Android 的接入能力。开始修改前阅读 [AGENTS.md](AGENTS.md)，按 [产品规范索引](docs/product-standards/README.md) 查阅对应章节，并核对 [Elves 与 Harness 架构](docs/harness-architecture.md)。

## 开发准备

需要支持 Rust edition 2024 的工具链、Node.js/npm；Windows 安装包需要 NSIS 和 WebView2 Runtime。仓库地址为 `https://github.com/junxin367/Elves`。

```powershell
npm --prefix apps/elves-management ci
npm --prefix apps/elves-management run check
npm --prefix apps/elves-management run vite:build
cargo fmt --all -- --check
cargo check --workspace
```

先生成共享管理页面，再编译 Rust，避免 Vite 替换资源时与 Rust 内嵌资源编译竞争。工作区原生网页修改后递增 `assets/harness/resource-revision.txt`。

## 代码结构

- `apps/elves/`：Windows 窗口、本机工作区启动及原生文件能力。
- `crates/elves-harness/`：原生执行、会话、管理与可选远程入口。
- `crates/elves-core/`：配置、模型与协议代理。
- `crates/elves-data/`：SQLite、历史和供应商数据同步。
- `apps/elves-management/`：共享管理前端，构建到 `assets/harness/management/`。
- `apps/elves-android/`：Android 容器，版本及签名独立维护。

禁止恢复 Codex App 注入、CDP、renderer 脚本、桌面执行桥接、独立管理器或旧启动器。

## 验证与提交

```powershell
cargo test --workspace --exclude elves-harness
cargo test -p elves-harness --lib -- --test-threads=1
node --test scripts/harness-ui-state.test.mjs scripts/harness-e2e-evidence.test.mjs scripts/harness-e2e-environment.test.mjs scripts/harness-e2e-receipt-driver.test.mjs scripts/workspace-tabs.test.mjs scripts/windows-remote-cache.test.mjs scripts/windows-remote-titlebar.test.mjs
.\scripts\build-windows-installer.ps1
```

Harness 集成测试串行运行，以避免进程内授权缓存和隔离后台互相干扰。Claude 共享历史的 Python 回归需要 Linux/WSL 的 `fcntl`。普通 Windows 构建不附带 Android，不表示已安装或发布。

提交按仓库中文约定式格式编写，核对用户 Git 身份和暂存范围。一个需求单独提交；临时日志、截图及验收产生的目录不入库。共享管理前端变化时同时提交其生成资源。缺陷报告包含版本、平台、复现步骤和实际错误，分享前移除凭据和会话隐私。
