//! 串流播放会话：单实例播放器 + JSON IPC（mpv 协议，跨平台）。
//!
//! 设计：
//! - 会话只 spawn 一次；换集走 IPC `loadfile replace`，不再每集开新窗口
//!   （旧行为每集一个进程 = 多窗口闪跳的来源之一）。
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
use std::sync::atomic::{AtomicU64, Ordering};

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

/// 一个存活的播放会话。
pub struct PlayerSession {
    pub ipc: PathBuf,
    pub kind: PlayerKind,
    child: Child,
}

impl PlayerSession {
    /// 启动播放器并播放第一集。stderr 收进 `log_path`。
    /// 追加参数:`GUO_MPV_ARGS`(空格分隔;测试用 --vo=null,用户可自定义)。
    pub fn launch(
        kind: PlayerKind,
        program: &str,
        first: &PlayItem,
        ipc: &Path,
        log_path: &Path,
    ) -> std::io::Result<PlayerSession> {
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let log = std::fs::File::create(log_path)?;
        let mut cmd = Command::new(program);
        // mpv 部分选项（input-ipc-server 等）只认 = 形态，不吃空格分离，全部用 = 传递。
        // （IINA 透传见 render_args；此处 opt() 前缀 + = 合并，iina-cli 也接受 --mpv-x=v。）
        cmd.arg(format!("{}=auto", kind.opt("hwdec")))
            .arg(format!("{}={}", kind.opt("input-ipc-server"), ipc.display()))
            // 预读 30s：直播源网络抖动时少卡顿跳帧
            .arg(format!("{}=30", kind.opt("demuxer-readahead-secs")))
            .arg(format!("{}=64MiB", kind.opt("demuxer-max-bytes")))
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
        Ok(PlayerSession { ipc: ipc.to_path_buf(), kind, child })
    }

    pub fn from_existing(ipc: PathBuf, kind: PlayerKind, child: Child) -> PlayerSession {
        PlayerSession { ipc, kind, child }
    }

    pub fn child_id(&self) -> u32 {
        self.child.id()
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
    pub fn load(&self, item: &PlayItem) -> std::io::Result<()> {
        let mut conn = open_ipc(&self.ipc)?;
        // 标题先 set 再 loadfile：命令在 mpv 侧按序执行。
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
        let mut load = format!(
            "{{\"command\": [\"loadfile\", {:?}, \"replace\"]}}\n",
            item.url
        );
        // referer/key 属于 demuxer 级选项，换集时随文件一起换
        // （loadfile 第 4 参 = per-file 选项，mpv ≥0.37）。
        let mut opts: Vec<String> = Vec::new();
        if !item.referer.is_empty() {
            opts.push(format!("referrer={}", item.referer));
        }
        if let Some(key) = &item.key {
            opts.push(format!(
                "demuxer-lavf-o=decryption_key={key}"
            ));
        }
        if !opts.is_empty() {
            load = format!(
                "{{\"command\": [\"loadfile\", {:?}, \"replace\", -1, {:?}]}}\n",
                item.url,
                opts.join(",")
            );
        }
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
                // eof = 自然播完；stop/quit/redirect = 我们 loadfile 换集或退出引起的，
                // 不是播完，调用方要能区分。
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
/// 断线自动重连（mpv 端到端生命周期内不丢事件）。`running` 清零或回调返回 false 时结束。
pub fn event_loop(
    ipc: &Path,
    running: &std::sync::atomic::AtomicBool,
    mut on_event: impl FnMut(PlayerEvent) -> bool,
) {
    use std::sync::atomic::Ordering;
    while running.load(Ordering::Relaxed) {
        let Ok(conn) = open_ipc(ipc) else {
            std::thread::sleep(std::time::Duration::from_millis(500));
            continue;
        };
        let mut reader = BufReader::new(conn);
        loop {
            if !running.load(Ordering::Relaxed) {
                return;
            }
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break, // 连接断开 → 重连
                Ok(_) => {
                    if let Some(event) = handle_event_line(&line) {
                        if !on_event(event) {
                            return;
                        }
                    }
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
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
        // mpv 的事件流混着大量无关事件（seek、metadata-update…），都必须被忽略
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
