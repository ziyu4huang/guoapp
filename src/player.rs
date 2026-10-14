//! 串流播放会话：单实例播放器 + JSON IPC（mpv 协议，跨平台）。
//!
//! 设计：
//! - 会话只 spawn 一次；换集走 IPC `loadfile replace`，不再每集开新窗口
//!   （旧行为每集一个进程 = 多窗口闪跳的来源之一）。
//! - GUI 连播形态（keepalive）：`--idle=yes --force-window=yes`——播完 eof 文件
//!   正常卸载（end-file eof = 最可靠的播完事件），但进程与窗口不退，下一集
//!   打进同一窗口不闪窗。实录：keep-open 方案下 mpv 0.41 不发 end-file 且
//!   eof-reached 通知断供（见 session_option_args），废弃。
//! - 缓冲策略（session_option_args）：起播先缓冲 3s、预读 60s/上限 128MiB、
//!   CDN 断线自动重连（lavf reconnect）、15s 网络超时兜底——串流卡顿三件套。
//! - `--hwdec=auto`（macOS VideoToolbox / Windows d3d11va / Linux vaapi）：
//!   HEVC 1080x1920 软解曾 53 秒丢 139 帧（A-V 漂移 0.16s）= 用户看到的「闪跳」。
//! - `--force-media-title` 显示《剧名》第N集（窗口标题 + OSC）。
//! - stderr 收进日志文件，不再直喷终端/父进程日志；启动失败时由调用方取尾行展示。
//! - IPC 客户端零依赖：Unix 用 `UnixStream`，Windows 用 `fs` 打开命名管道
//!   （mpv 的管道按文件打开是官方支持用法）；每条命令新建连接、写完即关。
//! - 播放器选择：默认 mpv；IINA 默认关闭（`GUO_PLAYER=iina` 显式开启）；`GUO_MPV` 指向任意兼容播放器
//!   > PATH 里的 mpv。三者都是 mpv 内核，IPC 协议一致。

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// 播放器窗口与播放设定记忆（GUI/CLI 共用，存 state.json，跨启动恢复）。
/// 只存「能精确取回」的量：大小用 window-scale（相对视频，跨显示器/HIDPI 自洽），
/// 位置用 macOS 窗口列表坐标（其余平台暂无零依赖取法 → None=不恢复位置）。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlayerPrefs {
    #[serde(default)]
    pub x: Option<i32>,
    #[serde(default)]
    pub y: Option<i32>,
    #[serde(default)]
    pub window_scale: Option<f64>,
    #[serde(default)]
    pub maximized: bool,
    #[serde(default)]
    pub fullscreen: bool,
    #[serde(default)]
    pub volume: Option<f64>,
    #[serde(default)]
    pub mute: bool,
    #[serde(default)]
    pub speed: Option<f64>,
}

impl PlayerPrefs {
    fn sane(&self) -> bool {
        // 只做垃圾值过滤（第三方播放器/异常状态）；音量上限交给 mpv 的
        // volume-max 自己夹（有的用户会调高 volume-max，我们不知道）。
        self.window_scale.map(|s| (0.1..=20.0).contains(&s)).unwrap_or(true)
            && self.speed.map(|s| (0.01..=100.0).contains(&s)).unwrap_or(true)
            && self.volume.map(|v| (0.0..=1000.0).contains(&v)).unwrap_or(true)
    }
}

/// 把记忆还原成启动参数（纯函数，可测）。只恢复非默认项：
/// mute/maximized/fullscreen 默认就是 no/false，false 不下发，免得覆盖用户 env。
pub fn restore_args(p: &PlayerPrefs) -> Vec<String> {
    let mut out = Vec::new();
    if !p.sane() {
        return out;
    }
    if let (Some(x), Some(y)) = (p.x, p.y) {
        out.push(format!("--geometry=+{x}+{y}"));
    }
    if let Some(s) = p.window_scale {
        out.push(format!("--window-scale={s}"));
    }
    if p.maximized {
        out.push("--window-maximized=yes".into());
    }
    if p.fullscreen {
        out.push("--fullscreen=yes".into());
    }
    if let Some(v) = p.volume {
        out.push(format!("--volume={v}"));
    }
    if p.mute {
        out.push("--mute=yes".into());
    }
    if let Some(s) = p.speed {
        out.push(format!("--speed={s}"));
    }
    out
}

/// 一次要播的内容（单集）。
#[derive(Clone, Debug)]
pub struct PlayItem {
    pub url: String,
    pub referer: String,
    /// CENC 解密金钥（hex）；无加密流为 None。
    pub key: Option<String>,
    /// 展示标题（《剧名》第N集），进窗口标题与 OSC。
    pub title: String,
}

