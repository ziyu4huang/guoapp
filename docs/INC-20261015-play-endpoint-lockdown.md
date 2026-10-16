# INC-20261015:紅果取流端點被上游鎖定(三層取流全斷)

## 現象
2026-10-15 午後起,所有劇所有集取流失敗,GUI 顯示三層錯誤:
`App 取流：红果 App 接口未返回有效数据；网页取流：红果该集未提供公开播放地址，可能需要登录或 App 授权；备用取流：备用播放接口未返回该集可用的媒体和密钥`
當日上午(<=v0.4.3 驗證輪)取流仍正常 → 變更發生在 10-15 白天。

## 診斷(A/B + 探針,測試檔=tests/probe_play_raw.rs)
- **版本回歸排除**:v0.4.0(超時改動前)binary 同樣失敗。
- **設備排除**:全新 device_id 與長期 device_id 同樣失敗。
- **空回應實錘**:未簽名直打 `POST /novel/player/video_model/v1/` →
  **HTTP 200 + Content-Length: 0 + Content-Type: application/json**(上游 gateway 清空回應,非簽名驗證失敗)。
- **UA 排除**:app UA/okhttp/iOS Safari 三種 UA 全部 200+len=0。
- **端點族群**:v2 端點同樣空;去 /v1/ 後綴與其他路徑=404(端點存在,被針對)。
- **對照組**:`/reading/distribution/category/landpage/v/`(目錄)同 host 同簽名**正常**回全量數據 → per-endpoint 針對性鎖定。
- **上游倉庫**:zhoufuweigg/guoapp 9/26 後無相關修復可抄。

## 結論
上游(紅果)對未授權客戶端鎖定播放數據下發(推測:要求新版 App 簽名/已註冊裝置或登入會話)。
非 guoapp 代碼缺陷,無法以配置修復。

## 影響
串流播放三層全斷;下載(依賴取流)同斷;目錄/搜索/詳情/片庫(已下載檔)不受影響。

## 候選路徑(待裁決)
1. **觀察期**:若為暫時性風控(過去有先例),定期用 probe 重測。
2. **反向工程新簽名/裝置註冊**:深水區,成本高且軍備競賽。
3. **登入會話流程**:紅果帳號 QR/cookie 導入,用登入態取流(上游鎖的是匿名客戶端)——最正路,大功能。
   → **v0.5.0 已實作 cookie 導入 v1**(GUI 🔑 登錄對話框+`guo web-play` 探針;解鎖有效性待用戶以真實登入 cookie 驗證)。
4. **跟隨上游**:Go 原版修復後移植。

## 附帶
診斷探針保留:`cargo test --test probe_play_raw -- --ignored --nocapture`(GUO_PROBE_VIDEO 可換 video_id)。
