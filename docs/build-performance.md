# 构建性能

日常开发使用 `.\scripts\build-windows-client.ps1`。该入口检查 Harness 网页、准备共享管理页面，再用 `release-fast` 增量编译客户端并生成便携程序，不构建安装包或 Android。

完整优化的客户端使用 `.\scripts\build-windows-client.ps1 -Profile release`。用户明确需要安装包时运行 `.\scripts\build-windows-installer.ps1`，默认同样使用 `release`；`-Profile release-fast` 仅供显式选择的本机调试包。

## 编译与资源处理

- `release-fast` 只将五个工作区 crate 的 `opt-level` 调整为 `1`，保留增量编译及 256 个代码生成单元。第三方依赖及其构建脚本和过程宏沿用原来的配置，不通过通配覆盖改变其优化参数。`release` 保持完整优化。
- 网页只在不依赖业务代码的 `elves-assets` 中内嵌一次，Harness 和客户端共用资源表。资源通过非内联函数访问，减少网页字节变化向大型业务 crate 的编译传播。
- 资源构建脚本在运行时读取当前包目录，并使用相对路径监视资源变化；生成的资源表内容不变时不重写文件。网页仍全部内嵌，不增加运行时文件依赖。
- JS 语法和导出链接检查共用一个 Node 子进程，源码通过标准输入传递；继续检查分层、循环依赖、预加载及各端扫描规则，且不执行页面代码。

首次应用配置、更换工作区或缺少缓存时，项目自身需要重新编译；下面的增量耗时不代表首次构建时间。

## 共享管理前端缓存

`npm --prefix apps/elves-management run build:frontend` 按内容核对项目源码和配置、依赖锁定信息、Node 版本、平台、`NODE_ENV` / `VITE_` 环境及全部生成文件。输入和产物均相同才复用；不依赖文件时间戳。

缓存位于该前端的 `node_modules/.cache/elves/`。缓存不存在或损坏、产物缺失或被修改时重新构建；构建失败不记录成功缓存。构建期间输入发生变化也不记录本次结果。新增业务目录同样参与摘要，只有 `node_modules` 和 Git 元数据被排除。

强制重建：

```powershell
npm --prefix apps/elves-management run build:frontend -- --force
```

原有 `npm --prefix apps/elves-management run vite:build` 保留完整 Vite 构建及其参数支持。Windows 构建入口始终执行 Harness 检查；安装流程继续验证客户端内嵌的网页、工具和许可证。

## 2026-11-17 实测

环境为本机 Windows x64、Cargo / rustc 1.95.0、Node 22.22.2、Vite 6.4.2。基线为 `c8738476` 的原构建配置；优化后使用同一提交加本需求补丁。因为主工作区存在其他进行中的设置与 UI 改动，优化后的编译在独立检出中验证，复用本机 Cargo 依赖缓存。

| 场景 | 优化前 | 优化后 |
| --- | ---: | ---: |
| 网页文件变动后的客户端增量编译 | 42.911 s | 11.533 s |
| 无源码变动的 Cargo 构建 | 0.944 s | 0.960 s |
| 148 个资源、139 个 JS 文件的检查 | 8.908 s | 1.107 s |
| 管理前端没有变化时 | 2.862 s（完整 Vite 构建） | 0.623 s（内容缓存核对） |
| Harness Rust 文件变动后的增量编译 | 未建立单独对照 | 11.421 s |

计时使用 PowerShell Stopwatch，覆盖对应 Cargo 或 npm 命令。源码探针是在网页入口或 Harness Rust 文件末尾添加无行为变化的注释；每个场景恢复源码后重新构建，再进入下一场景。编译测量结束后逐项核对程序包含的 148 个网页资源和构建标识，等待 Cargo 锁的样本不计入结果。

这是单次热缓存验证，不能推算干净构建、实际 Rust 函数大改或完整安装包的提速比例。Rust 原始探针受前序网页恢复影响，未作为独立对照使用。

验证覆盖批量解析对语法错误、缺失导出、缺失模块、静态环的拦截及不执行模块；前端缓存覆盖输入变化、产物变动或缺失、缓存损坏、强制重建、失败不写缓存和失败后的恢复，共 17 项。

另通过 11 项现有 Rust 回归，覆盖资源版本与清单、资源和 API 授权边界、客户端本机凭据、摘要校验、离线与原子更新。共享管理前端检查、Rust 格式检查及程序内嵌网页与工具字节核对均通过。

相关检查入口：

```powershell
npm --prefix apps/elves-management run check
cargo fmt --all -- --check
cargo test --locked -p elves-harness -p elves resource
cargo build --locked --profile release-fast -p elves --bin elves --timings
node scripts/verify-windows-build-inputs.mjs . target/release-fast
node scripts/verify-tool-runtime.mjs . --embedded target/release-fast/elves.exe
```