impl PlayItem {
    pub fn mpv_args(&self) -> Vec<String> {
        let mut args = vec![
            format!("--force-media-title={}", self.title),
            format!("--title={}", self.title),
        ];
        if !self.referer.is_empty() {
            args.push(format!("--referrer={}", self.referer));
        }
        if let Some(key) = &self.key {
            args.push(format!("--demuxer-lavf-o=decryption_key={key}"));
        }
        args
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PlayerKind {
    Mpv,
    Iina,
}

impl PlayerKind {
    fn program(self) -> &'static str {
        match self {
            PlayerKind::Mpv => "mpv",
            PlayerKind::Iina => "/Applications/IINA.app/Contents/MacOS/iina-cli",
        }
    }
    /// 该播放器的 mpv 选项前缀（IINA 用 `--mpv-xxx` 透传）。
    pub fn opt(self, name: &str) -> String {
        match self {
            PlayerKind::Mpv => format!("--{name}"),
            PlayerKind::Iina => format!("--mpv-{name}"),
        }
    }
}

/// 播放器解析：mpv 是全平台一致的默认。IINA **默认关闭**（装了 IINA 的用户
/// 不该在不知情下被换播放器），需显式 `GUO_PLAYER=iina` 开启；
/// `GUO_MPV` 优先级最高，可指向任意 mpv 兼容播放器。
/// 返回 (种类, 程序路径)；GUO_MPV 指向非标准路径时按 mpv 协议对待。
pub fn resolve_player() -> (PlayerKind, String) {
    if let Ok(custom) = std::env::var("GUO_MPV") {
        if !custom.trim().is_empty() {
            let kind = if custom.contains("iina") {
                PlayerKind::Iina
            } else {
                PlayerKind::Mpv
            };
            return (kind, custom);
        }
    }
    if std::env::var("GUO_PLAYER")
        .map(|v| v.eq_ignore_ascii_case("iina"))
        .unwrap_or(false)
    {
        return (PlayerKind::Iina, PlayerKind::Iina.program().to_string());
    }
    (PlayerKind::Mpv, PlayerKind::Mpv.program().to_string())
}

/// 会话内唯一递增的 IPC socket 名（多会话/重播不撞名）。
static SESSION_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn ipc_path() -> PathBuf {
    let n = SESSION_SEQ.fetch_add(1, Ordering::Relaxed);
    let name = format!("guo-mpv-{}-{n}", std::process::id());
    if cfg!(target_os = "windows") {
        std::env::temp_dir().join(name)
    } else {
        std::env::temp_dir().join(format!("{name}.sock"))
    }
}

/// 会话级播放器选项（每次 launch 都带上；与具体剧集无关）。
/// 独立成纯函数以便测试：缓冲/不闪退/IPC 三类缺一不可。
/// - 缓冲：串流 CDN 抖动是卡顿主因——起播先缓冲 3s（cache-pause-initial），
///   预读 60s/上限 128MiB；lavf reconnect 让 CDN 掐线时自动重连续播而不是卡死；
///   network-timeout 15s 兜底「永远 stall」的死流。
/// - keepalive（GUI 连播形态）：`--idle=yes --force-window=yes`——播完 eof 文件
///   正常卸载（end-file eof 事件照常发=最可靠的播完信号），但进程与窗口不退，
///   下一集 loadfile 打进同一窗口，不闪窗不重开。
///   实录：v0.3.2 曾用 `--keep-open=yes`，但其下 end-file 不发、eof-reached
///   属性通知在 mpv 0.41 断供（瞬态翻 true 即被重置，observe 六轮对照 0/6、
///   轮询追不到）——连播判完靠不住，废弃。
/// - input-ipc-server 由调用方按实例拼接，不在此列。
pub fn session_option_args(kind: PlayerKind, keepalive: bool) -> Vec<String> {
    let mut args = vec![
        format!("{}=auto", kind.opt("hwdec")),
        format!("{}=yes", kind.opt("cache")),
        format!("{}=2", kind.opt("cache-pause-wait")),
        // cache-pause-initial 是布尔开关，`--x=yes` 与 `--x` 等价，统一带 =yes 便于断言
        format!("{}=yes", kind.opt("cache-pause-initial")),
        format!("{}=60", kind.opt("demuxer-readahead-secs")),
        format!("{}=128MiB", kind.opt("demuxer-max-bytes")),
        format!("{}=64MiB", kind.opt("demuxer-max-back-bytes")),
        format!("{}=15", kind.opt("network-timeout")),
        format!(
            "{}=reconnect=1,reconnect_streamed=1,reconnect_delay_max=30",
            kind.opt("stream-lavf-o")
        ),
    ];
    if keepalive {
        args.push(format!("{}=yes", kind.opt("idle")));
        args.push(format!("{}=yes", kind.opt("force-window")));
        // 预取播放列表里的下一集（append 进来的）：播完瞬间切画面而非从零起播
        args.push(format!("{}=yes", kind.opt("prefetch-playlist")));
    }
    args
}

/// 「播完后要不要自动接下一集」的唯一判定（GUI 与测试共用）。
/// 返回 Some(下一集) 仅当：原因是自然播完 eof、开了连播、且还有下一集。
/// stop/quit/redirect 是我们自己换集/退出引起的，绝不推进。
pub fn autoplay_next_ep(reason: &str, autoplay: bool, ep: usize, total: usize) -> Option<usize> {
    if reason != "eof" || !autoplay || ep >= total {
        return None;
    }
    Some(ep + 1)
}

/// 一个存活的播放会话。
pub struct PlayerSession {
    pub ipc: PathBuf,
    pub kind: PlayerKind,
    child: Child,
    /// 会话级停机旗：quit/drop 置 false，事件读取线程见 false 即退出
    /// （不再依赖 GUI 侧共享旗——旧线程曾因旗永不落零而空转泄漏）。
    running: Arc<AtomicBool>,
}

/// 按 pid 抓窗口 bounds（左上角原点全局坐标）——只有操作系统知道窗口在哪，
/// mpv 本身不暴露位置属性。macOS 走 CoreGraphics 窗口列表（最小 FFI，零新依赖）；
/// 其余平台暂无零依赖取法 → None = 不恢复位置（大小等其余设定照常记忆）。
#[cfg(target_os = "macos")]
pub fn window_bounds_for_pid(pid: u32) -> Option<(i32, i32, u32, u32)> {
    macos_win::bounds_for_pid(pid)
}

#[cfg(not(target_os = "macos"))]
pub fn window_bounds_for_pid(_pid: u32) -> Option<(i32, i32, u32, u32)> {
    None
}

#[cfg(target_os = "macos")]
mod macos_win {
    //! CoreGraphics 窗口列表最小 FFI：kCGWindowListOptionOnScreenOnly |
    //! ExcludeDesktopElements，按 owner pid 命中最大面积窗口取 kCGWindowBounds。
    use std::collections::HashMap;
    use std::os::raw::c_void;
    use std::sync::OnceLock;

