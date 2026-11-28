# Elves 文档

本索引列出当前 Elves Windows 客户端、独立 Harness 和可选远程会话的使用与开发说明。Elves 启动直接进入本机工作区；网页、PWA 和 Android 通过 Harness 的远程会话能力接入。

## 使用指南

| 文档 | 内容 |
| --- | --- |
| [项目首页](../README.md) | 产品能力、安装、快速开始与源码构建 |
| [Windows 本机工作区](product-standards/clients/windows-workspace.md) | 本机启动、窗口、托盘及客户端数据 |
| [手机连接](product-standards/clients/mobile-web-pwa-android.md) | 电脑端准备、网页/PWA/Android 接入及连接恢复 |
| [远程会话接入与操作](product-standards/sessions/remote-access-guide.md) | 网页服务、设备授权及会话操作 |
| [公网入口](remote-public-access.md) | zrok / FRP 主入口、ngrok 第二线路及地址切换 |
| [工作区工具与关联会话](product-standards/sessions/workspace-tools-and-side-sessions.md) | 文件、变更、子智能体、侧边会话与平台差异 |
| [会话历史、展示缓存与恢复](product-standards/sessions/display-cache-and-recovery.md) | 历史分页、正文缓存、阅读位置及失效处理 |
| [Windows 更新](remote-service-update.md) | Elves 与内置 Harness 的升级、验证和回退 |

## 产品与架构

- [产品规范索引](product-standards/README.md)：按主规范、会话、客户端与前端实现查阅；通用 UI 规则在根目录 [DESIGN.md](../DESIGN.md)。
- [Elves 与 Harness 架构](harness-architecture.md)：代码边界、启动、本地代理和本机/远程隔离。
- [工作区前端架构与公共组件](product-standards/frontend/workspace-architecture-and-components.md)：模块分层、Select 与弹层、共享管理前端及网页资源构建。
- [工作区文件查看规划：本机与远程](workspace-file-preview-plan.md)：文本、音视频、图片、PDF、Office 和 ZIP 的读取边界及验收要求；文件面板不提供原文件下载。
- [构建性能](build-performance.md)：日常与正式编译配置、前端内容缓存及实测方法。
- [Harness Codex 执行](remote-service-mode.md)：独立 app-server、权限、回执及原生执行边界。
- [协议代理压缩](context-compaction.md)、[web_search](protocol-proxy-web-search.md)：协议代理的对应行为与兼容规则。

## Android

- [使用与构建](android.md)、[正式发布](android-release.md)。
- [应用更新](android-update.md)、[网页资源交付](pwa-android-implementation.md)。
- [通知](android-notifications.md)、[诊断日志](android-diagnostics.md)。

Android 版本、正式签名及 APK 发布独立维护，普通 Windows 编译不附带 Android 构建。

## 历史记录

[验收记录](validation/)与[发布审查记录](release/)保留对应日期和提交的事实、证据及未覆盖范围。记录中的旧程序名、目录、界面和构建命令只代表当时版本，不作为当前操作指南，也不表示当前版本已经安装或发布。已退役展示素材的历史引用固定到原提交，避免与当前界面混淆。

- [工作区大文件预览：文本方案与 Codex App 对照（2026-11-28）](validation/workspace-large-file-preview-2026-11-28.md)
- [PWA / Android 历史检查（2026-10-16）](validation/pwa-android-audit-2026-10-16.md)
- [DeepSeek 历史回放故障复盘（2026-09-17）](validation/deepseek-reasoning-replay-20260917.md)
- [上下文压缩改造前评估与实测（2026-08-03 至 2026-08-05）](validation/context-compaction-20260803-20260805.md)
- [远程会话 E2E 旧用例与取证](validation/remote-access-e2e-legacy-evidence.md)
