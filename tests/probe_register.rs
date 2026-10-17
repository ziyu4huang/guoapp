//! 設備註冊探針(真網,--ignored 手動):紅果 API 自家的 device register 端點族。
//! 動機=INC-20261015「要求已註冊設備」假說——本地自造 device_id 可能是 App 層
//! 空回應的根因;註冊成功→換正規 device_id 重試 video_model 看 EP4+ 是否解鎖。
use guo::hgapp::HgClient;
use guo::state::Store;

#[test]
#[ignore]
fn probe_register_endpoints() {
    let client = HgClient::new();
    let store = Store::load();
    for path in [
        "/device/register/v1/",
        "/passport/device/register/",
        "/settings/device/register/",
        "/device/register/",
    ] {
        let result = client.app_request(
            &store,
            path,
            Some(&serde_json::json!({
                "device_platform": "android", "os": "android", "os_version": "16",
                "device_type": "25053RT47C", "device_brand": "Redmi", "language": "zh",
                "region": "CN", "channel": "update_64", "ac": "wifi", "mcc_mnc": "46007",
            })),
        );
        match result {
            Ok(v) => {
                let text = v.to_string();
                let masked = if text.len() > 300 { format!("{}…(len {})", &text[..300], text.len()) } else { text };
                println!("{} → OK {}", path, masked);
            }
            Err(e) => println!("{} → ERR {}", path, e),
        }
    }
}