    type CFRef = *const c_void;

    /// CF 裸指针句柄包装：CoreFoundation 对象是线程安全的（文档明示 CF 层
    /// thread-safe），裸指针类型本身不标 Send/Sync，这里替编译器作证。
    #[derive(Clone, Copy)]
    struct CfHandle(CFRef);
    unsafe impl Send for CfHandle {}
    unsafe impl Sync for CfHandle {}

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to: CFRef) -> CFRef;
        static kCGWindowOwnerPID: CFRef;
        static kCGWindowBounds: CFRef;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(arr: CFRef) -> isize;
        fn CFArrayGetValueAtIndex(arr: CFRef, idx: isize) -> CFRef;
        fn CFDictionaryGetValue(dict: CFRef, key: CFRef) -> CFRef;
        fn CFNumberGetValue(num: CFRef, the_type: isize, value: *mut c_void) -> bool;
        fn CFStringCreateWithCString(
            alloc: CFRef,
            c_str: *const std::os::raw::c_char,
            encoding: u32,
        ) -> CFRef;
        fn CFRelease(cf: CFRef);
    }

    const LIST_OPTS: u32 = 1 | 2; // OnScreenOnly | ExcludeDesktopElements
    const K_CF_NUMBER_SINT32: isize = 3;
    const K_CF_NUMBER_FLOAT64: isize = 6;
    const K_CF_STRING_UTF8: u32 = 0x0800_0100;
    const K_CF_ALLOCATOR_NULL: CFRef = std::ptr::null();

    fn cfstr(s: &'static str) -> CFRef {
        let c = std::ffi::CString::new(s).expect("cfstr");
        unsafe { CFStringCreateWithCString(K_CF_ALLOCATOR_NULL, c.as_ptr(), K_CF_STRING_UTF8) }
    }

    fn number_f64(v: CFRef) -> Option<f64> {
        if v.is_null() {
            return None;
        }
        let mut out: f64 = 0.0;
        unsafe {
            CFNumberGetValue(v, K_CF_NUMBER_FLOAT64, &mut out as *mut f64 as *mut c_void)
                .then_some(out)
        }
    }

    /// 缓存 bounds 子键与固定键（"X"/"Y"/"Width"/"Height"——CoreGraphics 文档
    /// 的字面名；kCGWindowOwnerPID/kCGWindowBounds 是导出的常量）。
    fn keys() -> &'static HashMap<&'static str, CfHandle> {
        static KEYS: OnceLock<HashMap<&'static str, CfHandle>> = OnceLock::new();
        KEYS.get_or_init(|| {
            let mut m = HashMap::new();
            for k in ["X", "Y", "Width", "Height"] {
                m.insert(k, CfHandle(cfstr(k)));
            }
            unsafe {
                m.insert("owner_pid", CfHandle(kCGWindowOwnerPID));
                m.insert("bounds", CfHandle(kCGWindowBounds));
            }
            m
        })
    }

    pub fn bounds_for_pid(pid: u32) -> Option<(i32, i32, u32, u32)> {
        let keys = keys();
        unsafe {
            let list = CGWindowListCopyWindowInfo(LIST_OPTS, std::ptr::null());
            if list.is_null() {
                return None;
            }
            let n = CFArrayGetCount(list);
            let mut best: Option<(f64, (i32, i32, u32, u32))> = None;
            for i in 0..n {
                let dict = CFArrayGetValueAtIndex(list, i);
                if dict.is_null() {
                    continue;
                }
                let mut owner: i32 = 0;
                let owner_ref = CFDictionaryGetValue(dict, keys["owner_pid"].0);
                if owner_ref.is_null()
                    || !CFNumberGetValue(
                        owner_ref,
                        K_CF_NUMBER_SINT32,
                        &mut owner as *mut i32 as *mut c_void,
                    )
                    || owner != pid as i32
                {
                    continue;
                }
                let bounds = CFDictionaryGetValue(dict, keys["bounds"].0);
                if bounds.is_null() {
                    continue;
                }
                let (Some(x), Some(y), Some(w), Some(h)) = (
                    number_f64(CFDictionaryGetValue(bounds, keys["X"].0)),
                    number_f64(CFDictionaryGetValue(bounds, keys["Y"].0)),
                    number_f64(CFDictionaryGetValue(bounds, keys["Width"].0)),
                    number_f64(CFDictionaryGetValue(bounds, keys["Height"].0)),
                ) else {
                    continue;
                };
                // 同 pid 可能多窗（不可见于本列表的除外）：取面积最大的主窗
                let area = w * h;
                if best.map(|(a, _)| area > a).unwrap_or(true) {
                    best = Some((area, (x as i32, y as i32, w as u32, h as u32)));
                }
            }
            CFRelease(list);
            best.map(|(_, b)| b)
        }
    }
}

