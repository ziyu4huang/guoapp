//! guo-gui — 红果短剧原生 GUI（egui）。复用 `guo` 核心库的全部逻辑。

// Windows 上不挂控制台窗口（其他平台忽略此属性）。
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

fn main() {
    guo::gui::run();
}
