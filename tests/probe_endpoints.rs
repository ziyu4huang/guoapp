//! 播放端點變體族探針(真網,--ignored):video_model 同族端點看有無未鎖的門。
use guo::hgapp::HgClient;
use guo::state::Store;

#[test]
#[ignore]
fn probe_player_endpoint_family() {
    let client = HgClient::new();
    let store = Store::load();
    let vid = std::env::var("GUO_PROBE_VIDEO")
        .unwrap_or_else(|_| "7694859913515781145".into()); // 默認=測試劇 ep40(收費集)
    for path in [
        "/novel/player/video_model/v2/",
        "/novel/player/multi_video_model/",
        "/novel/player/video_model/",
        "/novel/player/short_video_model/",
        "/novel/duanju/player/",
        "/reading/player/video_model/v1/",
        "/novel/player/video_play_info/",
    ] {
        let result = client.app_request(
            &store,
            path,
            Some(&serde_json::json!({
                "video_id": vid, "content_type": 1,
                "biz_param": {"need_all_video_definition": true, "video_platform": 3}
            })),
        );
        match result {
            Ok(v) => {
                let text = v.to_string();
                println!("{} → OK len={} head={}", path, text.len(), &text.chars().take(120).collect::<String>());
            }
            Err(e) => println!("{} → {}", path, e),
        }
    }
}
