//! 一次性 fixture 抓取器（唯 --ignored 手動跑）：把真實上游 response 存到
//! tests/fixtures/，供 hermetic 測試的本地 fixture 伺服器使用（真樣本鐵則：
//! 解析器 fixture 禁想像格式，必須真盤樣本）。上游格式變更後可重抓對比。
//! 运行：cargo test --test dump_fixtures -- --ignored --nocapture
//!
//! 這是全 repo 唯二故意打真網的測試檔（另一個=probe_play_raw 診斷探針）；
//! 上游死了它會紅——那正是「該重抓樣本」的信號，不屬於 hermetic 套件。

use guo::hgapp::{catalog_payload, HgClient, GENRES, SORT_LATEST};
use guo::state::Store;

const TEST_DRAMA: &str = "7694857999663238168"; // 既有測試劇目

#[test]
#[ignore]
fn dump_fixtures() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    std::fs::create_dir_all(&dir).unwrap();
    let client = HgClient::new();
    let store = Store::load();

    // 1. 目錄第一頁（AI剧 feed；任何 genre 皆可，樣本足夠解析器覆蓋）
    let payload = catalog_payload(&GENRES[2], &Default::default(), SORT_LATEST);
    let catalog = client
        .app_request(&store, "/reading/distribution/category/landpage/v/", Some(&payload))
        .expect("catalog");
    std::fs::write(dir.join("catalog-ai-p1.real.json"), serde_json::to_vec_pretty(&catalog).unwrap()).unwrap();

    // 2. 詳情（含分集列表）
    let detail = client
        .app_request(&store, "/novel/player/video_detail/v1/", Some(&serde_json::json!({"series_id": TEST_DRAMA})))
        .expect("detail");
    std::fs::write(dir.join("detail.real.json"), serde_json::to_vec_pretty(&detail).unwrap()).unwrap();

    // 3. 建議索引（搜索名稱路）+ 4. web 搜索頁（_ROUTER_DATA HTML）
    let agent = ureq::agent();
    let sug = agent
        .get(&format!(
            "https://hongguoduanju.com/incent_resource/suggestion?app_id=8662&query={}&count=50",
            guo::hgweb::urlencode("重生")
        ))
        .set("User-Agent", guo::hgapp::WEB_UA)
        .call()
        .expect("suggestion");
    std::fs::write(dir.join("suggestion.real.json"), sug.into_string().unwrap().into_bytes()).unwrap();

    let page = agent
        .get("https://hongguoduanju.com/search/%E9%87%8D%E7%94%9F")
        .set("User-Agent", guo::hgapp::WEB_UA)
        .call()
        .expect("web search page");
    std::fs::write(dir.join("web-search.real.html"), page.into_string().unwrap().into_bytes()).unwrap();

    eprintln!("fixtures → {}", dir.display());
}
