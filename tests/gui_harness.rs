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


/// 隔离测试媒体源:本地 Range 伺服器(serve 一段伪随机字节,支援 Range 续传)。
/// 配合 GUO_RESOLVE_OVERRIDE=返回的 URL,让串流测试与上游可用性彻底脱钩
/// (INC-20261015 上游锁定后,所有真网播放测试都会假红)。
fn hermetic_media() -> String {
    use std::io::{Read, Write};
    let body: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
    let body = std::sync::Arc::new(body);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let body = body.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut one = [0u8; 1];
                loop {
                    let n = stream.read(&mut one).unwrap_or(0);
                    if n == 0 || buf.len() > 16384 {
                        return;
                    }
                    buf.extend_from_slice(&one[..n]);
                    if buf.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&buf).to_string();
                let total = body.len();
                let range = request
                    .lines()
                    .find_map(|l| l.strip_prefix("Range: bytes="))
                    .map(|s| s.trim().trim_start_matches("bytes=").to_string());
                let (status, extra, slice): (&str, String, &[u8]) = match range.as_deref() {
                    Some(spec) => {
                        let mut it = spec.splitn(2, '-');
                        let s: usize = it.next().unwrap_or("").trim().parse().unwrap_or(0);
                        let e: usize = it
                            .next()
                            .unwrap_or("")
                            .trim()
                            .parse::<usize>()
                            .map(|v| (v + 1).min(total))
                            .unwrap_or(total);
                        if s < e && e <= total {
                            (
                                "206 Partial Content",
                                format!("Content-Range: bytes {}-{}/{}\r\n", s, e - 1, total),
                                &body[s..e],
                            )
                        } else {
                            ("200 OK", String::new(), &body[..])
                        }
                    }
                    None => ("200 OK", String::new(), &body[..]),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                    slice.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(slice);
            });
        }
    });
    format!("http://127.0.0.1:{port}/sample.mp4")
}


