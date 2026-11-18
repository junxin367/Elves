# Elves 产品规范索引

设计、实现、审查或验收界面前，先读根目录 [DESIGN.md](../../DESIGN.md) 的通用规则，再读下表中的“产品行为与交互规范”及对应专题。

| 归档 | 文档 | 维护内容 |
| --- | --- | --- |
| `core/` | [产品行为与交互规范](core/product-behavior.md) | 产品默认值、共享设置、导航、菜单、跨端行为及明确例外 |
| `sessions/` | [远程会话接入与操作指南](sessions/remote-access-guide.md) | 远程启用、连接与授权，以及会话操作步骤 |
| `sessions/` | [工作区工具与关联会话](sessions/workspace-tools-and-side-sessions.md) | 文件、变更、子智能体、侧边会话和右侧标签面板 |
| `sessions/` | [会话展示缓存与恢复](sessions/display-cache-and-recovery.md) | 缓存范围、有效期、阅读位置及失效处理 |
| `clients/` | [Windows 本机工作区](clients/windows-workspace.md) | 本机启动、权限边界、原生窗口、托盘和构建入口 |
| `clients/` | [手机网页、PWA 与 Android](clients/mobile-web-pwa-android.md) | 手机接入、整页操作、平台差异与连接排查 |
| `frontend/` | [工作区前端架构与公共组件](frontend/workspace-architecture-and-components.md) | 前端分层、Select 与弹层复用、资源登记及检查入口 |

**维护口径：** Elves 已确认的产品规则和例外以 `core/product-behavior.md` 为准；专题文档保留操作步骤、模块细节和实现约束，遇到已有通用条款时链接到所属章节。专题文档或 [历史验收记录](../validation/) 与主规范不一致时，不用旧记录推定现行行为，也不把尚未验收的源码描述成已上线。
