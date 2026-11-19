# 构建性能

日常开发使用 `.\scripts\build-windows-client.ps1`。该入口检查 Harness 网页、准备共享管理页面，再用 `release-fast` 增量编译客户端并生成便携程序，不构建安装包或 Android。

完整优化的客户端使用 `.\scripts\build-windows-client.ps1 -Profile release`。用户明确需要安装包时运行 `.\scripts\build-windows-installer.ps1`，默认同样使用 `release`；`-Profile release-fast` 仅供显式选择的本机调试包。需要缩短本地安装包压缩等待时，可追加 `-FastCompression`，该模式改用 NSIS zlib，安装包体积可能增大。

## 编译与资源处理

- `release` 保持 `opt-level=3`，并开启增量编译与 64 个代码生成单元，缩短本机和持续集成中的重复正式编译；代码生成并行度提高会略微减少跨单元优化机会。`release-fast` 再将五个工作区 crate 的 `opt-level` 调整为 `1`，并使用 256 个代码生成单元，适合本机快速验收。第三方依赖及其构建脚本和过程宏沿用各 profile 的原配置，不通过通配覆盖改变其优化参数。
- 安装包默认使用固实 LZMA；`-FastCompression` 只将本地包改用 zlib，不改变客户端编译 profile、安装文件或运行时行为，适合本地反复验收，正式发布仍使用默认压缩。
- 网页只在不依赖业务代码的 `elves-assets` 中内嵌一次，Harness 和客户端共用资源表。资源通过非内联函数访问，减少网页字节变化向大型业务 crate 的编译传播。
- 资源构建脚本在运行时读取当前包目录，并使用相对路径监视资源变化；生成的资源表内容不变时不重写文件。网页仍全部内嵌，不增加运行时文件依赖。
- JS 语法和导出链接检查共用一个 Node 子进程，源码通过标准输入传递；继续检查分层、循环依赖、预加载及各端扫描规则，且不执行页面代码。

首次应用配置、更换工作区或缺少缓存时，项目自身需要重新编译；下面的增量耗时不代表首次构建时间。

## 共享管理前端缓存

`npm --prefix apps/elves-management run build:frontend` 按内容核对项目源码和配置、依赖锁定信息、Node 版本、平台、`NODE_ENV` / `VITE_` 环境及全部生成文件。输入和产物均相同才复用；不依赖文件时间戳。

缓存位于该前端的 `node_modules/.cache/elves/`。缓存不存在或损坏、产物缺失或被修改时重新构建；构建失败不记录成功缓存。构建期间输入发生变化也不记录本次结果。新增业务目录同样参与摘要，只有 `node_modules` 和 Git 元数据被排除。

管理页源码、配置与依赖锁定文件纳入 Git；`assets/harness/management/` 是被忽略的生成目录，HTML 和哈希资源均不提交。Windows 构建入口和 CI 在 Cargo 前准备前端。直接使用 Cargo 的开发者须先运行上述前端构建；`elves-assets` 会检查管理页入口及其脚本、样式引用，缺失时给出生成命令并终止，避免把不完整资源嵌入程序。

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

## 2026-11-18 快速安装包实测

本轮基线为同一工作区此前的正式 `release` 安装包构建，快速结果使用 `-Profile release-fast -FastCompression`。两次均复用本机依赖缓存；完整命令墙钟时间取 PowerShell 进程实际运行时长。

| 阶段 | 正式 `release` 基线 | 快速本地包 | 变化 |
| --- | ---: | ---: | ---: |
| Harness 检查 | 未单独计时 | 0.86 s | 新增分阶段计时 |
| 管理前端 | 2.17 s | 0.43 s（缓存命中） | 节省 1.74 s，快 80.2% |
| Rust 客户端 | 177.66 s | 128.91 s（`release-fast`） | 节省 48.75 s，快 27.4% |
| 完整命令墙钟 | 225.10 s | 198.76 s | 节省 26.35 s，快 11.7% |
| NSIS | 旧流程未单独计时 | 64.30 s（非固实 LZMA） | 无直接基线 |

上一轮非固实 LZMA 快速包安装程序为 18,593,424 bytes，客户端为 81,886,208 bytes；体积同时受 `release-fast` 优化等级影响，不能把差异全部归因于 NSIS 压缩方式。正式发布仍使用默认 `release` 和固实 LZMA。

同一 `release-fast` 缓存改用 zlib 再次构建时，Harness 检查为 `0.89s`、前端缓存命中为 `0.44s`、Rust 编译为 `20.40s`、NSIS 为 `19.01s`、完整命令为 `44.76s`。最终快速安装包为 `28,034,053 bytes`；相较非固实 LZMA，zlib 牺牲体积换取更短的打包等待。该 `44.76s` 是热缓存结果，不能与前面的冷 Rust 构建总耗时直接相减。

## 2026-11-19 正式 release 增量 profile 验证

本轮将正式 `release` 保持在 `opt-level=3`，开启 `incremental` 并将 `codegen-units` 设为 `64`。首次切换 profile 会重新建立 release 依赖缓存，冷构建耗时 `516s`，不作为优化收益；缓存建立后，无源码变化的重复构建为 `18.27s`，Harness 单文件注释变更后的 release 增量构建为 `129s`。临时探针已移除，未进入源码和产物。

该配置主要缩短本机和持续集成中的重复 release 构建；干净机器首次构建仍受完整优化和依赖编译影响。正式安装包继续使用同一 `release` profile，不切换到 `release-fast`。

相关检查入口：

```powershell
npm --prefix apps/elves-management run check
cargo fmt --all -- --check
cargo test --locked -p elves-harness -p elves resource
cargo build --locked --profile release-fast -p elves --bin elves --timings
node scripts/verify-windows-build-inputs.mjs . target/release-fast
node scripts/verify-tool-runtime.mjs . --embedded target/release-fast/elves.exe
```
