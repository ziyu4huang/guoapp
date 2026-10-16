//! 臨時診斷探針②:版本號/端點變體對 play 空回應的影響
#[test]
#[ignore]
fn probe_play_variants() {
    let client = guo::hgapp::HgClient::new();
    let store = guo::state::Store::load();
    let payload = serde_json::json!({
        "video_id": "7494709846981161001",
        "content_type": 1,
        "biz_param": { "need_all_video_definition": true, "video_platform": 3 }
    });
    // 變體 A:新版本號
    for vc in ["73532", "76336", "80366", "90000"] {
        let mut s2 = store.clone();
        s2.state.device_id = store.state.device_id.clone();
        // 直接改 common_query 不可行(私有)——繞法:app_request 讀 store 的 device_id/iid,
        // version_code 是寫死在 common_query。此處改以 raw agent 重放簽名請求太複雜,
        // 改為檢查「同端點不同 http 方法/無 body」的行為差異
        let _ = vc;
    }
    // 變體 B:GET 無 body 打同端點
    match client.app_request(&store, "/novel/player/video_model/v1/", None) {
        Ok(v) => eprintln!("== GET 無body: OK keys={:?}", v.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>())),
        Err(e) => eprintln!("== GET 無body: ERR {e}"),
    }
    // 變體 C:v1 端點 + 舊式 content_type 字串
    let p3 = serde_json::json!({ "video_id": "7494709846981161001", "content_type": "1" });
    match client.app_request(&store, "/novel/player/video_model/v1/", Some(&p3)) {
        Ok(v) => eprintln!("== content_type 字串: OK data={:?}", v.pointer("/data").map(|d| d.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()))),
        Err(e) => eprintln!("== content_type 字串: ERR {e}"),
    }
    // 變體 D:別的播放系端點(v2 常見升級)
    let p4 = serde_json::json!({
        "video_id": "7494709846981161001",
        "content_type": 1,
        "biz_param": { "need_all_video_definition": true, "video_platform": 3 }
    });
    for path in ["/novel/player/video_model/v2/", "/novel/player/video_model/", "/lukeng/player/video_model/v1/"] {
        match client.app_request(&store, path, Some(&p4)) {
            Ok(v) => {
                let keys = v.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>());
                eprintln!("== {path}: OK keys={keys:?} data_keys={:?}", v.pointer("/data").map(|d| d.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>())));
            }
            Err(e) => eprintln!("== {path}: ERR {e}"),
        }
    }
    // 變體 E:APP_BASE 直打(同 agent),確認空 body 是端點行為不是簽名路徑
    let url = format!("{}{}", guo::hgapp::APP_BASE, "/novel/player/video_model/v1/");
    match client.agent
        .post(&url)
        .set("User-Agent", guo::hgapp::WEB_UA)
        .send_bytes(serde_json::to_vec(&payload).unwrap().as_slice()) {
        Ok(resp) => {
            let len = resp.header("Content-Length").map(|s| s.to_string());
            let ct = resp.header("Content-Type").map(|s| s.to_string());
            let st = resp.status();
            let body_text = resp.into_string().map(|s| {
                let t = s.trim().to_string();
                format!("{}…(共{})", &t[..400.min(t.len())], t.len())
            }).unwrap_or_default();
            eprintln!("== raw(未簽名) status={st} len={len:?} ct={ct:?} body={body_text}");
        }
        Err(e) => eprintln!("== raw ERR {e}"),
    }
}