/// 右键点击（勾选/取消下载）：egui_kittest 只有左键 helper，
/// 这里注入原生 PointerButton 事件（坐标取节点 bounds 中心）。
fn right_click(h: &mut H, label: &str) {
    let pos = {
        let node = h.get_by_role_and_label(accesskit::Role::Button, label);
        let b = node.raw_bounds().expect("节点无 bounds");
        egui::pos2(((b.x0 + b.x1) / 2.0) as f32, ((b.y0 + b.y1) / 2.0) as f32)
    };
    h.input_mut().events.push(egui::Event::PointerMoved(pos));
    for pressed in [true, false] {
        h.input_mut().events.push(egui::Event::PointerButton {
            pos,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: Default::default(),
        });
    }
    h.run_steps(2);
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

/// 「更多AI剧」分页回归测试：点击后 AI剧 卡片数必须严格增长。
/// （旧 bug：分页 cursor 被丢弃，每次点击都重抓第一页，清单不变。）
/// 运行：cargo test --test gui_harness catalog_load_more_grows -- --ignored --nocapture
#[test]
#[ignore]
fn catalog_load_more_grows() {
    std::env::set_var("GUOAPP_DATA", temp_data_dir());
    let mut harness: H = Harness::builder()
        .with_size(egui::vec2(1180.0, 760.0))
        .with_pixels_per_point(1.0)
        .build_eframe(|cc| guo::gui::GuiApp::new(cc));

    // 初始目录加载完成：分类计数标签从 (0) 变为有内容（如 "AI剧 (18)"；
    // 标签本身第一帧就存在，必须等计数 > 0）
    pump_until(&mut harness, 60, |h| ai_count(h) > 0);
    fn ai_count(h: &H) -> usize {
        h.query_all_by_label_contains("AI剧 (")
            .filter_map(|n| n.label().and_then(|l| digits(&l)))
            .find(|c| *c > 0)
            .unwrap_or(0)
    }
    let before = ai_count(&harness);
    assert!(before > 0, "AI剧 初始目录为空");

    // 点「更多AI剧」→ 数量必须严格增长（修复前：数量不变）
    harness
        .get_by_role_and_label(accesskit::Role::Button, "更多AI剧")
        .click();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        harness.run_steps(2);
        let now = ai_count(&harness);
        if now > before {
            eprintln!("AI剧 卡片数 {} → {}，「更多」分页正常", before, now);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "点击「更多AI剧」后数量未增长（{} → {}）：分页 cursor 修复失效",
            before,
            now
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    // 再点一次确认可连续翻页且不误标「已到底」
    let mid = ai_count(&harness);
    harness
        .get_by_role_and_label(accesskit::Role::Button, "更多AI剧")
        .click();
    pump_until(&mut harness, 90, |h| ai_count(h) > mid);
    save_png(&mut harness, "guo-kit-load-more.png");
}

/// 全链路真实 API 测试，显式运行：cargo test --test gui_harness -- --ignored --nocapture
#[test]
#[ignore]
fn gui_full_flow() {
    // 下载链路走本地媒体源(hermetic):上游锁定(INC-20261015)后真网下载不可用
    std::env::set_var("GUO_RESOLVE_OVERRIDE", hermetic_media());
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
        h.query_by_label_contains("点击集数即播放").is_some()
    });
    save_png(&mut harness, "guo-kit-detail.png");

    // 4. 只保留第 1 集选中：取消其余集数（详情默认全选）
    eprintln!(
        "选中剧：{}，切「⬇ 下载」模式→全不选→右键勾选第 1 集",
        shortest_title
    );
    harness.run_steps(2);
    harness.get_by_label_contains("⬇ 下载").click();
    harness.run_steps(2);
    harness.get_by_label("全不选").click();
    harness.run_steps(2);
    right_click(&mut harness, "1");
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

/// 新交互语义：左键点集数格=直接播放该集（出现「正在播放」），
/// 右键=勾选/取消下载（计数变化）。真串流。
/// 运行：cargo test --test gui_harness episode_cell_semantics -- --ignored
#[test]
#[ignore]
fn episode_cell_semantics() {
    std::env::set_var("GUO_MPV_ARGS", "--vo=null --ao=null");
    std::env::set_var("GUO_RESOLVE_OVERRIDE", hermetic_media());
    std::env::set_var("GUOAPP_DATA", temp_data_dir());
    let mut harness: H = Harness::builder()
        .with_size(egui::vec2(1180.0, 760.0))
        .with_pixels_per_point(1.0)
        .build_eframe(|cc| guo::gui::GuiApp::new(cc));

    pump_until(&mut harness, 60, |h| {
        h.query_all_by_label_contains("共").count() >= 20
    });
    const FIXED: [&str; 3] = ["清空封面缓存", "刷新", "搜索"];
    let title = harness
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
        .expect("no cards");
    harness
        .get_by_role_and_label(accesskit::Role::Button, &title)
        .click();
    pump_until(&mut harness, 40, |h| {
        h.query_by_label_contains("点击集数即播放").is_some()
    });

    // A. 看片模式：左键点集格 1 = 播放该集（不是勾选下载）
    harness
        .get_by_role_and_label(accesskit::Role::Button, "1")
        .click();
    pump_until(&mut harness, 40, |h| {
        h.query_by_label_contains("正在播放：第1集").is_some()
    });
    eprintln!("✓ 左键点集数格 = 直接播放该集");

    // B. 切「⬇ 下载」模式：勾选才回来（与看片分家）
    harness.get_by_label_contains("⬇ 下载").click();
    harness.run_steps(2);
    harness.get_by_label("全不选").click();
    harness.run_steps(2);
    right_click(&mut harness, "2");
    pump_until(&mut harness, 5, |h| {
        h.query_by_label_contains("下载选中（1）").is_some()
    });
    eprintln!("✓ 右键点集数格 = 勾选/取消下载");
}

/// WatchSession 解耦验收：播放中关掉详情面板，底栏播放条仍有「下一集」
/// 且能真正推进到第 2 集（旧架构下 request_play 依赖 detail，关闭即断链）。
#[test]
#[ignore]
fn watch_survives_detail_close() {
    std::env::set_var("GUO_MPV_ARGS", "--vo=null --ao=null");
    std::env::set_var("GUO_RESOLVE_OVERRIDE", hermetic_media());
    std::env::set_var("GUOAPP_DATA", temp_data_dir());
    let mut harness: H = Harness::builder()
        .with_size(egui::vec2(1180.0, 760.0))
        .with_pixels_per_point(1.0)
        .build_eframe(|cc| guo::gui::GuiApp::new(cc));

    pump_until(&mut harness, 60, |h| {
        h.query_all_by_label_contains("共").count() >= 20
    });
    const FIXED: [&str; 3] = ["清空封面缓存", "刷新", "搜索"];
    let title = harness
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
        .expect("no cards");
    harness
        .get_by_role_and_label(accesskit::Role::Button, &title)
        .click();
    pump_until(&mut harness, 40, |h| {
        h.query_by_label_contains("播放第 1 集").is_some()
    });
    harness.get_by_label_contains("播放第 1 集").click();
    pump_until(&mut harness, 40, |h| {
        h.query_by_label_contains("正在播放：第1集").is_some()
    });

    // 关掉详情面板（✕）——看片会话必须独立存活；
    // 先断言详情真的关了（集数网格提示消失），底栏「下一集」才唯一
    harness.get_by_label("✕").click();
    pump_until(&mut harness, 10, |h| {
        h.query_by_label_contains("点击集数即播放").is_none()
    });
    pump_until(&mut harness, 10, |h| {
        h.query_by_label_contains("⏭ 下一集（第2集）").is_some()
    });
    eprintln!("✓ 关详情后底栏播放条仍在（⏭ 下一集）");
    harness.get_by_label_contains("⏭ 下一集").click();
    // 详情已关：无「正在播放」面板标签，成功信号=底栏/状态里的「》第2集」
    pump_until(&mut harness, 40, |h| {
        h.query_all_by_label_contains("》第2集").count() >= 1
    });
    eprintln!("✓ 关详情后底栏「下一集」真实推进到第 2 集（解耦成立）");
}

/// 片库内建播放 E2E：预置 library.json（本地 2s 片）→ 片库 → 点 ▶1 →
/// 状态出现「正在播放」——下載內容不再丟給系統播放器，走同一套 mpv 會話。
/// 运行：cargo test --test gui_harness library_builtin_play -- --ignored
#[test]
#[ignore]
fn library_builtin_play() {
    std::env::set_var("GUO_MPV_ARGS", "--vo=null --ao=null");
    let data = temp_data_dir();
    std::fs::create_dir_all(&data).unwrap();
    // 生成 2s 本地样片
    let ep_dir = data.join("test-drama").join("ep_001");
    std::fs::create_dir_all(&ep_dir).unwrap();
    let clip = ep_dir.join("media.mp4");
    let st = std::process::Command::new("ffmpeg")
        .args([
            "-y", "-loglevel", "error",
            "-f", "lavfi", "-i", "color=c=green:s=320x240:d=2",
            "-f", "lavfi", "-i", "sine=frequency=440:duration=2",
            "-pix_fmt", "yuv420p", "-shortest", "-c:v", "libx264",
        ])
        .arg(&clip)
        .status()
        .expect("ffmpeg");
    assert!(st.success());
    // 预写 library.json（Store::load 会读）
    let library = serde_json::json!({
        "hongguo:testlocal": {
            "drama_id": "hongguo:testlocal",
            "title": "测试本地剧",
            "dir": ep_dir.parent().unwrap().display().to_string(),
            "entry": "",
            "kind": "mp4",
            "episodes": [{ "index": 1, "quality": 1080, "file": "media.mp4" }]
        }
    });
    std::fs::write(
        data.join("library.json"),
        serde_json::to_vec_pretty(&library).unwrap(),
    )
    .unwrap();
    std::env::set_var("GUOAPP_DATA", &data);

    let mut harness: H = Harness::builder()
        .with_size(egui::vec2(1180.0, 760.0))
        .with_pixels_per_point(1.0)
        .build_eframe(|cc| guo::gui::GuiApp::new(cc));

    click_tab(&mut harness, "片库 (1)");
    harness.run_steps(2);
    // ▶1 = 内建播放（不再是系统播放器）
    harness
        .get_by_role_and_label(accesskit::Role::Button, "▶1")
        .click();
    pump_until(&mut harness, 30, |h| {
        h.query_all_by_label_contains("》第1集").count() >= 1
    });
    eprintln!("✓ 片库 ▶ 内建播放：本地集走 mpv 会话（底栏可见）");
}