impl PlayerSession {
    /// 启动播放器并播放第一集。stderr 收进 `log_path`。
    /// `keepalive`=GUI 连播形态（idle+force-window：eof 后进程/窗口存活，接集
    /// 不闪窗）；CLI 一次性播放传 false（播完自然退出）。
    /// `prefs`=上次会话记忆（窗口位置/大小/音量等），空 Default=不下发恢复参数。
    /// 追加参数:`GUO_MPV_ARGS`(空格分隔;测试用 --vo=null,用户可自定义)——
    /// 它排在恢复参数之后，用户的 env 永远赢过我们记忆的值。
    pub fn launch(
        kind: PlayerKind,
        program: &str,
        first: &PlayItem,
        ipc: &Path,
        log_path: &Path,
        keepalive: bool,
        prefs: &PlayerPrefs,
    ) -> std::io::Result<PlayerSession> {
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let log = std::fs::File::create(log_path)?;
        let mut cmd = Command::new(program);
        // mpv 部分选项（input-ipc-server 等）只认 = 形态，不吃空格分离，全部用 = 传递。
        // （IINA 透传见 render_args；此处 opt() 前缀 + = 合并，iina-cli 也接受 --mpv-x=v。）
        cmd.arg(format!("{}={}", kind.opt("input-ipc-server"), ipc.display()))
            .args(render_args(kind, session_option_args(kind, keepalive)))
            .args(render_args(kind, restore_args(prefs)))
            .args(render_args(kind, first.mpv_args()))
            .args(
                std::env::var("GUO_MPV_ARGS")
                    .unwrap_or_default()
                    .split_whitespace()
                    .map(str::to_string)
                    .collect::<Vec<String>>(),
            )
            .arg(&first.url)
            .stdin(Stdio::null())
            .stderr(Stdio::from(log));
        let child = cmd.spawn()?;
        Ok(PlayerSession {
            ipc: ipc.to_path_buf(),
            kind,
            child,
            running: Arc::new(AtomicBool::new(true)),
        })
    }

    pub fn from_existing(ipc: PathBuf, kind: PlayerKind, child: Child) -> PlayerSession {
        PlayerSession { ipc, kind, child, running: Arc::new(AtomicBool::new(true)) }
    }

    /// 事件读取线程用的停机旗（每会话独立，quit 后 reader 自行退出）。
    pub fn running_flag(&self) -> Arc<AtomicBool> {
        self.running.clone()
    }

    pub fn child_id(&self) -> u32 {
        self.child.id()
    }

    /// 捕获当前窗口与播放设定（音量/静音/倍速/全屏/最大化/窗口缩放 + macOS
    /// 位置）。逐项 IPC 查询，任何一项拿不到就保持 None/false——部分失败不
    /// 连坐。GUI 在每集开播/播完与窗口关闭时调用，存回 state.json。
    pub fn capture_prefs(&self) -> PlayerPrefs {
        Self::capture_ipc_prefs(&self.ipc, self.child.id())
    }

    /// 同上，但 ipc/pid 由调用方给（事件读取线程只有这两个，不持有会话）。
    pub fn capture_ipc_prefs(ipc: &Path, pid: u32) -> PlayerPrefs {
        let get = |conn: &mut IpcConn, name: &str| -> Option<serde_json::Value> {
            let reply = send_cmd_with_reply(
                conn,
                &format!("{{\"command\": [\"get_property\", \"{name}\"]}}\n"),
            )
            .ok()?;
            let v: serde_json::Value = serde_json::from_str(reply.trim()).ok()?;
            // 出错回包没有 data 字段 → None（property unavailable 等不连坐）
            v.get("data").filter(|d| !d.is_null()).cloned()
        };
        let mut p = PlayerPrefs::default();
        if let Ok(mut conn) = open_ipc(ipc) {
            p.volume = get(&mut conn, "volume").and_then(|v| v.as_f64());
            p.mute = get(&mut conn, "mute").and_then(|v| v.as_bool()).unwrap_or(false);
            p.speed = get(&mut conn, "speed").and_then(|v| v.as_f64());
            p.fullscreen = get(&mut conn, "fullscreen").and_then(|v| v.as_bool()).unwrap_or(false);
            p.maximized = get(&mut conn, "window-maximized")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            p.window_scale = get(&mut conn, "current-window-scale").and_then(|v| v.as_f64());
        }
        if let Some((x, y, _, _)) = window_bounds_for_pid(pid) {
            p.x = Some(x);
            p.y = Some(y);
        }
        if !p.sane() {
            // 数值越界（第三方播放器/异常状态）→ 宁可不恢复，只保留拿得到的窗口位置
            let pos = (p.x, p.y);
            p = PlayerPrefs { x: pos.0, y: pos.1, ..Default::default() };
        }
        p
    }

