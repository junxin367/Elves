# Elves 文档

本索引列出当前 Elves Windows 客户端、独立 Harness 和可选远程会话的使用与开发说明。Elves 启动直接进入本机工作区；网页、PWA 和 Android 通过 Harness 的远程会话能力接入。

## 使用指南

| 文档 | 内容 |
| --- | --- |
| [项目首页](../README.md) | 产品能力、安装、快速开始与源码构建 |
| [Windows 客户端](windows-remote-client.md) | 本机启动、窗口、托盘及客户端数据 |
| [手机连接](remote-access-mobile.md) | 电脑端准备、网页/PWA/Android 接入及连接恢复 |
| [远程会话](remote-access.md) | 网页服务、设备授权、会话能力及接口边界 |
| [公网入口](remote-public-access.md) | zrok / FRP 主入口、ngrok 第二线路及地址切换 |
| [会话工具](remote-session-tools.md) | 文件、变更、子智能体、侧边会话与平台差异 |
| [缓存与恢复](remote-cache.md) | 正文缓存、草稿、分页及断线恢复 |
| [Windows 更新](remote-service-update.md) | Elves 与内置 Harness 的升级、验证和回退 |

## 产品与架构

- [产品规范](product-standards.md)：已确认的功能、交互和跨端约束。
- [Elves 与 Harness 架构](harness-architecture.md)：代码边界、启动、本地代理和本机/远程隔离。
- [工作区网页架构](harness-web-architecture.md)：模块分层、共享管理前端及网页资源构建。
- [Harness Codex 执行](remote-service-mode.md)：独立 app-server、权限、回执及原生执行边界。
- [功能需求条目](harness-product-features.md)：按用户任务组织的功能与交互条目。
- [协议代理压缩](context-compaction.md)、[web_search](protocol-proxy-web-search.md)：协议代理的对应行为与兼容规则。

## Android

- [使用与构建](android.md)、[正式发布](android-release.md)。
- [应用更新](android-update.md)、[网页资源交付](pwa-android-implementation.md)。
- [通知](android-notifications.md)、[诊断日志](android-diagnostics.md)。

Android 版本、正式签名及 APK 发布独立维护，普通 Windows 编译不附带 Android 构建。

## 历史记录

[验收记录](validation/)与[发布审查记录](release/)保留对应日期和提交的事实、证据及未覆盖范围。记录中的旧程序名、目录、界面和构建命令只代表当时版本，不作为当前操作指南，也不表示当前版本已经安装或发布。已退役展示素材的历史引用固定到原提交，避免与当前界面混淆。
