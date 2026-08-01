use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Duration;
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::hook::HookClient;
use zlmediakit_core::media_frame::{CodecId, MediaFrame};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_core::recorder::RecorderControl;
use zlmediakit_core::session::SessionManager;
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
        .split(' ')
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
        let mgr = Arc::new(MediaSourceManager::new(None));
        let session_mgr = Arc::new(SessionManager::new());
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
            rtp: None,
            sip: None,
            transcode: None,
            session_manager: Some(session_mgr),
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
    let srv = TestServer::new(TEST_PORT).await;
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

#[tokio::test]
async fn http_api_get_api_list() {
    let srv = TestServer::new(TEST_PORT + 10).await;
    let (code, body) = srv.get("/index/api/getApiList").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    let data = json["data"].as_array().expect("data array");
    assert!(data.iter().any(|v| v == "/index/api/getMediaList"));
    assert!(data.iter().any(|v| v == "/index/api/getAllSession"));
    assert!(data.iter().any(|v| v == "/index/api/kick_sessions"));
    assert!(data.iter().any(|v| v == "/index/api/close_streams"));
}

#[tokio::test]
async fn http_api_version() {
    let srv = TestServer::new(TEST_PORT + 11).await;
    let (code, body) = srv.get("/index/api/version").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert!(!json["data"]["commitHash"].as_str().unwrap_or("").is_empty());
}

#[tokio::test]
async fn http_api_threads_load() {
    let srv = TestServer::new(TEST_PORT + 12).await;
    let (code, body) = srv.get("/index/api/getThreadsLoad").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert!(!json["data"].as_array().unwrap().is_empty());

    let (code, body) = srv.get("/index/api/getWorkThreadsLoad").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert!(!json["data"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn http_api_set_server_config() {
    let srv = TestServer::new(TEST_PORT + 13).await;
    let (code, body) = srv
        .get("/index/api/setServerConfig?general.flowThreshold=2048")
        .await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert!(json["changed"].as_u64().unwrap_or(0) >= 1);

    let (code, body) = srv.get("/index/api/getServerConfig").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entries = json["result"]["data"].as_array().unwrap();
    assert!(entries
        .iter()
        .any(|e| e["key"] == "general.flowThreshold" && e["value"] == "2048"));
}

#[tokio::test]
async fn http_api_is_media_online() {
    let srv = TestServer::new(TEST_PORT + 14).await;

    let (code, body) = srv.get("/index/api/isMediaOnline?stream=online_test").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["online"], false);

    let source = srv
        .mgr()
        .get_or_create("__defaultVhost__", "live", "online_test");
    let cfg = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, avcc_config().into(), false);
    source.publish_and_cache(cfg).await;

    let (code, body) = srv.get("/index/api/isMediaOnline?stream=online_test").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["online"], true);

    source.close();
}

#[tokio::test]
async fn http_api_close_streams_batch() {
    let srv = TestServer::new(TEST_PORT + 15).await;

    for name in ["batch_a", "batch_b"] {
        let source = srv.mgr().get_or_create("__defaultVhost__", "live", name);
        let cfg = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, avcc_config().into(), false);
        source.publish_and_cache(cfg).await;
    }

    let (code, body) = srv.get("/index/api/close_streams?app=live").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert_eq!(json["count_closed"], 2);

    let (code, body) = srv.get("/index/api/getMediaList").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["result"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn http_api_get_all_session_and_kick() {
    let srv = TestServer::new(TEST_PORT + 16).await;

    // A connection opens a session; snapshot before/after.
    let (_, _) = srv.get("/index/api/getAllSession").await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (code, body) = srv.get("/index/api/getAllSession").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let sessions = json["data"].as_array().unwrap();
    assert!(sessions.iter().any(|s| s["typeid"] == "HTTP"));
    let total = sessions.len();
    assert!(total >= 1);

    // kick_sessions with a filter that matches nothing
    let (code, body) = srv
        .get("/index/api/kick_sessions?peer_ip=203.0.113.99")
        .await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert_eq!(json["count_hit"], 0);

    // kick_sessions with protocol filter kicks all HTTP sessions
    let (code, body) = srv.get("/index/api/kick_sessions?typeid=HTTP").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert_eq!(json["count_hit"], total);

    let (code, body) = srv.get("/index/api/getAllSession").await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let sessions = json["data"].as_array().unwrap();
    // The request itself opens a fresh HTTP session, so at most that one remains.
    let http_count = sessions.iter().filter(|s| s["typeid"] == "HTTP").count();
    assert!(
        http_count <= 1,
        "expected at most 1 HTTP session, got {http_count}"
    );
}

#[tokio::test]
async fn http_api_get_media_player_list() {
    let srv = TestServer::new(TEST_PORT + 17).await;

    let (code, body) = srv
        .get("/index/api/getMediaPlayerList?stream=missing")
        .await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], -404);

    let source = srv
        .mgr()
        .get_or_create("__defaultVhost__", "live", "player_test");
    source.add_subscriber("HTTP-1".to_string()).await;
    let cfg = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, avcc_config().into(), false);
    source.publish_and_cache(cfg).await;

    let (code, body) = srv
        .get("/index/api/getMediaPlayerList?stream=player_test")
        .await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert!(!json["data"].as_array().unwrap().is_empty());

    source.close();
}