    /// 更新窗口标题/OSC 显示的集名（连播自动切换时就地推进，不等下一条命令）。
    pub fn set_title(&self, title: &str) -> std::io::Result<()> {
        let mut conn = open_ipc(&self.ipc)?;
        let reply = send_cmd_with_reply(
            &mut conn,
            &format!("{{\"command\": [\"set_property\", \"force-media-title\", {:?}]}}\n", title),
        )?;
        if !reply.contains("\"error\":\"success\"") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("set_property 被拒绝：{}", reply.trim()),
            ));
        }
        Ok(())
    }

    /// 进程是否还活着（探测，不收割 zombies 之外的语义）。
    pub fn process_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// IPC 连接可达？（进程在但 IPC 还没就绪时返回 false。）
    pub fn ipc_ready(&self) -> bool {
        open_ipc(&self.ipc).is_ok()
    }

    /// 换集（replace 当前播放），并同步更新标题。
    /// mpv 在起播/缓冲忙碌期对 IPC 命令的回执可能 >5s（实测偶发）——超时类
    /// 错误自动重试（每次全新连接），被拒绝类错误当场返回不重试。
    pub fn load(&self, item: &PlayItem) -> std::io::Result<()> {
        Self::with_retry(|| Self::load_once_at(&self.ipc, item, "replace"))
    }

    /// 预载下一集进播放列表（append）：当前集自然播完时 mpv **内部直接切换**
    /// 到已预载的下一集——无 IPC 往返、无黑屏等待，这是连播「连续感」的根基；
    /// 配合 `--prefetch-playlist=yes`（连播形态默认开）还会提前缓冲下一集。
    /// per-file 的 referrer/解密金钥随条目保存，切换后照常生效。
    pub fn append(&self, item: &PlayItem) -> std::io::Result<()> {
        Self::with_retry(|| Self::load_once_at(&self.ipc, item, "append"))
    }

    /// 清空播放列表中除当前播放外的全部条目（丢弃预载/过期的「下一集」）。
    /// 手动点集（loadfile replace 前必须调）与关连播时防止陈旧条目自动接播。
    pub fn playlist_clear(&self) -> std::io::Result<()> {
        Self::with_retry(|| {
            let mut conn = open_ipc(&self.ipc)?;
            let reply = send_cmd_with_reply(&mut conn, "{\"command\": [\"playlist-clear\"]}\n")?;
            if !reply.contains("\"error\":\"success\"") {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("playlist-clear 被拒绝：{}", reply.trim()),
                ));
            }
            Ok(())
        })
    }

    fn with_retry(mut op: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
        let mut last_err: Option<std::io::Error> = None;
        for _ in 0..3 {
            match op() {
                Ok(()) => return Ok(()),
                // WouldBlock/TimedOut=mpv 忙碌期回执慢；NotFound=socket 尚未建好
                //（本地缓存命中时 resolve 可能快过 mpv 初始化）——都值得重试
                Err(e) if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::NotFound
                ) => {
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(300));
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| std::io::Error::other("load 重试耗尽")))
    }

    /// loadfile 命令构造（replace=换集 / append=预载）。referer/key 属 demuxer
    /// 级选项，随 per-file 选项（第 5 参，mpv ≥0.37）一起走，切换后照常生效。
    fn loadfile_cmd(item: &PlayItem, mode: &str) -> String {
        let mut opts: Vec<String> = Vec::new();
        if !item.referer.is_empty() {
            opts.push(format!("referrer={}", item.referer));
        }
        if let Some(key) = &item.key {
            opts.push(format!("demuxer-lavf-o=decryption_key={key}"));
        }
        if opts.is_empty() {
            format!("{{\"command\": [\"loadfile\", {:?}, \"{mode}\"]}}\n", item.url)
        } else {
            format!(
                "{{\"command\": [\"loadfile\", {:?}, \"{mode}\", -1, {:?}]}}\n",
                item.url,
                opts.join(",")
            )
        }
    }

    fn load_once_at(ipc: &Path, item: &PlayItem, mode: &str) -> std::io::Result<()> {
        let mut conn = open_ipc(ipc)?;
        // 标题只在 replace 时同步（append 的下一集标题由 GUI 在换集时机更新）
        if mode == "replace" {
            let title_reply = send_cmd_with_reply(&mut conn, &format!(
                "{{\"command\": [\"set_property\", \"force-media-title\", {:?}]}}\n",
                item.title
            ))?;
            if !title_reply.contains("\"error\":\"success\"") {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("set_property 被拒绝：{}", title_reply.trim()),
                ));
            }
        }
        let load = Self::loadfile_cmd(item, mode);
        let load_reply = send_cmd_with_reply(&mut conn, &load)?;
        if !load_reply.contains("\"error\":\"success\"") {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("loadfile 被拒绝：{}", load_reply.trim()),
            ));
        }
        Ok(())
    }

    /// 等待播放器自然退出（CLI 一次性播放用）。
    pub fn child_wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait()
    }

    /// 礼貌退出（IPC quit；失败再由调用方 kill 进程）。
    pub fn quit(&mut self) {
        self.running.store(false, Ordering::Relaxed);
        if let Ok(mut conn) = open_ipc(&self.ipc) {
            let _ = send_cmd(&mut conn, "{\"command\": [\"quit\"]}\n");
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.ipc);
    }
}

impl Drop for PlayerSession {
    fn drop(&mut self) {
        // GUI 关闭时别留下孤儿播放器窗口。
        self.quit();
    }
}

