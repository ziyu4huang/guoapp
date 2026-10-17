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
   → **v0.5.1 自動化取 cookie**(用戶令「幫我自動化取cookie」;移植主控 repo safari-cookies skill 的 chrome 主路正典):
   `src/browser_cookies.rs` = Chrome 家族級聯(Chrome→Edge→Brave→Arc→Chromium,全 profile,Default 優先)
   直讀 cookie 庫(SQLite 複製副本唯讀查詢)+ v10 解密(PBKDF2-SHA1(鑰匙圈「<Browser> Safe Storage」,
   "saltysalt",1003 輪,16B)+ AES-128-CBC IV=16×0x20 + PKCS7 + 2026-07+ 32B host 綁定前綴取最長可列印後綴)
   → Safari `Cookies.binarycookies` 兜底(混合位元組序版面,skill 實證版)。
   入口=GUI 🔑 對話框「⚡ 从浏览器自动获取」按鈕(後台線程,鑰匙圈首訪彈對話框按「允許」一次)+
   CLI `guo cookie [--no-save] [--domain <域名>]`。同名 cookie 去重(後寫覆蓋,skill dict 語義);
   值永不打印(一律 `前4碼****(len N)` 脫敏);結果只落本機 state.json。
   **實機驗證(1016)**:Chrome·Default 對 google.com 32→去重 22 顆全數解密成功(含 __Host-/__Secure- 前綴真 session cookie);
   本機 Chrome/Safari 均無 hongguoduanju.com 登錄態(靜默跳過零打擾,正確)——待用戶在任一瀏覽器登錄一次後一鍵同步。
   安全邊界:無該域 cookie 時不觸碰鑰匙圈(零對話框);Safari 路需完全磁碟存取(錯誤訊息附指引);
   Windows 自動提取未支援(DPAPI 另案),提示手動貼。
4. **跟隨上游**:Go 原版修復後移植。

## 附帶
診斷探針保留:`cargo test --test probe_play_raw -- --ignored --nocapture`(GUO_PROBE_VIDEO 可換 video_id)。

## 2026-10-17 後記:匿名公開端點發現,免費集播放恢復(v0.6.0)
- **發現路徑**:播放頁 SSR 形狀已變(`player_(series_id)/(vid)/page`,新欄位 `protectedPlayback:true`/`publicVideoSeo`/`videoList` 空=登入閘仍在);改挖前端 bundle `player.*.js` → 找到構造器 `"/video/public/".concat(vid,"/360p.mp4")`(配合 `publicVideoSeo.available`)。
- **端點實渉**:`GET /video/public/<vid>/360p.mp4` 匿名 206 直出**完整集** MP4(avc1,175.63s 與 SEO 元數據吻合,Range 支援,免 UA/Referer);僅 360p(480p+ 全 404);覆蓋=每劇前幾集免費額度(實測劇目 ep1-3 可/ep40/80 404);App 簽名端點同日仍空(body len=0)=鎖定未撤,公開端點是**並行的另一扇門**。
- **guoapp 落地**:`hgweb.public_media`(Range 0-0 探針驗 206/200)接入取流鏈第四層(web cookie 之後、備用中繼之前——登入態可得高畫質故優先);GUI 取流失敗提示指路「前幾集免費/🔑登錄」;fixture 伺服器補 `/video/public/` 路由+tests/public_media.rs(命中/404 兩測,404 誤報成功=黑屏陷阱已釘)。
- **驗證**:CLI resolve 免費集=360p 直鏈;mpv 真播 time-pos 34.4s/175.6s 推進中;收費集四層誠實報錯。
- **未驗**:登入 cookie 對 `protectedPlayback` 劇集的實際解鎖效果(本機無紅果登入態);高畫質公開變體(僅 360p 存在)。

