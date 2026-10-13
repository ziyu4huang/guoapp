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

/// IPC 单实例播放会话端到端实测（真实串流，无头渲染）：
/// launch → 第一集 start-file → loadfile 换第二集 → end-file 事件 → quit。
/// 运行：cargo test --test record_gif ipc_session_live -- --ignored --nocapture
#[test]
#[ignore]
fn ipc_session_live() {
    use guo::player::{self, PlayItem, PlayerEvent};

    let store = guo::state::Store::load();
    let client = guo::hgapp::HgClient::new();
    let drama_id = "hongguo:7694857999663238168";
    let (_, chapters) = guo::hgapp::detail_with_fallback(&client, &store, drama_id)
        .expect("detail");
    assert!(chapters.len() >= 2, "测试剧目需要至少 2 集");

    let resolve = |ep: usize| {
        let ch = chapters.iter().find(|c| c.index == ep).unwrap();
        guo::media::try_resolve_media(&client, &store, "7694857999663238168", &ch.video_id)
            .expect("resolve")
    };
    let m1 = resolve(1);
    let item = |m: &guo::model::Media, ep: usize| PlayItem {
        url: m.url.clone(),
        referer: m.referer.clone(),
        key: m.cenc_key.as_ref().map(|k| hex::encode(k)),
        title: format!("《世子爷他要以身相许》第{}集", ep),
    };

    let (kind, program) = player::resolve_player();
    assert_eq!(kind, player::PlayerKind::Mpv, "本机应走 mpv（无 IINA）");
    let ipc = player::ipc_path();
    let log = store.dir.join("mpv-test.log");
    eprintln!("step1: launch");
    let mut session = player::PlayerSession::launch(
        kind,
        &program,
        &item(&m1, 1),
        &ipc,
        &log,
        false,
        &Default::default(),
    )
        .expect("launch mpv");
    eprintln!("step2: session up, wait file-loaded");

    let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    let ev_running = running.clone();
    let ev_ipc = session.ipc.clone();
    std::thread::spawn(move || {
        player::event_loop(&ev_ipc, &ev_running, move |ev| ev_tx.send(ev).is_ok())
    });

    // 等 IPC 就绪 + 第一集开播（headless 下 start-file 事件即视为在播）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut started = false;
    while std::time::Instant::now() < deadline {
        if let Ok(ev) = ev_rx.recv_timeout(std::time::Duration::from_millis(500)) {
            if ev == PlayerEvent::FileLoaded {
                started = true;
                break;
            }
        }
    }
    assert!(started, "30s 内未收到 file-loaded（第一集未开播）");
    eprintln!("✓ 第一集开播（IPC 事件链路通；注：start-file 可能早于连接被错过，file-loaded 才是可靠的开播信号）");

    // 换第二集：同一会话 loadfile replace
    let m2 = resolve(2);
    eprintln!("step4: resolved ep2, sending loadfile");
    session.load(&item(&m2, 2)).expect("loadfile ep2");
    eprintln!("step5: loadfile acknowledged");

    // 应先收到换集引起的 end-file(Replaced)，再收到第二集 start-file
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut replaced_seen = false;
    let mut started2 = false;
    while std::time::Instant::now() < deadline {
        if let Ok(ev) = ev_rx.recv_timeout(std::time::Duration::from_millis(500)) {
            eprintln!("  ev: {:?}", ev);
            match ev {
                PlayerEvent::Replaced => replaced_seen = true,
                PlayerEvent::FileLoaded => {
                    started2 = true;
                    break;
                }
                _ => {}
            }
        }
    }
    assert!(started2, "30s 内第二集未开播（file-loaded）");
    eprintln!("✓ 同一窗口换集成功（replaced_seen={}）", replaced_seen);

    // 退出
    running.store(false, std::sync::atomic::Ordering::Relaxed);
    session.quit();
    assert!(!session.process_alive(), "quit 后进程应已退出");
    eprintln!("✓ 会话退出干净");
}