/// 渲染启动参数。IINA 的透传形態是 `--mpv-<名> <值>`（空格分隔、值不含 =），
/// 所以 `--name=value` 要拆成两个 argv。
fn render_args(kind: PlayerKind, args: Vec<String>) -> Vec<String> {
    match kind {
        PlayerKind::Mpv => args,
        PlayerKind::Iina => {
            let mut out = Vec::new();
            for arg in args {
                if let Some(rest) = arg.strip_prefix("--") {
                    match rest.split_once('=') {
                        Some((name, value)) => {
                            out.push(format!("--mpv-{name}"));
                            out.push(value.to_string());
                        }
                        None => out.push(format!("--mpv-{rest}")),
                    }
                } else {
                    out.push(arg);
                }
            }
            out
        }
    }
}

/// 连接 IPC：Unix = UnixStream；Windows = 以文件方式打开命名管道。
enum IpcConn {
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
    #[cfg(windows)]
    Win(std::fs::File),
}

impl Write for IpcConn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            IpcConn::Unix(s) => s.write(buf),
            #[cfg(windows)]
            IpcConn::Win(f) => f.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            IpcConn::Unix(s) => s.flush(),
            #[cfg(windows)]
            IpcConn::Win(f) => f.flush(),
        }
    }
}

impl Read for IpcConn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            #[cfg(unix)]
            IpcConn::Unix(s) => s.read(buf),
            #[cfg(windows)]
            IpcConn::Win(f) => f.read(buf),
        }
    }
}

impl IpcConn {
    fn set_read_timeout_secs(&self, secs: u64) -> std::io::Result<()> {
        match self {
            #[cfg(unix)]
            IpcConn::Unix(s) => s.set_read_timeout(Some(std::time::Duration::from_secs(secs))),
            #[cfg(windows)]
            IpcConn::Win(_) => Ok(()),
        }
    }
}

fn open_ipc(path: &Path) -> std::io::Result<IpcConn> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;
        let conn = UnixStream::connect(path)?;
        conn.set_nonblocking(false).ok();
        Ok(IpcConn::Unix(conn))
    }
    #[cfg(windows)]
    {
        let pipe = format!("\\\\.\\pipe\\{}", path.file_name().and_then(|s| s.to_str()).unwrap_or("guo-mpv"));
        let f = std::fs::OpenOptions::new().read(true).write(true).open(pipe)?;
        Ok(IpcConn::Win(f))
    }
}

fn send_cmd(conn: &mut IpcConn, line: &str) -> std::io::Result<()> {
    conn.write_all(line.as_bytes())
}

/// 发送并读回执（mpv 对每条命令回一行 {"error":...}）；被拒绝时可见具体原因。
/// 5s 读超时：mpv 不回时报错而非永久挂起（事件循环连接不受影响，各自独立）。
fn send_cmd_with_reply(conn: &mut IpcConn, line: &str) -> std::io::Result<String> {
    conn.set_read_timeout_secs(5)?;
    conn.write_all(line.as_bytes())?;
    let mut reply = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = conn.read(&mut byte)?;
        if n == 0 {
            break; // 连接关闭
        }
        reply.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    conn.set_read_timeout_secs(u64::MAX / 2)?;
    Ok(String::from_utf8_lossy(&reply).into_owned())
}