## 2026-10-17 第二輪:免登入免 cookie 全路徑普查(證偽勿重測)
使用者令「try again, get resource without login and cookie」——窮舉後**紅果本體免登入可得=每劇前 3 集 360p**(全站統一,兩劇實測 ep1-3=206/ep4+=404;已在 v0.6.0 接線)。其餘全路死因入帳:
- **App 層與版本無關**:version_code 73532/74000/74102/75000/76000 五檔 A/B 全空回應——閘不在版本淘汰,在設備指紋/Argus 證言或登入態(GUO_APP_VERSION_CODE env 覆寫留作探針工具)。
- **SSR 對收費集整頁隱藏**:ep40 播放頁匿名=HTTP 404(非 200+空數據);免費集頁 200 但 video_player_info/videoList 空anonymous(protectedPlayback:true)。播放數據=純 SSR 供給(player.js 考據 playerData=u.video_player_info),無客戶端播放 API 可模擬。
- **qznovelvod CDN 族不可直連**:v1-hgweb/v1-reading.qznovelvod.com DNS 不解析(內部命名);受保護 URL 帶 x-amz-signature/play_auth_token 簽名(q.js 日誌脫敏器鍵名清單實錄),無簽名不可構造。
- **React Router .data 取數路由**:404(非 single-fetch 形態)。
- **上游 Go 原版同樣只有 api5-normal-sinfonlineb 單 host**(novel.snssdk.com 僅 Referer),無備用 host 族可切。
- **djapi 中繼活著但無數據**(200 但不回媒體)。
- **上游另接的鏡像站**(huangguoai.com+五鏡像/sorani.net/guipian/hanxiaoquan)=成人內容鏡像庫,與紅果劇目不同源;接線屬產品決策(公開 repo 內容風險),未動。
**結論:第 4 集起的免登入解鎖不存在可寫代碼的路徑;唯一未驗的門=真登入態(GUI ⚡ 自動取 cookie)。**

## 2026-10-17 第三輪:EP4+ 專項普查(使用者令「EP4以上是不是也可以解」)
**結論:EP4+ 免登入不可解——閘是帳號權益(服務端按劇的 vid 級授權),非設備/版本/cookie 可繞。** 全向量實測:
- **applog 設備註冊被拒**:log.snssdk.com/service/2/device_register/ 回 200 但 device_id=0/install_id=0(新代 applog 要 X-Argus 證言)——本地自造 device_id 無法升級成正規註冊設備。
- **紅果 passport 註冊無權限**:/passport/device/register/ 端點存在但 error_code=16「該應用無權限」(需 App 證言);其餘 register 路徑 404。
- **播放端點族 7 變體**:video_model/v2 空、multi_video_model/short_video_model/duanju/player/reading 系/player/video_play_info 全 404。
- **ttwid(匿名設備會話)無差**:帶新鮮 ttwid 訪 ep40 播放頁仍 404、公開端點仍 404——**公開集是 vid 內生屬性**(每劇前 3 集),非 cookie 記帳式額度,輪換 cookie 無效。
- **FaaS 路徑把戲全死**:尾綴 ?/雙斜線/%2f/大小寫/.MP4/protected 路徑/%00 → 404/400。
- **fanqienovel.com 無短劇區**(番茄生態網頁端只有小說/有聲)。
探針沉澱=tests/probe_register.rs+tests/probe_endpoints.rs(#[ignore] 真網診斷,與 probe_play_raw 同族)。**唯一未驗的門=真登入態(使用者帳號 cookie;GUI ⚡ 自動取+guo web-play 驗證已備好)。**

## 2026-10-17 終章:全集免登入恢復(v0.7.0;使用者令「search internet about the answer」)
**網路查證命中 GitHub bimoshuxinghe/hongguoTV commit 0815d5ec(10-16「APK 端取址改用无签名+随机设备+多域名」)**——徹底翻案第三輪「EP4+ 不可解」結論:
- **方法**:`POST /novel/player/multi_video_model/v1/`(此前探針漏了 /v1/ 後綴=404 假陰性教訓)+ **無簽名**(帶 X-Gorgon 反被拒 Code 110001)+ **每次隨機 18 位 device_id/iid**(900e15~999e15)+ **四域名輪詢**(sinfonlinec/sinfonlineb/lf/api5-normal)+ payload `mixed_video_id_map{"1004":[vid]}`/`video_platform:1024`/version_code 71332。
- **回包**:`data.<vid>.video_model`=內嵌 JSON 字串,video_list 與舊 video_model 同形(360p-1080p 五檔,bytevc1,CENC cenc-aes-ctr+spade_a)——select_app_media/content_key 全復用。
- **實證**:EP40 resolve=1080p+金鑰;mpv 真播 time-pos 21s/120.23s 推進中;ep1/ep80 同樣 1080p。**免費集也從 360p 公開端點升級到 1080p 主路。**
- **guoapp 落地**:hgapp.multi_video_model(四域名 failover)+parse_multi_video_model(真樣本單測:五檔/CENC/vid 未命中 Err/業務碼 Err);resolve 鏈首位=multi,回退序=帶簽名 App→web cookie→公開 360p→備用中繼;GUI「前3集免費」提示撤除(不再適用);fixture 路由+真樣本入庫。
- **教訓入帳**:①「端點不存在」判定前必須窮舉後綴變體(/v1//v2/);②本地探針窮盡≠網路上無解——社區(其他客戶端 repo 的修復 commit)是現成答案庫,早該先搜;③帶簽名被拒=試無簽名(閘常按「過度合法」攔截)。
