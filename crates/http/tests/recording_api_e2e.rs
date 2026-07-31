use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Notify};
use tokio::time::Duration;
use zlmediakit_core::auth::StreamAuth;
use zlmediakit_core::hook::HookClient;
use zlmediakit_core::media_frame::{CodecId, MediaFrame};
use zlmediakit_core::media_source::MediaSourceManager;
use zlmediakit_core::recorder::{RecorderCommand, RecorderControl};
use zlmediakit_core::stream_proxy::StreamProxyControl;
use zlmediakit_flv::FlvRecorder;
use zlmediakit_http::server::{HttpServer, HttpServerConfig};

const TEST_PORT: u16 = 19157;

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

fn h264_config_data() -> Vec<u8> {
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

const RECORD_BASE: &str = "/tmp/zlmediakit_recording_api_test";

/// Run a minimal recorder supervisor that processes RecorderCommands.
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
            } else {
                if let Some(stop) = stops.lock().await.remove(&source_id) {
                    stop.notify_waiters();
                }
                control.unmark_recording(&source_id);
            }
        }
    });
}

#[tokio::test]
async fn recording_api_start_stop_via_http() {
    let _ = std::fs::remove_dir_all(RECORD_BASE);

    let mgr = Arc::new(MediaSourceManager::new(None));
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
        pusher: Default::default(),
        ffmpeg: Default::default(),
        record_root: std::path::PathBuf::from(RECORD_BASE),
        www_root: None,
        ssl_cert: None,
        ssl_key: None,
        rtp: None,
        sip: None,
        transcode: None,
        session_manager: None,
    })
    .await
    .expect("HttpServer start");

    tokio::spawn(async move {
        let _ = srv.run().await;
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Start the recorder supervisor
    run_supervisor(
        cmd_rx,
        mgr.clone(),
        recorder.clone(),
        RECORD_BASE.to_string(),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Publish a stream
    let source = mgr.get_or_create("__defaultVhost__", "live", "recordtest");
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

    // Verify stream exists
    let (code, body) = http_get("/index/api/getMediaList", port).await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["result"].as_array().unwrap().len(), 1);

    // Start FLV recording via HTTP API
    let (code, body) = http_get(
        "/index/api/startRecord?app=live&stream=recordtest&type=flv",
        port,
    )
    .await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0, "startRecord should succeed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Check recording state
    let (code, body) = http_get("/index/api/isRecording?app=live&stream=recordtest", port).await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0);
    assert!(
        json["result"]["recording"].as_bool().unwrap_or(false),
        "should be recording"
    );
    assert!(
        json["result"]["flv"].as_bool().unwrap_or(false),
        "FLV recording should be active"
    );
    assert!(
        !json["result"]["mp4"].as_bool().unwrap_or(true),
        "MP4 should not be recording"
    );

    // Publish a few frames while recording
    for i in 0..2 {
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

    // Stop recording via HTTP API
    let (code, body) = http_get("/index/api/stopRecord?app=live&stream=recordtest", port).await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], 0, "stopRecord should succeed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Verify recording stopped
    let (code, body) = http_get("/index/api/isRecording?app=live&stream=recordtest", port).await;
    assert_eq!(code, 200);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        !json["result"]["recording"].as_bool().unwrap_or(true),
        "should not be recording after stop"
    );

    // Verify FLV file was created
    let dir_path = std::path::Path::new(RECORD_BASE)
        .join("live")
        .join("recordtest");
    assert!(dir_path.exists(), "recordings dir should exist");
    let entries = std::fs::read_dir(&dir_path).expect("read recordings dir");
    let flv_files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "flv").unwrap_or(false))
        .collect();
    assert_eq!(flv_files.len(), 1, "should have one FLV file");
    let data = std::fs::read(&flv_files[0].path()).expect("read flv file");
    assert_eq!(&data[..3], b"FLV", "should be valid FLV file");

    source.close();
    let _ = std::fs::remove_dir_all(RECORD_BASE);
}