/// eof 后进程/窗口必须存活（idle+force-window 连播形态）且 IPC 仍可 loadfile
/// 接集——连播不断窗/不假播放的根基（旧版 mpv 默认 eof 退出进程，loadfile 打进
/// 垂死进程会被接受但随即退出 = UI 显示在播实际黑屏）。播完信号=end-file eof
/// （idle 会话照常发；keep-open 方案因 mpv 0.41 eof 通知断供已废弃，实录见
/// session_option_args）。本地 2s 样片，不依赖网络。
/// 运行：cargo test --test record_gif eof_keepopen -- --ignored --nocapture
#[test]
#[ignore]
fn eof_keepopen_session_alive_then_loadfile_next() {
    use guo::player::{self, PlayItem, PlayerEvent};

    std::env::set_var("GUO_MPV_ARGS", "--vo=null --ao=null");
    let dir = std::env::temp_dir().join(format!("guo-eof-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("tmp dir");
    let clip = |name: &str, color: &str| {
        let p = dir.join(name);
        let st = std::process::Command::new("ffmpeg")
            .args([
                "-y", "-loglevel", "error",
                "-f", "lavfi", "-i", &format!("color=c={color}:s=320x240:d=2"),
                "-f", "lavfi", "-i", "sine=frequency=440:duration=2",
                "-pix_fmt", "yuv420p", "-shortest", "-c:v", "libx264",
            ])
            .arg(&p)
            .status()
            .expect("ffmpeg");
        assert!(st.success(), "ffmpeg 生成失败: {}", name);
        p
    };
    let ep1 = clip("ep1.mp4", "red");
    let ep2 = clip("ep2.mp4", "blue");
    let item = |p: &std::path::Path, title: &str| PlayItem {
        url: p.display().to_string(),
        referer: String::new(),
        key: None,
        title: title.into(),
    };

    let (kind, program) = player::resolve_player();
    let ipc = player::ipc_path();
    let log = dir.join("mpv-eof.log");
    // 恢复参数格式实测：负坐标位置（副屏在主屏左/上时 x/y 为负）+ 窗口缩放。
    // mpv 对非法 option 值会立刻退出——本测试能开播即证明格式被接受。
    let prefs = player::PlayerPrefs {
        x: Some(-1920),
        y: Some(-80),
        window_scale: Some(1.0),
        ..Default::default()
    };
    eprintln!("step1: launch (keep-open 会话 + 恢复参数 -1920,-80 @1.0)");
    let mut session = player::PlayerSession::launch(
        kind,
        &program,
        &item(&ep1, "测试第1集"),
        &ipc,
        &log,
        true,
        &prefs,
    )
    .expect("launch mpv");

    let running = session.running_flag();
    let (ev_tx, ev_rx) = std::sync::mpsc::channel();
    let ev_ipc = session.ipc.clone();
    std::thread::spawn(move || {
        // 生产路径：event_loop（end-file eof 是 idle 会话的播完信号）
        player::event_loop(&ev_ipc, &running, move |ev| ev_tx.send(ev).is_ok())
    });

    let wait_ended = |what: &str, secs: u64| {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            match ev_rx.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(PlayerEvent::Ended) => return,
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if std::time::Instant::now() > deadline {
                        panic!("{what}: {}s 内未等到 eof（本地 2s 片应早已播完）", secs);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("{what}: 事件通道关闭（reader 线程已退出）")
                }
            }
        }
    };
    // 注：本地 2s 片加载快过 IPC 连线建立，file-loaded 会在 reader 连上之前
    // 发完（真实网络串流则不会）——所以「开播」无法用事件断言，改等自然播完
    // 的 eof，它同时证明「文件真的加载并播到了底」，是更强的证据。
    wait_ended("第一集播完", 30);
    eprintln!("✓ 第一集自然播完（end-file eof，idle 会话进程/窗口存活）");

    // 核心：eof 后——keep-open 让进程活着，IPC 仍可用
    assert!(session.process_alive(), "eof 后进程必须存活（--keep-open=yes 失效？）");
    assert!(session.ipc_ready(), "eof 后 IPC 必须仍可达");
    eprintln!("✓ eof 后进程存活 + IPC 可达（旧版此处 mpv 已退出 = 连播 bug 根源）");

    // 设定捕获：eof 后 idle 会话文件已卸载——全局量（音量/静音/全屏/最大化）
    // 仍应可读，文件级量（speed）拿不到=None（生产上 FileLoaded 时已捕过真值，
    // 正合「部分失败不连坐」）；null vo 无窗口 → scale/位置也拿不到。
    let caps = session.capture_prefs();
    assert!((caps.volume.unwrap_or(-1.0) - 100.0).abs() < 0.5, "volume 应为默认 100: {:?}", caps);
    assert!(!caps.mute, "mute 默认关: {:?}", caps);
    assert!(
        caps.speed.map(|s| (s - 1.0).abs() < 0.01).unwrap_or(true),
        "speed 缺席=None 或 1.0: {:?}",
        caps
    );
    assert!(!caps.fullscreen && !caps.maximized, "全屏/最大化默认关: {:?}", caps);
    // scale 读回的是选项当前值：本测试种了 1.0 → Some(1.0)（真窗口下=实际缩放）
    assert!(
        caps.window_scale.map(|s| (s - 1.0).abs() < 0.01).unwrap_or(true),
        "scale 应回读种子值 1.0（或缺席）: {:?}",
        caps
    );
    eprintln!("✓ IPC 设定捕获链路通（{:?}）", caps);

    // eof 后接第二集：同窗口 loadfile replace（连播的真实路径）；
    // 第二集也必须播到底（Ended）= 真加载真播放，不是「接受了命令但没播」
    session.load(&item(&ep2, "测试第2集")).expect("eof 后 loadfile 必须成功");
    wait_ended("第二集播完", 30);
    eprintln!("✓ eof 后同一窗口接集成功且播到底（连播无缝）");

    session.quit();
    assert!(!session.process_alive(), "quit 后进程应已退出");
    let _ = std::fs::remove_dir_all(&dir);
    eprintln!("✓ 会话退出干净");
}

