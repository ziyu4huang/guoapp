//! 全功能截图自动化（教学视频 + README 素材）：
//! 目录 → 搜索 → 详情选集 → 下载进度 → 片库，真实网络 + 无头渲染。
//! 输出到 $GUO_SHOT_DIR（默认 docs/shots），1920×1200（1280×800 @1.5x）。
//! 运行：cargo test --test screenshots -- --ignored --nocapture

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

type App = guo::gui::GuiApp;
type H<'a> = Harness<'a, App>;

fn temp_data_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "guo-shots-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp data dir");
    dir
}

fn shot_dir() -> std::path::PathBuf {
    std::env::var("GUO_SHOT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("docs/shots"))
}

fn digits(text: &str) -> Option<usize> {
    let only: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    only.parse().ok()
}

fn pump_until(harness: &mut H, secs: u64, mut ready: impl FnMut(&mut H) -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        harness.run_steps(2);
        if ready(harness) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    panic!("pump_until: condition not met within {}s", secs);
}

/// 无条件泵帧 N 秒：等封面图等异步资源在画面上落定（封面走独立线程下载 +
/// sips 转码，accesskit 查不到纹理，只能时间维稳）。
fn settle(harness: &mut H, secs: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline {
        harness.run_steps(2);
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn save_png(harness: &mut H, dir: &std::path::Path, name: &str) {
    let image = harness.render().expect("render");
    std::fs::create_dir_all(dir).expect("create shot dir");
    let out = dir.join(name);
    image.save(&out).expect("save png");
    eprintln!("shot → {}", out.display());
}

fn click_tab(harness: &mut H, label: &str) {
    let tab = harness
        .query_all_by(|node| {
            node.role() == accesskit::Role::Button && node.label() == Some(label.to_string())
        })
        .next()
        .unwrap_or_else(|| panic!("tab not found: {}", label));
    tab.click();
    harness.run_steps(2);
}

/// 第一张非固定按钮的剧目卡片（目录卡片墙）。
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
fn capture_feature_screenshots() {
    let out = shot_dir();
    std::env::set_var("GUOAPP_DATA", temp_data_dir());
    let mut harness: H = Harness::builder()
        .with_size(egui::vec2(1280.0, 800.0))
        .with_pixels_per_point(1.5)
        .build_eframe(|cc| guo::gui::GuiApp::new(cc));

    // 1. 目录：三个分类都加载出卡片（「共 N 集」标签 ≥ 40 张）
    pump_until(&mut harness, 60, |h| {
        h.query_all_by_label_contains("共").count() >= 40
    });
    settle(&mut harness, 12);
    save_png(&mut harness, &out, "catalog.png");

    // 2. 搜索：输入剧名 → 结果卡片墙
    click_tab(&mut harness, "搜索");
    harness.run_steps(4);
    let field = harness
        .query_by_role(accesskit::Role::TextInput)
        .expect("search text field");
    field.focus();
    harness.run_steps(2);
    harness
        .input_mut()
        .events
        .push(egui::Event::Text("重生".into()));
    harness.run_steps(2);
    harness
        .query_all_by(|node| {
            node.role() == accesskit::Role::Button
                && node.label() == Some("搜索".to_string())
                && node.toggled().is_none()
        })
        .next()
        .expect("search button")
        .click();
    pump_until(&mut harness, 40, |h| {
        h.query_all_by_label_contains("共").count() > 0
    });
    settle(&mut harness, 8);
    save_png(&mut harness, &out, "search.png");

    // 3. 回目录，打开一部剧的详情面板
    click_tab(&mut harness, "目录");
    pump_until(&mut harness, 20, |h| {
        h.query_all_by_label_contains("共").count() >= 40
    });
    let title = first_card(&mut harness);
    harness
        .get_by_role_and_label(accesskit::Role::Button, &title)
        .click();
    pump_until(&mut harness, 40, |h| {
        h.query_by_label_contains("全部下载").is_some()
    });
    settle(&mut harness, 5);
    save_png(&mut harness, &out, "detail.png");

    // 4. 下载第 1 集：进度条出现后截「下载中」，完成标志出现后截「片库」
    harness.run_steps(2);
    harness.get_by_label("全不选").click();
    harness.run_steps(2);
    harness
        .get_by_role_and_label(accesskit::Role::Button, "1")
        .click();
    harness.run_steps(2);
    harness.get_by_label_contains("下载选中").click();
    // 下载中：底部「⬇ 1 个下载中」出现即截（抓进度条最好的时机）
    pump_until(&mut harness, 30, |h| {
        h.query_by_label_contains("个下载中").is_some()
    });
    save_png(&mut harness, &out, "downloading.png");
    // 等下载完成 → 片库 (1)
    pump_until(&mut harness, 300, |h| {
        h.query_by_label_contains("片库 (1)").is_some()
    });
    click_tab(&mut harness, "片库 (1)");
    harness.run_steps(4);
    pump_until(&mut harness, 10, |h| {
        h.query_by_label_contains("打开目录").is_some()
    });
    save_png(&mut harness, &out, "library.png");

    // 5. 分页：点「更多AI剧」前后数量对比（体现「更多×」可无限下滑）
    click_tab(&mut harness, "目录");
    pump_until(&mut harness, 20, |h| {
        h.query_all_by_label_contains("AI剧 (").count() == 1
    });
    let ai_before = harness
        .query_all_by_label_contains("AI剧 (")
        .filter_map(|n| n.label().and_then(|l| digits(&l)))
        .find(|c| *c > 0)
        .unwrap_or(0);
    harness
        .get_by_role_and_label(accesskit::Role::Button, "更多AI剧")
        .click();
    pump_until(&mut harness, 60, |h| {
        let now = h
            .query_all_by_label_contains("AI剧 (")
            .filter_map(|n| n.label().and_then(|l| digits(&l)))
            .find(|c| *c > 0)
            .unwrap_or(0);
        now > ai_before
    });
    save_png(&mut harness, &out, "load-more.png");

    eprintln!("全部截图完成 → {}", out.display());
}
