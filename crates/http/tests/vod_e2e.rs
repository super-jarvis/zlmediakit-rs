use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify};
use tokio::time::Duration;
use zlmediakit_core::auth::{generate_sign, StreamAuth};
use zlmediakit_core::hook::HookClient;
use zlmediakit_core::media_frame::{CodecId, MediaFrame};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_core::recorder::{RecorderCommand, RecorderControl};
use zlmediakit_core::stream_proxy::StreamProxyControl;
use zlmediakit_flv::FlvRecorder;
use zlmediakit_http::server::{HttpServer, HttpServerConfig};

const TEST_PORT: u16 = 19158;
const RECORD_BASE: &str = "/tmp/zlmediakit_vod_e2e";
const RECORD_BASE_AUTH: &str = "/tmp/zlmediakit_vod_auth_e2e";

/// Sends a raw HTTP request and returns (status_code, body, full_response_text).
async fn http_request(request: &str, port: u16) -> (u16, Vec<u8>, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("connect");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.unwrap();
    let s = String::from_utf8_lossy(&buf).to_string();
    let status_line = &s[..s.find("\r\n").unwrap_or(0)];
    let code = status_line
        .splitn(3, ' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body_start = s.find("\r\n\r\n").map(|p| p + 4).unwrap_or(buf.len());
    (code, buf[body_start..].to_vec(), s)
}

fn h264_config_data() -> Vec<u8> {
    let sps = [0x67u8, 0x42, 0x00, 0x1f, 0x9a, 0x66, 0x02, 0x80, 0x2c, 0x8e];
    let pps = [0x68u8, 0xee, 0x3c, 0x80];
    let mut v = vec![
        0x17, 0x00, 0x00, 0x00, 0x00, 0x01, 0x42, 0x00, 0x1f, 0xff, 0xe0 | 1, 0x00, 0x0a,
    ];
    v.extend_from_slice(&sps);
    v.push(0x01);
    v.extend_from_slice(&[0x00, 0x04]);
    v.extend_from_slice(&pps);
    v
}

/// Minimal recorder supervisor that reacts to RecorderCommands (FLV only here).
fn run_supervisor(
    mut cmd_rx: mpsc::UnboundedReceiver<RecorderCommand>,
    mgr: Arc<MediaSourceManager>,
    control: Arc<RecorderControl>,
    base_path: String,
) {
    tokio::spawn(async move {
        let stops: Arc<tokio::sync::Mutex<std::collections::HashMap<String, Arc<Notify>>>> =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        while let Some(cmd) = cmd_rx.recv().await {
            let source_id = format!("{}/{}/{}", cmd.vhost, cmd.app, cmd.stream);
            if cmd.hls || cmd.flv || cmd.mp4 {
                if !stops.lock().await.contains_key(&source_id) {
                    if let Some(src) = mgr.get(&cmd.vhost, &cmd.app, &cmd.stream) {
                        let stop = Arc::new(Notify::new());
                        stops.lock().await.insert(source_id.clone(), stop.clone());
                        control.mark_recording(&source_id, cmd.hls, cmd.flv, cmd.mp4);
                        let rec_src = src.clone();
                        let rec_base = base_path.clone();
                        let rec_stop = stop.clone();
                        if cmd.flv {
                            tokio::spawn(async move {
                                let _ = FlvRecorder::record(rec_src, &rec_base, rec_stop).await;
                            });
                        }
                    }
                }
            } else if let Some(stop) = stops.lock().await.remove(&source_id) {
                stop.notify_waiters();
                control.unmark_recording(&source_id);
            }
        }
    });
}

#[tokio::test]
async fn vod_flv_playback_with_range_and_dir_listing() {
    let _ = std::fs::remove_dir_all(RECORD_BASE);

    let mgr = Arc::new(MediaSourceManager::new());
    let auth = StreamAuth::new(false, String::new());
        let hook = HookClient::empty();
    let (recorder, cmd_rx) = RecorderControl::new();
    let recorder = Arc::new(recorder);
    let proxy = Arc::new(StreamProxyControl::new().0);

    let port = TEST_PORT;
    let srv = HttpServer::new(HttpServerConfig {
        addr: format!("127.0.0.1:{}", port),
        source_manager: mgr.clone(),
        auth,
        hook,
        recorder: recorder.clone(),
        proxy,
        record_root: std::path::PathBuf::from(RECORD_BASE),
        www_root: None,
        ssl_cert: None,
        ssl_key: None,
    })
    .await
    .expect("HttpServer start");

    tokio::spawn(async move {
        let _ = srv.run().await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    run_supervisor(
        cmd_rx,
        mgr.clone(),
        recorder.clone(),
        RECORD_BASE.to_string(),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Publish a stream and record it to FLV.
    let source = mgr.get_or_create("__defaultVhost__", "live", "vodtest");
    let cfg = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, h264_config_data().into(), false);
    source.publish_and_cache(cfg).await;
    let key = MediaFrame::new_video(
        0,
        CodecId::H264,
        0,
        0,
        0,
        vec![
            0x17, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x65, 0x9a, 0x00, 0x15, 0x20,
        ]
        .into(),
        true,
    );
    source.publish_and_cache(key).await;

    recorder.start("__defaultVhost__", "live", "vodtest", false, true, false);
    tokio::time::sleep(Duration::from_millis(100)).await;

    for i in 0..3 {
        let frame = MediaFrame::new_video(
            0,
            CodecId::H264,
            (i + 1) * 100,
            ((i + 1) * 100) as u64,
            ((i + 1) * 100) as u64,
            vec![
                0x17, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x05, 0x65, 0x9a, 0x00, 0x15, 0x20,
            ]
            .into(),
            true,
        );
        source.publish_and_cache(frame).await;
    }

    recorder.stop("__defaultVhost__", "live", "vodtest");
    tokio::time::sleep(Duration::from_millis(150)).await;

    // Locate the recorded FLV file.
    let dir = Path::new(RECORD_BASE).join("live").join("vodtest");
    assert!(dir.exists(), "recordings dir should exist");
    let flv_name = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .find_map(|e| {
            let p = e.path();
            if p.extension().map(|x| x == "flv").unwrap_or(false) {
                Some(p.file_name().unwrap().to_string_lossy().to_string())
            } else {
                None
            }
        })
        .expect("a recorded FLV file should exist");

    let vod_path = format!("/record/live/vodtest/{}", flv_name);

    // 1) Full-file playback.
    let (code, body, _) = http_request(
        &format!("GET {} HTTP/1.0\r\nHost: localhost\r\n\r\n", vod_path),
        port,
    )
    .await;
    assert_eq!(code, 200, "VOD FLV should return 200");
    assert_eq!(&body[..3], b"FLV", "VOD should serve a valid FLV");
    let total = body.len() as u64;
    assert!(total > 100, "recorded file should be non-trivial");

    // 2) Range request (seek).
    let (code, body, resp) = http_request(
        &format!("GET {} HTTP/1.0\r\nHost: localhost\r\nRange: bytes=0-99\r\n\r\n", vod_path),
        port,
    )
    .await;
    assert_eq!(code, 206, "Range request should return 206");
    assert_eq!(body.len(), 100, "Range body should be exactly 100 bytes");
    assert!(
        resp.contains("Content-Range: bytes 0-99/"),
        "Response should carry Content-Range"
    );

    // 3) Directory listing.
    let (code, body, _) = http_request("GET /record/live/vodtest/ HTTP/1.0\r\n\r\n", port).await;
    assert_eq!(code, 200, "directory listing should return 200");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains(&flv_name),
        "directory listing should contain the recorded file"
    );

    // 4) Path traversal is rejected.
    let (code, _, _) = http_request(
        "GET /record/../Cargo.toml HTTP/1.0\r\n\r\n",
        port,
    )
    .await;
    assert_eq!(code, 404, "path traversal should be rejected");

    source.close();
    let _ = std::fs::remove_dir_all(RECORD_BASE);
}

/// Returns an unused TCP port by binding to port 0.
async fn find_free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test]
async fn static_file_serving_from_www_root() {
    let www_temp = tempfile::TempDir::new().expect("temp dir for www_root");
    // Create a test hierarchy: /css/style.css and /img/dot.png
    let css_dir = www_temp.path().join("css");
    tokio::fs::create_dir(&css_dir).await.unwrap();
    tokio::fs::write(css_dir.join("style.css"), b"body { color: red; }\n")
        .await
        .unwrap();
    let img_dir = www_temp.path().join("img");
    tokio::fs::create_dir(&img_dir).await.unwrap();
    tokio::fs::write(img_dir.join("dot.png"), b"\x89PNG\x0d\x0a\x1a\x0a")
        .await
        .unwrap();
    // Put a plain-text file at the root
    tokio::fs::write(www_temp.path().join("hello.txt"), b"hello world\n")
        .await
        .unwrap();

    let mgr = Arc::new(MediaSourceManager::new());
    let auth = StreamAuth::new(false, String::new());
        let hook = HookClient::empty();
    let (recorder, _cmd_rx) = RecorderControl::new();
    let recorder = Arc::new(recorder);
    let proxy = Arc::new(StreamProxyControl::new().0);

    let port = find_free_port().await;
    let www_root = www_temp.path().to_path_buf();
    let srv = HttpServer::new(HttpServerConfig {
        addr: format!("127.0.0.1:{}", port),
        source_manager: mgr.clone(),
        auth,
        hook,
        recorder: recorder.clone(),
        proxy,
        record_root: std::path::PathBuf::from("/tmp/zlmediakit_vod_static_nonexistent"),
        www_root: Some(www_root.clone()),
        ssl_cert: None,
        ssl_key: None,
    })
    .await
    .expect("HttpServer start");

    tokio::spawn(async move {
        let _ = srv.run().await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 1) Serve a file with correct MIME type.
    let (code, body, _) =
        http_request("GET /hello.txt HTTP/1.0\r\nHost: localhost\r\n\r\n", port).await;
    assert_eq!(code, 200, "static file should return 200");
    assert_eq!(&body, b"hello world\n", "static file content mismatch");

    // 2) Serve CSS with correct MIME type.
    let (code, _, resp) =
        http_request("GET /css/style.css HTTP/1.0\r\nHost: localhost\r\n\r\n", port).await;
    assert_eq!(code, 200);
    assert!(
        resp.contains("Content-Type: text/css"),
        "CSS should get text/css MIME"
    );

    // 3) Serve PNG with correct MIME type.
    let (code, _, resp) =
        http_request("GET /img/dot.png HTTP/1.0\r\nHost: localhost\r\n\r\n", port).await;
    assert_eq!(code, 200);
    assert!(
        resp.contains("Content-Type: image/png"),
        "PNG should get image/png MIME"
    );

    // 4) Range request on static file.
    let (code, body, resp) = http_request(
        "GET /hello.txt HTTP/1.0\r\nHost: localhost\r\nRange: bytes=0-4\r\n\r\n",
        port,
    )
    .await;
    assert_eq!(code, 206, "Range request on static file should return 206");
    assert_eq!(&body, b"hello", "Range body mismatch");
    assert!(
        resp.contains("Content-Range: bytes 0-4/12"),
        "Static Range response should carry Content-Range"
    );

    // 5) Directory listing at www root.
    let (code, body, _) =
        http_request("GET / HTTP/1.0\r\nHost: localhost\r\n\r\n", port).await;
    assert_eq!(code, 200, "www root dir listing should return 200");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("hello.txt") && text.contains("css") && text.contains("img"),
        "www root listing should contain all entries"
    );

    // 6) Path traversal from www_root is rejected.
    let (code, _, _) =
        http_request("GET /../etc/passwd HTTP/1.0\r\nHost: localhost\r\n\r\n", port).await;
    assert_eq!(code, 404, "path traversal from www_root should be rejected");
}

#[tokio::test]
async fn vod_auth_rejects_without_valid_sign() {
    let _ = std::fs::remove_dir_all(RECORD_BASE_AUTH);

    let secret = "test-auth-secret";
    let mgr = Arc::new(MediaSourceManager::new());
    let auth = StreamAuth::new(true, secret.to_string());
        let hook = HookClient::empty();
    let (recorder, cmd_rx) = RecorderControl::new();
    let recorder = Arc::new(recorder);
    let proxy = Arc::new(StreamProxyControl::new().0);

    let port = find_free_port().await;
    let srv = HttpServer::new(HttpServerConfig {
        addr: format!("127.0.0.1:{}", port),
        source_manager: mgr.clone(),
        auth,
        hook,
        recorder: recorder.clone(),
        proxy,
        record_root: std::path::PathBuf::from(RECORD_BASE_AUTH),
        www_root: None,
        ssl_cert: None,
        ssl_key: None,
    })
    .await
    .expect("HttpServer start");

    tokio::spawn(async move {
        let _ = srv.run().await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    run_supervisor(
        cmd_rx,
        mgr.clone(),
        recorder.clone(),
        RECORD_BASE_AUTH.to_string(),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Publish and record a stream.
    let source = mgr.get_or_create("__defaultVhost__", "live", "vodtest");
    let cfg = MediaFrame::new_video(0, CodecId::H264, 0, 0, 0, h264_config_data().into(), false);
    source.publish_and_cache(cfg).await;
    let key = MediaFrame::new_video(
        0, CodecId::H264, 0, 0, 0,
        vec![0x17, 0x01, 0x00, 0x00, 0x00, 0x05, 0x65, 0x9a, 0x00, 0x15, 0x20].into(),
        true,
    );
    source.publish_and_cache(key).await;
    recorder.start("__defaultVhost__", "live", "vodtest", false, true, false);
    tokio::time::sleep(Duration::from_millis(100)).await;
    recorder.stop("__defaultVhost__", "live", "vodtest");
    tokio::time::sleep(Duration::from_millis(150)).await;

    let dir = Path::new(RECORD_BASE_AUTH).join("live").join("vodtest");
    let flv_name = std::fs::read_dir(&dir)
        .expect("read dir")
        .filter_map(|e| e.ok())
        .find_map(|e| {
            let p = e.path();
            if p.extension().map(|x| x == "flv").unwrap_or(false) {
                Some(p.file_name().unwrap().to_string_lossy().to_string())
            } else {
                None
            }
        })
        .expect("a recorded FLV file should exist");

    let vod_path = format!("/record/live/vodtest/{}", flv_name);

    // 1) Without sign → 401.
    let (code, _, _) =
        http_request(&format!("GET {} HTTP/1.0\r\nHost: localhost\r\n\r\n", vod_path), port).await;
    assert_eq!(code, 401, "VOD without sign should be rejected when auth_enabled");

    // 2) With invalid sign → 401.
    let (code, _, _) = http_request(
        &format!("GET {}?sign=badtoken HTTP/1.0\r\nHost: localhost\r\n\r\n", vod_path),
        port,
    )
    .await;
    assert_eq!(code, 401, "VOD with bad sign should be rejected");

    // 3) With valid sign → 200.
    let stream_path = format!("live/vodtest/{}", flv_name);
    let valid_sign = generate_sign(secret, &["__defaultVhost__", "record", &stream_path, "play"]);
    let (code, body, _) = http_request(
        &format!("GET {}?sign={} HTTP/1.0\r\nHost: localhost\r\n\r\n", vod_path, valid_sign),
        port,
    )
    .await;
    assert_eq!(code, 200, "VOD with valid sign should succeed");
    assert_eq!(&body[..3], b"FLV", "VOD with valid sign should serve FLV");

    // 4) Directory listing without sign → 401.
    let (code, _, _) =
        http_request("GET /record/live/vodtest/ HTTP/1.0\r\nHost: localhost\r\n\r\n", port).await;
    assert_eq!(code, 401, "VOD dir listing without sign should be rejected when auth_enabled");

    // 5) Directory listing with valid sign → 200.
    let dir_sign = generate_sign(secret, &["__defaultVhost__", "record", "live/vodtest", "play"]);
    let (code, body, _) = http_request(
        &format!("GET /record/live/vodtest/?sign={} HTTP/1.0\r\nHost: localhost\r\n\r\n", dir_sign),
        port,
    )
    .await;
    assert_eq!(code, 200, "VOD dir listing with valid sign should succeed");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains(&flv_name),
        "auth-protected dir listing should contain the file"
    );

    source.close();
    let _ = std::fs::remove_dir_all(RECORD_BASE_AUTH);
}