/// GUI 播放链路冒烟:点「▶ 播放第1集」→ 状态「▶ 正在播放」→ 详情面板出现
/// 「正在播放:第1集」+ 集数格「▶1」标记 + 底栏常驻播放指示。
/// GUO_MPV_ARGS=--vo=null --ao=null 由测试自身注入(无头环境 mpv 真窗口起不来)。
#[test]
#[ignore]
fn gui_play_now_playing() {
    std::env::set_var("GUO_MPV_ARGS", "--vo=null --ao=null");
    std::env::set_var("GUOAPP_DATA", temp_data_dir());
    let mut harness: H = Harness::builder()
        .with_size(egui::vec2(1180.0, 740.0))
        .with_pixels_per_point(1.0)
        .build_eframe(|cc| guo::gui::GuiApp::new(cc));

    pump_until(&mut harness, 60, |h| {
        h.query_all_by_label_contains("共").count() >= 40
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
        h.query_by_label_contains("播放第1集").is_some()
    });
    harness.get_by_label_contains("播放第1集").click();

    // 详情面板:「正在播放:第1集」标记(依赖 mpv spawn 成功)
    pump_until(&mut harness, 40, |h| {
        h.query_by_label_contains("正在播放：第1集").is_some()
    });
    // 集数格出现「▶1」金色标记
    pump_until(&mut harness, 10, |h| {
        h.query_by_label_contains("▶1").is_some()
    });
    // 下一集按钮出现(第1集在播 → 下一集=第2集)
    pump_until(&mut harness, 10, |h| {
        h.query_by_label_contains("下一集").is_some()
    });
    eprintln!("✓ GUI 播放链路:now_playing 标记/▶1 集格标记/下一集按钮 全部在场");
}
