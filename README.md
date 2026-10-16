# guoapp

**在电脑上追红果短剧的桌面客户端**：封面卡片墙浏览 / 搜索 / 在线播放 / 离线下载，
含 **GUI（egui 原生窗口）与 CLI** 两套界面，Rust 从零实现：
release 下 CLI 约 **1.5 MB**、GUI 约 **4.0 MB**，单文件、无运行时依赖。

**三种看片方式**

1. **看片优先**：封面悬停出「▶ 播放/续看」一键开播；详情页默认**看片模式**
   （大播放按钮 + 点击集数即播 + 看过的集变灰），下载收进独立的「⬇ 下载」模式页
2. **追更提醒**：看过的剧出了新集，卡片左上角挂「新集 +N」红标，
   「继续观看」列同步标 🔔
3. **分类即过滤**：顶部「显示：全部/真人剧/漫剧/AI剧」点谁只显示谁
   （旧行为三类堆一页），排序独立于分类自由组合
1. **目录按热度排**：排序切换「最新上线 / 最热门 / 最多观看」——服务端排序
   加客户端稳定序，卡片直接显示观看数（94.7万 这种）；选择跨启动记忆
1. **在线播放**：不用下载，选「▶ 播放第 1 集」直接流式播放——自动解出 CENC
   加密金钥交给 mpv（`mpv --demuxer-lavf-o=decryption_key=…`），逐帧与
   本地解密文件一致（2026-10-12 实测）
2. **离线下载**：详情页切「⬇ 下载」模式，默认全选、右键（看片模式）或点击
   （下载模式）勾/取消，可逐集 ✕ 取消；下载后自动解密成**普通 mp4**，
   任何播放器可放、可剪、可离线
4. **每部剧独立播放记录 + 精确续看**：看过哪集记在哪部剧名下（互不共用）；
   每 5 秒取样集内位置——「▶ 续看第N集（从 12:30）」从上次中断的秒数继续；
   看过的集数格变灰；播放中关掉详情面板，底栏播放条照样能切「下一集」/连播
5. **继续观看置顶列**：目录页顶部横排最近看的剧，一键续看（有更新带 🔔）
6. **片库内建播放**：片库 ▶ 用同一套 mpv 会话播放本地已下载集
   （续看/连播/窗口记忆全通用），不再丢给系统播放器
3. **片库管理**：下载进度实时可见，完成后一键 ▶ 播放 / 打开目录

![真机演示：目录 → 更多 → 详情 → 下载 → 片库](docs/shots/demo.gif)

| 目录卡片墙（分类过滤+追更标） | 详情看片视图（▶ 直播） | 下载模式 · 片库 |
| --- | --- | --- |
| ![目录](docs/shots/catalog.png) | ![详情](docs/shots/detail.png) | ![片库](docs/shots/library.png) |