/// 事件读取线程的循环体：阻塞读一行，回调给调用方；返回 false = 断线结束。
/// 独立成函数以便测试（喂字符串断言事件解析）。
pub fn handle_event_line(line: &str) -> Option<PlayerEvent> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let event = v.get("event")?.as_str()?;
    match event {
        "end-file" => {
            let reason = v.get("reason").and_then(|r| r.as_str()).unwrap_or("unknown");
            Some(match reason {
                // eof = 自然播完（idle 会话下照常发）；stop/quit/redirect =
                // 我们 loadfile 换集或退出引起的，不是播完，调用方要能区分。
                "eof" => PlayerEvent::Ended,
                "stop" | "quit" | "redirect" => PlayerEvent::Replaced,
                other => PlayerEvent::Failed(other.to_string()),
            })
        }
        "start-file" => Some(PlayerEvent::Started),
        "file-loaded" => Some(PlayerEvent::FileLoaded),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlayerEvent {
    Started,
    /// 文件加载完成（file-loaded；比 start-file 晚，IPC 客户端迟到也能收到）。
    FileLoaded,
    /// 自然播完（end-file reason=eof）。
    Ended,
    /// 我们 loadfile 换集 / 退出引起的 end-file（stop|quit|redirect）——不是播完。
    Replaced,
    Failed(String),
}

/// 事件循环（供专用线程调用）：持久连接逐行读事件，回调给调用方；
/// 断线自动重连。`running` 清零或回调返回 false 时结束。
/// `GUO_MPV_EVENT_DEBUG=1` 时把每行原始事件打到 stderr（诊断事件链路用）。
/// 播完信号=end-file eof（idle 会话下照常发，见 session_option_args 注释）。
pub fn event_loop(
    ipc: &Path,
    running: &std::sync::atomic::AtomicBool,
    mut on_event: impl FnMut(PlayerEvent) -> bool,
) {
    use std::sync::atomic::Ordering;
    let debug = std::env::var("GUO_MPV_EVENT_DEBUG").map(|v| v == "1").unwrap_or(false);
    while running.load(Ordering::Relaxed) {
        let Ok(mut conn) = open_ipc(ipc) else {
            std::thread::sleep(std::time::Duration::from_millis(300));
            continue;
        };
        if debug {
            eprintln!("[ev-loop] connected");
        }
        let mut reader = BufReader::new(conn);
        loop {
            if !running.load(Ordering::Relaxed) {
                return;
            }
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break, // 连接断开 → 重连
                Ok(_) => {
                    if debug {
                        eprintln!("[ev-loop] {}", line.trim_end());
                    }
                    if let Some(event) = handle_event_line(&line) {
                        if !on_event(event) {
                            return;
                        }
                    }
                }
            }
        }
        if debug {
            eprintln!("[ev-loop] conn lost → 重连");
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// 读 stderr 日志的尾 N 行（启动失败时给用户看真正的「怪讯息」）。
pub fn tail_lines(path: &Path, n: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else { return String::new() };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_args_cover_buffer_keepopen_ipc() {
        for kind in [PlayerKind::Mpv, PlayerKind::Iina] {
            let args = session_option_args(kind, true);
            let has = |prefix: &str| args.iter().any(|a| a.starts_with(prefix));
            // 缓冲三件套：起播缓冲 + 预读上限 + 断线重连/超时
            assert!(has(&format!("{}=yes", kind.opt("cache"))), "{args:?}");
            assert!(has(&format!("{}=yes", kind.opt("cache-pause-initial"))), "{args:?}");
            assert!(has(&format!("{}=2", kind.opt("cache-pause-wait"))), "{args:?}");
            assert!(has(&format!("{}=60", kind.opt("demuxer-readahead-secs"))), "{args:?}");
            assert!(has(&format!("{}=128MiB", kind.opt("demuxer-max-bytes"))), "{args:?}");
            assert!(has(&format!("{}=reconnect=1", kind.opt("stream-lavf-o"))), "{args:?}");
            assert!(has(&format!("{}=15", kind.opt("network-timeout"))), "{args:?}");
            // 连播形态：eof 后进程/窗口存活，end-file eof 事件照常发
            assert!(has(&format!("{}=yes", kind.opt("idle"))), "{args:?}");
            assert!(has(&format!("{}=yes", kind.opt("force-window"))), "{args:?}");
            // 预载下一集的预取（连续感的另一半：提前缓冲）
            assert!(has(&format!("{}=yes", kind.opt("prefetch-playlist"))), "{args:?}");
            assert!(!has("keep-open"), "keep-open 方案已废弃（end-file 不发）: {args:?}");
            assert!(has(&format!("{}=auto", kind.opt("hwdec"))), "{args:?}");
            // IPC/剧集参数不属于会话级选项
            assert!(!has("input-ipc-server"), "{args:?}");
            // CLI 一次性形态：不带 idle/force-window（播完自然退出）
            let cli = session_option_args(kind, false);
            assert!(!cli.iter().any(|a| a.contains("idle") || a.contains("force-window")), "{cli:?}");
        }
        // IINA 透传形态：--mpv-<名> <值> 拆分由 render_args 负责，这里确认前缀已带
        let iina = session_option_args(PlayerKind::Iina, true);
        assert!(iina.iter().all(|a| a.starts_with("--mpv-")), "{iina:?}");
    }

    #[test]
    fn autoplay_decision_matrix() {
        // 自然播完 + 开连播 + 有下一集 → 接下一集
        assert_eq!(autoplay_next_ep("eof", true, 3, 12), Some(4));
        // 最后一级 → 不再推进
        assert_eq!(autoplay_next_ep("eof", true, 12, 12), None);
        // 没开连播 → 不推进
        assert_eq!(autoplay_next_ep("eof", false, 3, 12), None);
        // 自己换集/退出（stop|quit|redirect）绝不推进
        for reason in ["stop", "quit", "redirect"] {
            assert_eq!(autoplay_next_ep(reason, true, 3, 12), None, "{reason}");
        }
        // 播放失败 → 不推进
        assert_eq!(autoplay_next_ep("error", true, 3, 12), None);
    }

    #[test]
    fn restore_args_full_empty_and_insane() {
        // 空 = 不下发任何恢复参数（首启无记忆）
        assert!(restore_args(&PlayerPrefs::default()).is_empty());
        // 全量记忆 → 各参数逐一在场；mute/maximized/fullscreen=false 不下发
        let p = PlayerPrefs {
            x: Some(-1920),
            y: Some(-80),
            window_scale: Some(1.5),
            maximized: true,
            fullscreen: false,
            volume: Some(42.0),
            mute: false,
            speed: Some(1.25),
        };
        let args = restore_args(&p);
        assert!(args.contains(&"--geometry=+-1920+-80".to_string()), "{args:?}");
        assert!(args.contains(&"--window-scale=1.5".to_string()), "{args:?}");
        assert!(args.contains(&"--window-maximized=yes".to_string()), "{args:?}");
        assert!(!args.iter().any(|a| a.contains("fullscreen")), "{args:?}");
        assert!(args.contains(&"--volume=42".to_string()), "{args:?}");
        assert!(!args.iter().any(|a| a.starts_with("--mute")), "{args:?}");
        assert!(args.contains(&"--speed=1.25".to_string()), "{args:?}");
        // 静音/全屏为 true 时才下发
        let args = restore_args(&PlayerPrefs { mute: true, fullscreen: true, ..Default::default() });
        assert!(args.contains(&"--mute=yes".to_string()) && args.contains(&"--fullscreen=yes".to_string()));
        // 越界值（第三方播放器/异常）→ 整包拒绝，宁可不恢复
        let insane = PlayerPrefs { window_scale: Some(9999.0), volume: Some(100.0), ..Default::default() };
        assert!(restore_args(&insane).is_empty());
        let insane = PlayerPrefs { volume: Some(9999.0), ..Default::default() };
        assert!(restore_args(&insane).is_empty());
        // 调高过 volume-max 的合法音量直通（mpv 自己按 volume-max 夹）
        let loud = PlayerPrefs { volume: Some(140.0), ..Default::default() };
        assert_eq!(restore_args(&loud), vec!["--volume=140".to_string()]);
    }

    #[test]
    fn window_bounds_probe_returns_gracefully() {
        // pid 0（内核）无窗口 → None 而非崩溃/挂起（macOS FFI 链路烟测）
        assert_eq!(window_bounds_for_pid(0), None);
    }

    #[test]
    fn loadfile_cmd_modes_and_per_file_opts() {
        let item = PlayItem {
            url: "https://x/v".into(),
            referer: "https://r/".into(),
            key: Some("aabb".into()),
            title: "《剧》第2集".into(),
        };
        // replace 与 append 只差模式字串；per-file 选项两条路都带
        for mode in ["replace", "append"] {
            let cmd = PlayerSession::loadfile_cmd(&item, mode);
            assert!(cmd.contains(&format!("\"{mode}\"")), "{cmd}");
            assert!(cmd.contains("referrer=https://r/"), "{cmd}");
            assert!(cmd.contains("decryption_key=aabb"), "{cmd}");
            assert!(cmd.contains("-1"), "{cmd}");
        }
        // 无 per-file 选项时走短形态
        let bare = PlayItem { url: "u".into(), referer: String::new(), key: None, title: "t".into() };
        let cmd = PlayerSession::loadfile_cmd(&bare, "append");
        assert!(cmd.starts_with("{\"command\": [\"loadfile\", \"u\", \"append\"]}"), "{cmd}");
    }

    #[test]
    fn event_parsing() {
        assert_eq!(
            handle_event_line("{\"event\":\"end-file\",\"reason\":\"eof\"}"),
            Some(PlayerEvent::Ended)
        );
        assert_eq!(
            handle_event_line("{\"event\":\"end-file\",\"reason\":\"error\"}"),
            Some(PlayerEvent::Failed("error".into()))
        );
        // 自己换集引起的 stop ≠ 播完
        assert_eq!(
            handle_event_line("{\"event\":\"end-file\",\"reason\":\"stop\"}"),
            Some(PlayerEvent::Replaced)
        );
        assert_eq!(handle_event_line("{\"event\":\"start-file\"}"), Some(PlayerEvent::Started));
        assert_eq!(
            handle_event_line("{\"event\":\"file-loaded\"}"),
            Some(PlayerEvent::FileLoaded)
        );
        // mpv 的事件流混着大量无关事件（seek、metadata-update、属性变更…），都必须被忽略
        assert_eq!(
            handle_event_line(
                "{\"event\":\"property-change\",\"id\":1,\"name\":\"eof-reached\",\"data\":true}"
            ),
            None
        );
        assert_eq!(handle_event_line("{\"event\":\"seek\"}"), None);
        assert_eq!(handle_event_line("not json"), None);
    }

    #[test]
    fn item_args_cover_title_key_referer() {
        let item = PlayItem {
            url: "https://x/video".into(),
            referer: "https://novel.snssdk.com/".into(),
            key: Some("aabb".into()),
            title: "《剧》第2集".into(),
        };
        let args = item.mpv_args();
        assert!(args.contains(&"--force-media-title=《剧》第2集".to_string()));
        assert!(args.contains(&"--title=《剧》第2集".to_string()));
        assert!(args.iter().any(|a| a.starts_with("--referrer=")));
        assert!(args.iter().any(|a| a.starts_with("--demuxer-lavf-o=decryption_key=aabb")));
    }

    #[test]
    fn iina_args_split_on_first_eq() {
        let out = render_args(PlayerKind::Iina, vec![
            "--force-media-title=《剧》第2集".into(),
            "--hwdec".into(),
        ]);
        assert_eq!(out, vec!["--mpv-force-media-title", "《剧》第2集", "--mpv-hwdec"]);
        let out = render_args(PlayerKind::Mpv, vec!["--force-media-title=x".into()]);
        assert_eq!(out, vec!["--force-media-title=x"]);
    }

    #[test]
    fn resolve_player_config_precedence() {
        // env 是进程级全局:两个场景放同一个测试里顺序验证,避免并行线程互踩
        // ① GUO_MPV 优先级最高
        std::env::set_var("GUO_MPV", "/usr/local/bin/mpv");
        std::env::set_var("GUO_PLAYER", "iina");
        let (kind, program) = resolve_player();
        assert_eq!(kind, PlayerKind::Mpv);
        assert_eq!(program, "/usr/local/bin/mpv");
        std::env::remove_var("GUO_MPV");
        // ② GUO_PLAYER=iina 显式开启 IINA
        let (kind, program) = resolve_player();
        assert_eq!(kind, PlayerKind::Iina);
        assert!(program.contains("iina-cli"));
        std::env::remove_var("GUO_PLAYER");
        // ③ 都没设 → mpv 默认(IINA 默认关闭)
        let (kind, _) = resolve_player();
        assert_eq!(kind, PlayerKind::Mpv);
    }}
