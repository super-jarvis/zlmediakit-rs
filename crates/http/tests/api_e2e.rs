use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::hook::HookClient;
use zlmediakit_core::media_frame::{CodecId, MediaFrame};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_core::recorder::RecorderControl;
use zlmediakit_core::stream_proxy::StreamProxyControl;
use zlmediakit_http::server::{HttpServer, HttpServerConfig};

const TEST_PORT: u16 = 19149;

async fn http_get(path: &str, port: u16) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    let request = format!("GET {} HTTP/1.0\r\nHost: localhost\r\n\r\n", path);
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let s = String::from_utf8_lossy(&buf);
    let pos = s.find("\r\n").unwrap_or(0);
    let status_line = &s[..pos];
    let code = status_line
        .splitn(3, ' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body_start = s.find("\r\n\r\n").map(|p| p + 4).unwrap_or(buf.len());
    let body = buf[body_start..].to_vec();
    (code, body)
}

fn avcc_config() -> Vec<u8> {
    let sps = [0x67u8, 0x42, 0x00, 0x1f, 0x9a, 0x66, 0x02, 0x80, 0x2c, 0x8e];
    let pps = [0x68u8, 0xee, 0x3c, 0x80];
    let mut v = vec![
        0x17,
        0x00,
        0x00,
        0x00,
        0x00,
        0x01,
        0x42,
        0x00,
        0x1f,
        0xff,
        0xe0 | 1,
        0x00,
        0x0a,
    ];
    v.extend_from_slice(&sps);
    v.push(0x01);
    v.extend_from_slice(&[0x00, 0x04]);
    v.extend_from_slice(&pps);
    v
}

struct TestServer {
    mgr: Arc<MediaSourceManager>,
    port: u16,
}

impl TestServer {
    async fn new(port: u16) -> Self {
        let mgr = Arc::new(MediaSourceManager::new());
        let auth = StreamAuth::new(false, String::new());
        let hook = HookClient::empty();
        let recorder = Arc::new(RecorderControl::new().0);
        let proxy = Arc::new(StreamProxyControl::new().0);

        let addr = format!("127.0.0.1:{}", port);
        let srv = HttpServer::new(HttpServerConfig {
            addr,
            source_manager: mgr.clone(),
            auth,
            hook,
            recorder,
            proxy,
            pusher: Default::default(),
            ffmpeg: Default::default(),
            record_root: std::path::PathBuf::from("./record"),
            www_root: None,
            ssl_cert: None,
            ssl_key: None,
        })
            .await
            .expect("HttpServer start");

        tokio::spawn(async move {
            let _ = srv.run().await;
        });

        tokio::time::sleep(Duration::from_millis(200)).await;
        TestServer { mgr, port }
    }

    async fn get(&self, path: &str) -> (u16, Vec<u8>) {
        http_get(path, self.port).await
    }

    fn mgr(&self) -> &Arc<MediaSourceManager> {
        &self.mgr
    }
}

#[tokio::test]
async fn http_api_get_server_config() {
    let srv = TestServer::new(TEST_PORT + 0).await;
    let (code, body) = srv.get("/index/api/getServerConfig").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert_eq!(json["server"], "zlmediakit-rs");
}

#[tokio::test]
async fn http_api_get_media_list() {
    let srv = TestServer::new(TEST_PORT + 1).await;

    // Initially empty
    let (code, body) = srv.get("/index/api/getMediaList").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert_eq!(json["result"].as_array().unwrap().len(), 0);

    // Create a stream
    let source = srv
        .mgr()
        .get_or_create("__defaultVhost__", "live", "teststream");
    let cfg = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, avcc_config().into(), false);
    source.publish_and_cache(cfg).await;

    // Should now appear
    let (code, body) = srv.get("/index/api/getMediaList").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let list = json["result"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["app"], "live");
    assert_eq!(list[0]["stream"], "teststream");
    assert_eq!(list[0]["vhost"], "__defaultVhost__");

    source.close();
}

#[tokio::test]
async fn http_api_get_media_info() {
    let srv = TestServer::new(TEST_PORT + 2).await;

    // No stream -> 404
    let (code, body) = srv.get("/index/api/getMediaInfo?stream=nonexistent").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], -404);

    // Create stream
    let source = srv
        .mgr()
        .get_or_create("__defaultVhost__", "live", "infostream");
    let cfg = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, avcc_config().into(), false);
    source.publish_and_cache(cfg).await;

    let (code, body) = srv.get("/index/api/getMediaInfo?stream=infostream").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert_eq!(json["result"]["app"], "live");
    assert_eq!(json["result"]["stream"], "infostream");

    source.close();
}

#[tokio::test]
async fn http_api_get_statistic() {
    let srv = TestServer::new(TEST_PORT + 3).await;

    // Initially 0 streams
    let (code, body) = srv.get("/index/api/getStatistic").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert_eq!(json["result"]["totalStreams"], 0);

    // Create a stream
    let source = srv
        .mgr()
        .get_or_create("__defaultVhost__", "live", "stattest");
    let cfg = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, avcc_config().into(), false);
    source.publish_and_cache(cfg).await;

    let (code, body) = srv.get("/index/api/getStatistic").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["result"]["totalStreams"], 1);

    source.close();
}

#[tokio::test]
async fn http_api_close_stream() {
    let srv = TestServer::new(TEST_PORT + 4).await;

    // Close non-existent stream -> 404
    let (code, body) = srv.get("/index/api/closeStream?stream=nonexistent").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], -404);

    // Create and verify it exists
    let source = srv
        .mgr()
        .get_or_create("__defaultVhost__", "live", "closetest");
    let cfg = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, avcc_config().into(), false);
    source.publish_and_cache(cfg).await;

    let (_, body) = srv.get("/index/api/getMediaList").await;
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["result"].as_array().unwrap().len(), 1);

    // Close the stream
    let (code, body) = srv.get("/index/api/closeStream?stream=closetest").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);

    // Give time for close to propagate
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Verify it's gone
    let (code, body) = srv.get("/index/api/getMediaList").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["result"].as_array().unwrap().len(), 0);
}
