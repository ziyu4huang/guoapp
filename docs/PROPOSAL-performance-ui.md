# guoapp 改善預案:效能(優先)與 GUI

> 2026-10-14 立項。**狀態:預案,未實作**——按使用者令「先做預案,先不改動」。
> 參考:mpv 官方手冊(big-cache/low-latency 內建 profile)、egui 官方文檔(mimalloc ~20%/rayon 並行 tessellation/cache 模塊)、本庫代碼盤點(行號為 v0.4.0=4ad3a63 時點)。

## 一、效能(最重要)

### P0-1 下載 worker 每集重建 HTTP 客戶端(白扔 TLS 連線池)
- **證據**:`src/gui/mod.rs` 下載 job 內 `let client = HgClient::new()`——每集一個新 `ureq::Agent`,連線池/TLS session 全部作廢,每集重做 TLS 握手。
- **修法**:GUI 已有 `Arc<HgClient>`(mod.rs:316),傳進 job 復用。順帶檢查所有 `HgClient::new()` 呼叫點。
- **收益**:每集省 1-2 次握手(數百 ms)+ 減少被 CDN 風控的機率。
- **成本**:10 行。**風險**:無。

### P0-2 ureq Agent 只有連線超時,無讀取/總超時(僵死 worker)
- **證據**:`src/hgapp.rs:45-50` 只有 `timeout_connect`。下載/取流卡在無數據的連線上會**永久佔住 worker**(2 個 worker 壞一個=半速)。
- **修法**:`AgentBuilder::new().timeout_read(30s)`(下載 agent 可放寬到 60s);取流/目錄請求加 `.timeout(20s)`。
- **收益**:僵死自癒;配合已有的 job 超時是雙保險。
- **成本**:5 行。

### P1-3 下載單連線,無多段並行(被 CDN 限速時無解)
- **證據**:`src/media.rs download_direct` 單連線 Range 續傳。實測晚間 CDN 限速 ~700KB/s(2026-10-13 盤點)。
- **修法**:多段 Range 並行(4-8 連線分段拉,落地拼接;aria2/IDM 同款)。保留單連線路徑作為後備(伺服器不支援 Range 時)。
- **收益**:受限速场景 N 倍(實測 4 連線通常 2.5-3.5×)。
- **成本**:中(150 行+測試);**風險**:部分 CDN 對多連線風控,需可配置+可回退。

### P1-4 封面紋理無上限(UI 記憶體緩漲)
- **證據**:`CoverCache.map: HashMap<String, Option<TextureHandle>>` 只進不出;每頁 18 張×3 分類×多排序,刷久了 GPU/記憶體線性漲。
- **修法**:LRU 上限(如 300 張,超出逐出最舊 texture handle;egui `TextureHandle` drop 即釋放)。
- **成本**:40 行(lru 或手工 VecDeque 時序驅逐,不引依賴)。

### P1-5 egui 全域分配器換 mimalloc(官方文檔:~20%)
- **證據**:egui 文檔明言 custom allocator(mimalloc/talc)可顯著提速。
- **修法**:`Cargo.toml` 加 `mimalloc`,`fn main` 設 `#[global_allocator]`。
- **成本**:3 行+一個依賴。**風險**:低(mimalloc 成熟)。

### P2-6 起播延遲可再讓 1.5s
- **證據**:`cache-pause-wait=3` + `cache-pause-initial=yes`——首集起播必等 3 秒緩衝。無縫連播有 prefetch 熱數據不受影響,但**首次點開**是硬等。
- **修法**:wait 降到 1.5-2s(ADSL 級頻寬才會因此卡頓),或播放中動態(首集 2s、預載切換維持 3s)。
- **成本**:player.rs 一行+實測。

### P2-7 進度回呼節流(防 mpsc/重繪風暴)
- **證據**:HLS 逐分片回呼(OK);direct 下載逐 chunk 讀取時若每 chunk 發 `DownloadProgress` 會刷爆 mpsc 觸發高頻重繪。
- **修法**:回呼節流到 ≥100ms 間隔(進度條 10Hz 足夠)。
- **成本**:10 行。

### P2-8 mpv 微調(錦上添花,當前配置已接近最優)
- 已有:cache=yes/pause-initial/60s readahead/128MiB/lavf reconnect/15s timeout/hwdec=auto/prefetch-playlist——對照官方 big-cache profile(512MiB/20s)我們的預讀秒數更激進,合理。
- 可試:`--profile=fast`(降渲染 CPU)、macOS 顯式 `--hwdec=videotoolbox`(跳過 auto 探測)、`--stream-buffer-size=1MiB`。收益個位數 %,做 A/B 再收。

## 二、GUI(參考主流串流客戶端慣例;現 UI 凍結為基線,以下為下批候選)

### G1 「繼續觀看」置頂列(Netflix/YouTube 標配)
目錄頁頂部橫排「有播放記錄的劇」(按 updated_at 排),一鍵續看。數據(state.watch)已齊,純 UI。
### G2 集內進度記憶(精確續看)
mpv IPC 定期取 `time-pos/duration` 存 WatchRec.pos;「续看第N集」變「從 12:30 繼續」。解決 adversarial 指出的「續看=重播整集」語義缺口。
### G3 鍵盤快捷鍵
egui 層:`N`=下一集、`Enter`=續看/播放、`/`=聚焦搜索。mpv 窗口內的快捷鍵已原生。
### G4 片庫內建播放
片庫的「▶」目前丟給系統預設播放器——改用 WatchSession 同款 mpv 會話(統一續看/連播/窗口記憶體驗),下載內容不再遊離於看片動線外(adversarial 指出的孤兒)。
### G5 卡片 hover tooltip(簡介+評分)
懸停 500ms 出簡介浮層;egui `on_hover_ui`。
### G6 播放歷史管理頁
watch 記錄可清除(隱私);片庫頁加「清空播放記錄」。

## 三、明確不做
- 不換 HTTP 庫(ureq 阻塞模型+2 worker 夠用;async 化是反方向重構)。
- 不做 app 內嵌視頻渲染(維持 mpv 外窗+IPC)。
- 不做 HLS 預緩存到磁碟(下載管線已有)。
- 不做多語言/主題。

## 四、建議實施順序(若立項)
第一批(半天,全是低風險高確定性):P0-1、P0-2、P1-5、P2-7 → 發版實測下載/取流體感。
第二批(一天):P1-3 多段並行下載(帶開關與回退)+P1-4 封面 LRU。
第三批(按需):G1→G2→G4(動線閉環),P2-6/P2-8 配合 A/B。
