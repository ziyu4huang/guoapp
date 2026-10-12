//! 无头 GUI 交互测试：真实网络 + 真实 egui 渲染，覆盖
//! 目录加载 → 搜索 → 详情选集 → 下载 → 片库更新 全链路。
//! 运行：cargo test --test gui_harness -- --ignored --nocapture

use egui_kittest::kittest::Queryable;
use egui_kittest::Harness;

type App = guo::gui::GuiApp;
type H<'a> = Harness<'a, App>;

fn temp_data_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "guo-kittest-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create temp data dir");
    dir
}

fn digits(text: &str) -> Option<usize> {
    let only: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    only.parse().ok()
}

/// Pump frames until `ready` turns true or `secs` elapse.
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

/// 点顶部分页（selectable_value 在 accesskit 中是带 toggled 的 Button）。
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

fn save_png(harness: &mut H, name: &str) {
    let image = harness.render().expect("render");
    let out = std::env::temp_dir().join(name);
    image.save(&out).expect("save png");
    eprintln!("screenshot → {}", out.display());
}

/// 全链路真实 API 测试，显式运行：cargo test --test gui_harness -- --ignored --nocapture
#[test]
#[ignore]
fn gui_full_flow() {
    std::env::set_var("GUOAPP_DATA", temp_data_dir());
    let mut harness: H = Harness::builder()
        .with_size(egui::vec2(1180.0, 760.0))
        .with_pixels_per_point(1.0)
        .build_eframe(|cc| guo::gui::GuiApp::new(cc));

    // 1. 目录：三分类卡片墙加载完成
    pump_until(&mut harness, 60, |h| {
        h.query_all_by_label_contains("共").count() >= 20
    });
    save_png(&mut harness, "guo-kit-catalog.png");

    // 2. 搜索：聚焦输入框 → 文本事件 → 点搜索按钮
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
    // 搜索按钮：同名分頁已选中（toggled），按钮 toggled = None
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
    save_png(&mut harness, "guo-kit-search.png");

    // 3. 回目录，打开分集最少的一部剧
    click_tab(&mut harness, "目录");
    pump_until(&mut harness, 20, |h| {
        h.query_all_by_label_contains("共").count() >= 20
    });
    harness.run_steps(2);
    let counts: Vec<usize> = harness
        .query_all_by_label_contains("共")
        .filter_map(|node| node.value().and_then(|l| digits(&l)))
        .collect();
    let shortest_count = counts.iter().copied().min().expect("no drama cards");
    // 卡片按钮 label = 剧名（WidgetType::Button）；排除顶栏固定按钮后取第一张。
    // 集数与剧名的对应不重要：后面按 count 循环取消选中即可。
    const FIXED: [&str; 3] = ["清空封面缓存", "刷新", "搜索"];
    let shortest_title = harness
        .query_all_by(|node| {
            node.role() == accesskit::Role::Button && node.toggled().is_none()
        })
        .filter_map(|node| node.label().map(|l| l.to_string()))
        .find(|label| {
            !FIXED.contains(&label.as_str())
                && !label.starts_with("更多")
                && !label.starts_with("⬇")
                && !label.starts_with("▶")
                && label.chars().count() > 2
        })
        .expect("no card buttons");
    harness
        .get_by_role_and_label(accesskit::Role::Button, &shortest_title)
        .click();
    pump_until(&mut harness, 40, |h| {
        h.query_by_label_contains("全部下载").is_some()
    });
    save_png(&mut harness, "guo-kit-detail.png");

    // 4. 只保留第 1 集选中：取消其余集数（详情默认全选）
    eprintln!("选中剧：{}，全不选后仅选第 1 集下载", shortest_title);
    harness.run_steps(2);
    harness.get_by_label("全不选").click();
    harness.run_steps(2);
    harness
        .get_by_role_and_label(accesskit::Role::Button, "1")
        .click();
    harness.run_steps(2);
    harness
        .get_by_label_contains("下载选中")
        .click();

    // 5. 等下载完成：片库计数变为 (1)
    pump_until(&mut harness, 300, |h| {
        h.query_by_label_contains("片库 (1)").is_some()
    });
    save_png(&mut harness, "guo-kit-downloaded.png");

    // 6. 片库页面展示剧目与本集按钮
    click_tab(&mut harness, "片库 (1)");
    harness.run_steps(4);
    pump_until(&mut harness, 10, |h| {
        h.query_by_label_contains("打开目录").is_some()
    });
    save_png(&mut harness, "guo-kit-library-tab.png");

}
