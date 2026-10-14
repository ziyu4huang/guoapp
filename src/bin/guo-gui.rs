//! guo-gui — 红果短剧原生 GUI（egui）。复用 `guo` 核心库的全部逻辑。

// Windows 上不挂控制台窗口（其他平台忽略此属性）。
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

/// mimalloc 全域分配器:egui 官方文檔實測即時模式 UI ~20% 提速
#[global_allocator]
static GLOBAL_ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() {
    guo::gui::run();
}