**反馈**：遇到问题（闪跳、下载失败、界面异常）或想要新功能？
[开个 Issue](https://github.com/ziyu4huang/guoapp/issues/new/choose)，
bug 请附 `guo --version`、平台与数据目录里的 `mpv-last.log` 尾部。

**🎬 视频演示**:[在电脑上追短剧——全功能演示](https://www.bilibili.com/video/BV1bCpZ6BEtb)(B站) ·
[YouTube](https://youtu.be/jkY7YnxR-b4)

> ⚠️ 免责声明：仅供学习研究。站源可用性、清晰度取决于源站及网络；本工具不解除
> 源站 VIP 或任何授权限制。请勿用于商业用途。如有侵权请联系删除。

## 原生 GUI（guo-gui）

```sh
cargo run --release --bin guo-gui     # 或直接运行 target/release/guo-gui
```

- **目录**：真人剧 / 漫剧 / AI剧 封面卡片墙，按分类续扫（「更多××」加载下一页）
- **搜索**：输入剧名回车，结果卡片墙（与 CLI 同一套合并排序逻辑）
- **详情面板**：点击卡片展开，封面、简介、点选集数（默认全选），
  「下载选中 / 全部下载 / 播放第1集」
- **下载管理**：双 worker 并发，实时进度条，失败原因内联显示
- **片库**：已下载剧目列表，每集 ▶ 按钮用系统播放器打开，「打开目录」直达文件夹
- 封面：CDN 返回 HEIC，macOS 上自动经系统 `sips` 转码（零额外依赖）；
  非 macOS 需站源返回 jpeg/webp 才有封面
- 在线播放：依赖 mpv（`brew install mpv`）；红果流是 CENC 加密的，工具自动把
  spade_a 解出的 AES 金钥经 `--demuxer-lavf-o=decryption_key` 交给 mpv 的
  mov 解复用器，逐帧 md5 与下载后本地播放完全一致（2026-10-12 实测）
- **播放会话（v0.3.0）**：单实例 mpv + JSON IPC——点下一集不再开新窗口，
  同一窗口 `loadfile` 无缝换集；窗口标题/OSC 显示《剧名》第N集；详情面板
  高亮正在播的集数、提供「⏭ 下一集」与「连播」开关（播完自动接下一集）；
  `--hwdec=auto`（macOS VideoToolbox）修复 HEVC 软解丢帧闪跳；播放器 stderr
  收进数据目录 `mpv-last.log`；播放器默认 mpv，想用 [IINA](https://iina.io/)
  （原生 macOS 界面/画中画）设环境变量 `GUO_PLAYER=iina` 显式开启；
  `GUO_MPV` 可指定任意 mpv 兼容播放器，`GUO_MPV_ARGS` 追加 mpv 参数
- **界面效率与体验（v0.3.1）**：后台消息即时上屏（封面/进度/播放事件不再等
  鼠标动才刷新）；封面抓取 4 并发队列（一页 18+ 张不再同时开线程）；目录/
  搜索/详情/片库列表渲染零逐帧克隆（之前每帧克隆全部剧集数据）
- **串流缓冲与连播（v0.3.2–v0.3.4）**：起播先缓冲 3 秒、预读 60s/上限 128MiB、
  CDN 断线自动重连、15s 网络超时兜底——网络抖动少卡顿；连播/下一集打在
  同一个播放器窗口上（idle 常驻会话，播完不关窗不重开），播完信号走 mpv
  标准的 end-file 事件；换集命令自动重试，缓冲期点击不报错
- **目录排序（v0.3.5）**：目录接口原生支持 `sort`（实测 play_cnt=按真实
  观看数、hot_score=平台热度榜、online_time=最新）——GUI 排序列+CLI
  `--sort`；同一排序两次请求顺序会漂（API 掺推荐），客户端按观看数稳定排序；
  网页回退通道不支持排序（保持原序）
- **精确续看与继续观看（v0.4.2）**：见功能清单 3/4/5；CLI `guo play
  <剧> <集> --from <秒>` 同款机制
- **性能（v0.4.1）**：mimalloc 分配器（egui 官方实测 ~20%）；下载
  4 连线 Range 并行（限速 CDN 实测 2.5-3.5×，不支持 Range 自动回退单线）；
  HTTP 连接池跨集复用 + 读超时（僵死连接 60s 自断）；封面纹理 LRU 上限；
  进度回调节流 10Hz；起播缓冲 3s→2s
- **无缝连播（v0.3.4）**：播放当前集时后台解析下一集并预载进播放器播放
  列表（`--prefetch-playlist` 提前缓冲）——播完瞬间 mpv 内部直接切换，
  无黑屏等待，像本地连播一样连续
- **播放器记忆（v0.3.3）**：窗口位置、大小（跨显示器稳定的 window-scale）、
  音量、静音、倍速、全屏/最大化跨启动恢复——拖好的窗口下次还在原地；
  macOS 读窗口坐标（CoreGraphics），其余平台恢复除位置外的全部设定；
  播放中/换集/关窗时自动捕获，改 GUO_MPV_ARGS 可随时覆盖
- 本地已下载文件用系统默认播放器
- 数据与 CLI 共用：`$GUOAPP_DATA` / `~/.local/share/guoapp`（macOS
  `~/Library/Application Support/guoapp`），下载状态两端互通

## 代码结构

| 模块 | 职责 |
| --- | --- |
| `src/hgapp.rs` + `src/sign.rs` | 红果 App API（X-Gorgon 签名）+ 目录/详情/取流解析 |
| `src/hgweb.rs` | 网页 `_ROUTER_DATA` 目录/详情/播放/搜索回退层 |
| `src/backup.rs` | 备用播放中继 + v2 响应解密 + spade_a 内容密钥 |
| `src/cenc.rs` | 自带渐进式 MP4 CENC-CTR 解密器（在线播放/离线解密共用） |
| `src/media.rs` | HLS/MP4 下载（分片/密钥本地化改写、多连接并行、断点续传） |
| `src/player.rs` | mpv 单实例 IPC 会话（连播预载/窗口记忆/播放位置取样） |
| `src/browser_cookies.rs` | 浏览器登录 cookie 自动提取（Chrome 家族+Safari） |
| `src/state.rs` | 播放记录/下载片库/偏好持久化（JSON 状态） |
| `src/gui/` | egui 原生 GUI（目录/搜索/片库/详情双模式/播放条） |

## 登录解锁取流（重要）

上游已锁定匿名取流（2026-10-15，详见
[INC 文档](docs/INC-20261015-play-endpoint-lockdown.md)），未登录时点击播放会提示
「取流失败」。试行解锁路径：

1. 详情页点「🔑 登录」→ 点「⚡ 从浏览器自动获取」直接读本机浏览器的红果登录
   cookie（macOS：Chrome/Edge/Brave/Arc/Chromium→Safari 级联；首次访问
   Chrome 系会弹钥匙圈授权对话框，点「允许」即可，此后免打扰）；
   非 macOS 或自动获取失败时，按对话框三步手动粘贴整行 Cookie（仅存本机）；
2. 重新点击要播放的集数——cookie 解锁有效即可恢复播放；
3. 命令行一步到位：`guo cookie` 自动提取并保存（`--no-save` 试提取、
   `--domain <域名>` 换目标域），再用 `guo web-play <series_id> <video_id>` 验证。

注意：自动提取读的是**你已登录的浏览器**——若从未在任一浏览器登录过
hongguoduanju.com，先登录一次，之后 `guo cookie` / ⚡ 按钮随时一键同步。
Cookie 值永不打印到日志/输出（一律脱敏显示），只落本机数据目录 state.json。

若 cookie 路径无效（上游改动仍在加码），片库已下载的内容不受影响。

## 下载安装（预编译）

从 [GitHub Releases](https://github.com/ziyu4huang/guoapp/releases) 下载对应平台的压缩包
（tag push 后由 GitHub Actions 自动构建发布）：

| 压缩包 | 平台 | 内容 |
| --- | --- | --- |
| `guoapp-v*-macos-arm64.zip` | macOS（Apple Silicon） | `Guo.app`（GUI） |
| `guoapp-v*-macos-x64.zip` | macOS（Intel） | 同上 |
| `guoapp-v*-windows-x64.zip` | Windows 10/11 x64 | `Guo.exe`（GUI） |
| `guoapp-v*-linux-x64.tar.gz` | Linux x64（glibc） | `guo-gui`（GUI） |

> 预编译包只含 GUI（看片产品本体）。命令行版 `guo` 面向进阶用户，
> 任何平台 `cargo build --release` 自取。

- **macOS 报「已损坏」？** 这是 macOS 13+ 对**无开发者签名** App 的统一报法
  （下载自带隔离标记），不是包真的坏了。三选一：
  ① 双击包内的 `双击修复打开.command`（一键解除隔离）；
  ② 终端执行 `xattr -cr /Applications/Guo.app`；
  ③ 系统设置 → 隐私权与安全性 → 底部「仍要打开」。
  彻底消除警告需要 Apple Developer ID 签名+公证（$99/年），暂未购买。
  解压后把 `Guo.app` 拖进「应用程序」即可。
- **Windows**：解压到任意目录直接运行 `Guo.exe`。在线播放需
  [mpv](https://mpv.io/)：安装后确保 `mpv` 在 PATH，或设环境变量
  `GUO_MPV` 指向 `mpv.exe` 完整路径。数据目录为
  `%USERPROFILE%\AppData\Local\guoapp`。
- **Linux**：解压后 `./guo-gui` 需 X11/Wayland 桌面环境；封面 HEIC 解码
  依赖 macOS `sips`，其他平台站源返回 jpeg/webp 时才有封面（当前已知限制）。
- **源码构建**：任何平台 `cargo build --release` 即可，无系统依赖。

## 构建

```sh
cargo build --release     # target/release/{guo, guo-gui}（CLI + GUI）
cargo test                # 单元测试（无网络）
# GUI/端到端套件（hermetic：目录/详情/搜索/封面/媒体全走本地 fixture 伺服器，
# 上游再怎么改都不影响测试；需 mpv + ffmpeg）：
cargo test --test gui_harness --test record_gif --test screenshots -- --ignored
```

依赖仅：ureq、serde/serde_json、md-5、sha2、hex、aes、cbc、base64、url，
GUI 另加 eframe（egui 0.31）、image（jpeg/png/webp）。

## 用法

```sh
guo catalog                          # 浏览目录（真人剧+漫剧+AI剧 各 1 页）
guo catalog --genre comic_series --pages 3
guo search 闪婚                      # 官网搜索 + 名称索引合并
guo suggest 重生                     # 搜索联想
guo detail hongguo:7632003395225668670
guo resolve <剧ID> 1                 # 打印播放地址 + CENC 密钥
guo play <剧ID> 1 --player mpv       # 在线播放（自动传 Referer + CENC 解密金钥）
guo download <剧ID> --eps 1-5,8      # 离线下载（缺省全部分集）
guo download <剧ID> --quality 720    # 指定画质
guo library                          # 本地已下载剧目
```

剧 ID 形如 `hongguo:7632003395225668670`（catalog/search/detail 输出第一列）。
所有命令支持 `--json` 输出结构化结果，方便脚本化。

### 数据目录

`$GUOAPP_DATA` 优先；默认 `~/.local/share/guoapp`（macOS：
`~/Library/Application Support/guoapp`）。内存放 `state.json`
（设备 ID + 分类游标）、`library.json`（下载索引）与 `library/<剧>/ep_NNN/`。

### 离线播放

- 直连 mp4：下载后自动解密 CENC（渐进式 MP4，AES-128-CTR，senc/saio/saiz，
  8/16 字节 IV 均支持），产出普通 `media.mp4`，任何播放器可放。
- HLS：分片与 AES-128 密钥全部本地化，`index.m3u8` 离线可播
  （`mpv ep_001/index.m3u8`）。
- 失败重跑同一命令即续传（.part + ETag/If-Range 校验）。

### 隐藏调试命令

```sh
guo fetch <URL> --referer <URL> --key <hex> --out <目录>   # 下载任意 mp4/m3u8
guo cenc-debug --file <mp4>                                # 打印轨道解析结果
```

## 取流回退链

1. App API（`api5-normal-sinfonlineb.fqnovel.com`，X-Gorgon/X-Khronos 签名）
2. 网页（`hongguoduanju.com`，`_ROUTER_DATA` JSON）
3. 备用中继（`djapi.999888456.xyz`，`v2.` 加密响应 + spade_a 内容密钥）

目录/详情/搜索同样具备 App → 网页 双层回退。

## 验证记录（2026-10-11）

- 签名算法经独立移植交叉对拍，逐字节一致（单元测试固化）。
- 目录/搜索/联想/详情：真实 API 全通；网页回退路径验证通过。
- 取流：1080p 直连 mp4 + CENC 密钥。
- 下载 + 解密：`ffmpeg -v error` 全片（113s，HEVC 1080x1920 + HE-AACv2）
  零错误解码；逐样本字节与 `ffmpeg -decryption_key` 的参考输出一致。
- HLS 捆绑：本地 fixture（master→variant、AES-128 key、分片改写）端到端通过。
- 备用中继：实测返回 `v2.api_<hex>.<b64>`，与解析实现兼容。

## 源码结构

```
src/
├── lib.rs     库入口（CLI 与 GUI 共用）
├── main.rs    CLI 子命令（手写解析，无 clap）
├── bin/
│   └── guo-gui.rs   GUI 入口（薄壳，调 guo::gui::run）
├── gui/mod.rs 原生 GUI（egui 卡片墙 / 详情 / 下载管理 / 片库）
├── hgapp.rs   红果 App API（签名请求、目录游标、详情、取流、搜索）
├── hgweb.rs   网页回退（_ROUTER_DATA、搜索排序、合并）
├── backup.rs  备用播放中继（v2 响应解密、spade_a 密钥）
├── cenc.rs    渐进式 MP4 CENC-CTR 解密器（零依赖）
├── media.rs   取流回退链、HLS 捆绑下载、直连断点续传
├── sign.rs    X-Gorgon 签名与设备 ID
├── state.rs   状态与片库（JSON 持久化）
├── jsonx.rs   serde_json::Value 导航辅助
└── model.rs   Drama / Chapter / Media
```
