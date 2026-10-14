//! guo-gui — 红果短剧原生 GUI（egui）。复用 `guo` 核心库的全部逻辑。

// Windows 上不挂控制台窗口（其他平台忽略此属性）。
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

/// mimalloc 全域分配器:egui 官方文檔實測即時模式 UI ~20% 提速
#[global_allocator]
static GLOBAL_ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// panic 现场写入数据目录 panic.log(尽力而为,失败静默)——公开版用户
/// 不会填 issue,有现场日志才可诊断。
fn install_panic_hook() {
    let default_dir = || -> std::path::PathBuf {
        if let Ok(x) = std::env::var("XDG_DATA_HOME") {
            if !x.is_empty() {
                return std::path::PathBuf::from(x).join("guoapp");
            }
        }
        if cfg!(target_os = "windows") {
            let home = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_else(|_| ".".into());
            return std::path::PathBuf::from(home).join("AppData").join("Local").join("guoapp");
        }
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        if cfg!(target_os = "macos") {
            std::path::PathBuf::from(home).join("Library/Application Support/guoapp")
        } else {
            std::path::PathBuf::from(home).join(".local/share/guoapp")
        }
    };
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let msg = format!(
            "\n[panic {}] {}\n{}\n",
            ts,
            info,
            std::backtrace::Backtrace::force_capture()
        );
        let dir = default_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("panic.log"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, msg.as_bytes()));
        default_hook(info);
    }));
}

fn main() {
    install_panic_hook();
    guo::gui::run();
}
