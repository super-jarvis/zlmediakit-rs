use bytes::Bytes;
use std::ffi::{c_char, c_int};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};
use std::time::Duration;
use tokio::sync::Notify;
use zlmediakit_core::{CodecId, MediaFrame, MediaSourceManager, PayloadFormat};
use zlmediakit_hls::TsLiveMuxer;
use zlmediakit_srt::{ffi, pull_client};

fn reserve_udp_port() -> u16 {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    socket.local_addr().unwrap().port()
}

fn build_ts_stream() -> Vec<u8> {
    let mut muxer = TsLiveMuxer::new();
    let mut output = Vec::new();
    let mut video_config = MediaFrame::new_video(
        0,
        CodecId::H264,
        0,
        0,
        0,
        Bytes::from_static(&[
            0x17, 0, 0, 0, 0, 0x01, 0x64, 0, 0x0a, 0xff, 0xe1, 0, 4, 0x67, 0x64, 0, 0x0a, 1, 0, 4,
            0x68, 0xee, 0x3c, 0x80,
        ]),
        false,
    )
    .with_payload_format(PayloadFormat::Flv);
    video_config.config_frame = true;
    output.extend(muxer.push_frame(&video_config));
    output.extend(
        muxer.push_frame(
            &MediaFrame::new_video(
                0,
                CodecId::H264,
                40,
                40,
                40,
                Bytes::from_static(&[0, 0, 0, 1, 0x65, 1, 2, 3]),
                true,
            )
            .with_payload_format(PayloadFormat::AnnexB),
        ),
    );
    output.extend(
        muxer.push_frame(
            &MediaFrame::new_video(
                0,
                CodecId::H264,
                80,
                80,
                80,
                Bytes::from_static(&[0, 0, 0, 1, 0x41, 4, 5, 6]),
                false,
            )
            .with_payload_format(PayloadFormat::AnnexB),
        ),
    );
    let mut audio_config = MediaFrame::new_audio(
        1,
        CodecId::AAC,
        0,
        0,
        0,
        Bytes::from_static(&[0xaf, 0, 0x12, 0x10]),
    )
    .with_payload_format(PayloadFormat::Flv);
    audio_config.config_frame = true;
    output.extend(muxer.push_frame(&audio_config));
    for (timestamp, payload) in [
        (40, &[0xaf, 1, 0x11, 0x22, 0x33][..]),
        (80, &[0xaf, 1, 0x44, 0x55, 0x66][..]),
    ] {
        output.extend(
            muxer.push_frame(
                &MediaFrame::new_audio(
                    1,
                    CodecId::AAC,
                    timestamp,
                    timestamp as u64,
                    timestamp as u64,
                    Bytes::copy_from_slice(payload),
                )
                .with_payload_format(PayloadFormat::Flv),
            ),
        );
    }
    output
}

fn run_srt_sender(port: u16, ready: mpsc::Sender<()>, payload: Vec<u8>) -> anyhow::Result<()> {
    ffi::ensure_startup()?;
    let listener = unsafe { ffi::srt_create_socket() };
    if listener < 0 {
        anyhow::bail!("create listener failed: {}", ffi::last_error());
    }
    let result = (|| {
        ffi::set_sockflag_int(listener, ffi::SRT_SOCKOPT_TRANSTYPE, ffi::SRTT_LIVE)?;
        ffi::set_sockflag_bool(listener, ffi::SRT_SOCKOPT_SENDER, true)?;
        ffi::set_sockflag_int(listener, ffi::SRT_SOCKOPT_LATENCY, 40)?;
        let addr: SocketAddr = format!("127.0.0.1:{port}").parse()?;
        let (sockaddr, sockaddr_len) = ffi::socket_addr_to_sockaddr(&addr)?;
        let bind_result = unsafe {
            ffi::srt_bind(
                listener,
                &sockaddr as *const libc::sockaddr_in as *const libc::sockaddr,
                sockaddr_len as c_int,
            )
        };
        if bind_result != ffi::SRT_SUCCESS {
            anyhow::bail!("bind listener failed: {}", ffi::last_error());
        }
        if unsafe { ffi::srt_listen(listener, 1) } != ffi::SRT_SUCCESS {
            anyhow::bail!("listen failed: {}", ffi::last_error());
        }
        ready.send(()).unwrap();
        let accepted =
            unsafe { ffi::srt_accept(listener, std::ptr::null_mut(), std::ptr::null_mut()) };
        if accepted < 0 {
            anyhow::bail!("accept failed: {}", ffi::last_error());
        }
        std::thread::sleep(Duration::from_millis(200));
        for chunk in payload.chunks(188 * 7) {
            let sent = unsafe {
                ffi::srt_sendmsg(
                    accepted,
                    chunk.as_ptr() as *const c_char,
                    chunk.len() as c_int,
                    -1,
                    1,
                )
            };
            if sent != chunk.len() as c_int {
                unsafe { ffi::srt_close(accepted) };
                anyhow::bail!("send failed: {}", ffi::last_error());
            }
        }
        std::thread::sleep(Duration::from_millis(500));
        unsafe { ffi::srt_close(accepted) };
        Ok(())
    })();
    unsafe { ffi::srt_close(listener) };
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn srt_caller_pulls_mpeg_ts_from_listener() {
    let port = reserve_udp_port();
    let (ready_tx, ready_rx) = mpsc::channel();
    let sender = std::thread::spawn(move || run_srt_sender(port, ready_tx, build_ts_stream()));
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();

    let manager = Arc::new(MediaSourceManager::new(None));
    let pull_manager = manager.clone();
    let pull_task = tokio::spawn(async move {
        pull_client::start(
            &format!("srt://127.0.0.1:{port}?latency=40"),
            "__defaultVhost__",
            "live",
            "pulled",
            pull_manager,
            Arc::new(Notify::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), pull_task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    sender.join().unwrap().unwrap();

    let source = manager
        .get("__defaultVhost__", "live", "pulled")
        .expect("SRT pull did not publish a local source");
    let frames = source.get_latest_gop_frames().await;
    let diagnostics = frames
        .iter()
        .map(|frame| (frame.codec, frame.key_frame, frame.data.len()))
        .collect::<Vec<_>>();
    assert!(
        frames
            .iter()
            .any(|frame| frame.codec == CodecId::H264 && frame.key_frame),
        "SRT pull did not publish H.264 key video: {diagnostics:?}"
    );
    assert!(
        frames.iter().any(|frame| frame.codec == CodecId::AAC),
        "SRT pull did not publish AAC audio: {diagnostics:?}"
    );
}
