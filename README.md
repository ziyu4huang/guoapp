# guoapp（Rust 重写）

红果短剧命令行工具。参考 [zhoufuweigg/guoapp](https://github.com/zhoufuweigg/guoapp)
（Flutter + Go 原生核心，约 5.4 万行）的核心业务逻辑，用 Rust 从零重写为
**单一静态二进制**：release 约 **1.5 MB**，无运行时依赖，无 ffmpeg / mpv 依赖。

> ⚠️ 免责声明：仅供学习研究。站源可用性、清晰度取决于源站及网络；本工具不解除
> 源站 VIP 或任何授权限制。请勿用于商业用途。

## 与原版的关系

| 原版（Go `native/core` + Flutter） | 本项目（Rust） |
| --- | --- |
| `provider_hongguo*.go` App API + X-Gorgon 签名 | `src/hgapp.rs` + `src/sign.rs` |
| 网页 `_ROUTER_DATA` 目录/详情/播放/搜索回退 | `src/hgweb.rs` |
| 备用播放中继 + v2 响应解密 + spade_a 密钥 | `src/backup.rs` |
| FFmpegKit / mpv 解 CENC | `src/cenc.rs`（自带渐进式 MP4 CENC-CTR 解密器） |
| HLS 捆绑下载（分片/密钥本地化改写） | `src/media.rs` |
| 追剧/下载记录 | `src/state.rs`（JSON 状态） |

未移植：Flutter UI、弹幕、画中画、画质增强、局域网同步、其他站源
（韩小圈/鬼片/青空/黄豆等）。要的就是精小：只保留红果一条完整闭环。

## 构建

```sh
cargo build --release     # target/release/guo，约 1.5 MB
cargo test                # 7 项单元测试
```

依赖仅：ureq、serde/serde_json、md-5、sha2、hex、aes、cbc、base64、url。

## 用法

```sh
guo catalog                          # 浏览目录（真人剧+漫剧+AI剧 各 1 页）
guo catalog --genre comic_series --pages 3
guo search 闪婚                      # 官网搜索 + 名称索引合并
guo suggest 重生                     # 搜索联想
guo detail hongguo:7632003395225668670
guo resolve <剧ID> 1                 # 打印播放地址 + CENC 密钥
guo play <剧ID> 1 --player mpv       # 在线播放（mpv --referrer）
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
- 失败重跑同一命令即续传（.part + ETag/If-Range 校验，与原版一致）。

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

- 签名：与原版 Go 算法的独立 Python 移植逐字节一致（单元测试固化）。
- 目录/搜索/联想/详情：真实 API 全通；网页回退路径验证通过。
- 取流：1080p 直连 mp4 + CENC 密钥。
- 下载 + 解密：`ffmpeg -v error` 全片（113s，HEVC 1080x1920 + HE-AACv2）
  零错误解码；逐样本字节与 `ffmpeg -decryption_key` 的参考输出一致。
- HLS 捆绑：本地 fixture（master→variant、AES-128 key、分片改写）端到端通过。
- 备用中继：实测返回 `v2.api_<hex>.<b64>`，与解析实现兼容。

## 源码结构

```
src/
├── main.rs    CLI 子命令（手写解析，无 clap）
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
