//! Elves / Harness 构建身份。产品不再向 Codex App 提供任何页面注入。
// 版本交接能力变更时递增；安装器用它拒绝把不支持 handoff 的旧 Harness 当成可接管服务。
pub const DIAGNOSTIC_BUILD_ID: &str = "elves-harness-native-r2";
