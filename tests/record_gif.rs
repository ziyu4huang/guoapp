//! 录制 README 演示 GIF 的帧序列：真实网络 + 真实操作
//! （封面载入 → 点「更多AI剧」清单变长 → 点卡片开详情 → 下载进度 → 完成 ▶）。
//! 输出 $GUO_GIF_DIR 下 f0001.png…，再用 ffmpeg 调色板法组 GIF。
//! 运行：cargo test --test record_gif -- --ignored --nocapture

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

type App = guo::gui::GuiApp;
type H<'a> = Harness<'a, App>;

fn temp_data_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "guo-gif-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp data dir");
    dir
}

fn gif_dir() -> std::path::PathBuf {
    std::env::var("GUO_GIF_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp/guo-gif-frames"))
}

fn digits(text: &str) -> Option<usize> {
    let only: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    only.parse().ok()
}

fn ai_count(harness: &H) -> usize {
    harness
        .query_all_by_label_contains("AI剧 (")
        .filter_map(|n| n.label().and_then(|l| digits(&l)))
        .find(|c| *c > 0)
        .unwrap_or(0)
}

/// 录制器：按节奏 pump + 截帧（interval_ms 越小越顺滑）。
struct Recorder {
    dir: std::path::PathBuf,
    n: usize,
}
impl Recorder {
    fn new(dir: std::path::PathBuf) -> Recorder {
        std::fs::create_dir_all(&dir).expect("create gif dir");
        Recorder { dir, n: 0 }
    }
    fn snap(&mut self, harness: &mut H) {
        self.n += 1;
        let image = harness.render().expect("render");
        let out = self.dir.join(format!("f{:04}.png", self.n));
        image.save(&out).expect("save frame");
    }
    /// 持续 ms 时长，每 interval_ms 截一帧。
    fn record(&mut self, harness: &mut H, ms: u64, interval_ms: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while std::time::Instant::now() < deadline {
            harness.run_steps(2);
            self.snap(harness);
            std::thread::sleep(std::time::Duration::from_millis(interval_ms));
        }
    }
    /// 持续截帧直到条件成立或超时（保证录到状态切换的瞬间）。
    fn record_until(
        &mut self,
        harness: &mut H,
        secs: u64,
        interval_ms: u64,
        mut ready: impl FnMut(&mut H) -> bool,
    ) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            harness.run_steps(2);
            self.snap(harness);
            if ready(harness) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(interval_ms));
        }
        false
    }
}

fn click_tab(harness: &mut H, prefix: &str) {
    let tab = harness
        .query_all_by(|node| {
            node.role() == accesskit::Role::Button
                && node.label().map(|l| l.starts_with(prefix)).unwrap_or(false)
        })
        .next()
        .unwrap_or_else(|| panic!("tab not found: {}", prefix));
    tab.click();
    harness.run_steps(2);
}

fn first_card(harness: &mut H) -> String {
    const FIXED: [&str; 3] = ["清空封面缓存", "刷新", "搜索"];
    harness
        .query_all_by(|node| {
            node.role() == accesskit::Role::Button && node.toggled().is_none()
        })
        .filter_map(|node| node.label().map(|l| l.to_string()))
        .find(|label| {
            !FIXED.contains(&label.as_str())
                && !label.starts_with("更多")
                && !label.starts_with("真人剧")
                && !label.starts_with("漫剧")
                && !label.starts_with("AI剧")
                && !label.starts_with("⬇")
                && !label.starts_with("▶")
                && label.chars().count() > 2
        })
        .expect("no card buttons")
}

#[test]
#[ignore]
fn record_demo_gif() {
    let out = gif_dir();
    let _ = std::fs::remove_dir_all(&out);
    std::env::set_var("GUOAPP_DATA", temp_data_dir());
    let mut harness: H = Harness::builder()
        .with_size(egui::vec2(1180.0, 740.0))
        .with_pixels_per_point(1.0)
        .build_eframe(|cc| guo::gui::GuiApp::new(cc));
    let mut rec = Recorder::new(out);

    // A. 目录载入：封面逐张浮现（本身就是很好的演示）
    rec.record_until(&mut harness, 90, 400, |h| {
        h.query_all_by_label_contains("共").count() >= 40
    })
    .then_some(())
    .expect("catalog load timeout");
    rec.record(&mut harness, 1600, 280);

    // B. 点「更多AI剧」：AI剧 计数增长、新卡片追加
    let before = ai_count(&harness);
    harness
        .get_by_role_and_label(accesskit::Role::Button, "更多AI剧")
        .click();
    let grew = rec.record_until(&mut harness, 40, 300, |h| ai_count(h) > before);
    assert!(grew, "更多AI剧 未增长");
    rec.record(&mut harness, 1400, 260);

    // C. 点第一张卡片：详情面板展开
    let title = first_card(&mut harness);
    harness
        .get_by_role_and_label(accesskit::Role::Button, &title)
        .click();
    let opened = rec.record_until(&mut harness, 40, 300, |h| {
        h.query_by_label_contains("全部下载").is_some()
    });
    assert!(opened, "详情未展开");
    rec.record(&mut harness, 1200, 260);

    // D. 只选第 1 集 → 下载选中 → 切片库看进度条
    harness.get_by_label("全不选").click();
    harness.run_steps(2);
    rec.snap(&mut harness);
    harness
        .get_by_role_and_label(accesskit::Role::Button, "1")
        .click();
    harness.run_steps(2);
    harness.get_by_label_contains("下载选中").click();
    rec.record(&mut harness, 500, 200);
    click_tab(&mut harness, "片库");
    rec.record(&mut harness, 700, 200);

    // E. 进度条推进到「完成 mp4 1080p」（绿字出现瞬间必须录到）
    let done = rec.record_until(&mut harness, 300, 300, |h| {
        h.query_by_label_contains("完成 mp4").is_some()
    });
    assert!(done, "下载未完成");
    rec.record(&mut harness, 1800, 260);

    eprintln!("frames: {} → {}", rec.n, rec.dir.display());
}