#[tokio::test]
async fn http_api_get_mp4_record_file() {
    let srv = TestServer::new(TEST_PORT + 18).await;
    srv.get("/index/api/getMp4RecordFile?stream=nonexistent")
        .await;

    let (code, body) = srv
        .get("/index/api/getMp4RecordFile?stream=nonexistent")
        .await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert_eq!(json["data"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn http_api_get_record_status() {
    let srv = TestServer::new(TEST_PORT + 19).await;

    let (code, body) = srv
        .get("/index/api/getRecordStatus?stream=rec_status")
        .await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert_eq!(json["status"], false);
}

#[tokio::test]
async fn http_api_gb28181_device_list() {
    // Wire a real SIP server into the HTTP server and verify getDeviceList /
    // queryCatalog against a registered fake device.
    let mgr = Arc::new(MediaSourceManager::new(None));
    let sip_cfg = zlmediakit_srt::SipServerConfig {
        sip_port: 0,
        realm: "3402000000".to_string(),
        password: None,
        host: Some("127.0.0.1".to_string()),
        media_port_base: 26000,
        vhost: "__defaultVhost__".to_string(),
    };
    let sip = zlmediakit_srt::SipServer::new(sip_cfg, mgr.clone(), 26000)
        .await
        .expect("sip server start");
    let sip_port = sip.local_port();
    let run = sip.clone();
    tokio::spawn(async move {
        let _ = run.run().await;
    });

    let session_mgr = Arc::new(SessionManager::new());
    let port = TEST_PORT + 20;
    let addr = format!("127.0.0.1:{port}");
    let srv = HttpServer::new(HttpServerConfig {
        addr,
        source_manager: mgr.clone(),
        auth: zlmediakit_core::auth::StreamAuth::new(false, String::new()),
        hook: zlmediakit_core::hook::HookClient::empty(),
        recorder: Arc::new(RecorderControl::new().0),
        proxy: Arc::new(StreamProxyControl::new().0),
        pusher: Default::default(),
        ffmpeg: Default::default(),
        record_root: std::path::PathBuf::from("./record"),
        www_root: None,
        ssl_cert: None,
        ssl_key: None,
        rtp: None,
        sip: Some(sip.clone()),
        transcode: None,
        session_manager: Some(session_mgr),
    })
    .await
    .expect("HttpServer start");
    let srv_task = Arc::new(srv);
    let srv_run = srv_task.clone();
    tokio::spawn(async move {
        let _ = srv_run.run().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Empty before any device registers.
    let (code, body) = http_get("/index/api/getDeviceList", port).await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert_eq!(json["data"].as_array().unwrap().len(), 0);

    // queryCatalog against an unregistered device -> error.
    let (code, body) = http_get(
        "/index/api/queryCatalog?device_id=34020000001320000099",
        port,
    )
    .await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_ne!(json["code"], 0);

    // Register a fake device over SIP.
    let dev = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let dev_addr = dev.local_addr().unwrap();
    let reg = format!(
        "REGISTER sip:34020000002000000001@127.0.0.1:{sip_port} SIP/2.0\r\n\
         Via: SIP/2.0/UDP {dev_addr};branch=z9hG4bKe2e\r\n\
         From: <sip:34020000001320000001@127.0.0.1:{sip_port}>;tag=1\r\n\
         To: <sip:34020000001320000001@127.0.0.1:{sip_port}>\r\n\
         Call-ID: e2e@127.0.0.1\r\n\
         CSeq: 1 REGISTER\r\n\
         Contact: <sip:34020000001320000001@{dev_addr}>\r\n\
         Expires: 3600\r\n\
         Content-Length: 0\r\n\r\n"
    );
    dev.send_to(reg.as_bytes(), ("127.0.0.1", sip_port))
        .await
        .unwrap();
    let mut buf = [0u8; 4096];
    tokio::time::timeout(Duration::from_secs(2), dev.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (code, body) = http_get("/index/api/getDeviceList", port).await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    let list = json["data"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["device_id"], "34020000001320000001");
    assert_eq!(list[0]["online"], true);

    // getDeviceInfo for a known device id.
    let (code, body) = http_get(
        "/index/api/getDeviceInfo?device_id=34020000001320000001",
        port,
    )
    .await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert_eq!(json["result"]["device_id"], "34020000001320000001");

    // getDeviceList with a device_id filter returns only that device.
    let (code, body) = http_get(
        "/index/api/getDeviceList?device_id=34020000001320000001",
        port,
    )
    .await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let list = json["data"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["device_id"], "34020000001320000001");

    // getDeviceList with a device_id filter for an unknown device -> empty.
    let (code, body) = http_get(
        "/index/api/getDeviceList?device_id=34020000001320000099",
        port,
    )
    .await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["data"].as_array().unwrap().len(), 0);

    // getSipInfo reports the running SIP server.
    let (code, body) = http_get("/index/api/getSipInfo", port).await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert_eq!(json["result"]["sip_port"], sip_port);
    assert_eq!(json["result"]["device_count"], 1);
    assert_eq!(json["result"]["stopped"], false);

    // stopSip stops the SIP server and clears its devices.
    let (code, body) = http_get("/index/api/stopSip", port).await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (code, body) = http_get("/index/api/getSipInfo", port).await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert_eq!(json["result"]["stopped"], true);
    assert_eq!(json["result"]["device_count"], 0);
}
