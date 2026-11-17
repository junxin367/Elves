//! Elves 与 Harness 共用的内嵌网页资源，不包含执行、授权或管理逻辑。

pub type WebAsset = (&'static str, &'static str, &'static [u8]);

include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));

// 资源字节只由本 crate 持有；跨 crate 保留函数边界，避免网页内容改动传播为大型业务 crate 的常量变更。
#[inline(never)]
pub fn web_assets() -> &'static [WebAsset] {
    WEB_ASSETS
}

#[inline(never)]
pub fn resource_revision() -> u64 {
    include_str!("../../../assets/harness/resource-revision.txt")
        .trim()
        .parse()
        .expect("web resource revision should be a positive integer")
}

#[inline(never)]
pub fn cache_recovery_html() -> &'static [u8] {
    include_bytes!("../../../assets/harness/cache-recovery.html")
}

#[inline(never)]
pub fn cache_recovery_js() -> &'static [u8] {
    include_bytes!("../../../assets/harness/cache-recovery.js")
}
