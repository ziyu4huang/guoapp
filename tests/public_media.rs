//! 匿名公开播放端点(public_media,2026-10-17 前端 bundle 考古发现)的
//! hermetic 测试:免费集命中 → 360p 直链;收费集 404 → Err(留给后续取流路)。
//! 运行：cargo test --test public_media -- --ignored

mod common;

/// 免费集:命中公开端点,返回 360p 无加密直链。
#[test]
#[ignore]
fn public_media_hits_free_episode() {
    common::hermetic().install();
    let client = guo::hgapp::HgClient::new();
    let media = client
        .public_media("7694859908860103705")
        .expect("免费集应命中公开端点");
    assert!(media.url.contains("/video/public/7694859908860103705/360p.mp4"));
    assert_eq!(media.quality, 360);
    assert!(media.cenc_key.is_none(), "公开端点无加密");
    assert!(media.url.starts_with(&format!("{}/video/public/", common::hermetic().base)));
}

/// 收费集(fixture 路径含 "paid" → 404):必须 Err——404 谎称成功会把
/// 「超出免费额度」洗成「播放黑屏」。
#[test]
#[ignore]
fn public_media_rejects_paid_episode() {
    common::hermetic().install();
    let client = guo::hgapp::HgClient::new();
    let result = client.public_media("paid0000000000000001");
    assert!(result.is_err(), "收费集 404 必须 Err: {:?}", result);
    assert!(result.unwrap_err().contains("404"));
}
