//! SRT ingest server — listens for incoming SRT connections and
//! publishes the received stream to `MediaSourceManager`.

use crate::ffi;
use std::ffi::c_int;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, info, warn};
use zlmediakit_core::media_frame::{CodecId, MediaFrame};
use zlmediakit_core::media_source::MediaSourceManager;

/// Configuration for the SRT server.
pub struct SrtServerConfig {
    pub addr: String,
    pub latency_ms: u32,
    pub passphrase: Option<String>,
    pub source_manager: Arc<MediaSourceManager>,
}

/// An SRT ingest server. Binds a UDP port, listens for SRT connections,
/// and publishes incoming data to the MediaSourceManager.
pub struct SrtServer {
    config: SrtServerConfig,
    started: bool,
}

impl SrtServer {
    pub fn new(config: SrtServerConfig) -> Self {
        Self {
            config,
            started: false,
        }
    }

    /// Starts the SRT server. This will block the calling thread (use in
    /// a dedicated `tokio::task::spawn_blocking`).
    pub fn run_blocking(&mut self, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
        if self.started {
            anyhow::bail!("SRT server already started");
        }
        self.started = true;

        // Initialize SRT
        let ret = unsafe { ffi::srt_startup() };
        if ret != 0 {
            anyhow::bail!("srt_startup failed: {}", ffi::last_error());
        }

        let addr: SocketAddr = self.config.addr.parse()?;

        // Create listener socket
        let sock = unsafe { ffi::srt_create_socket() };
        if sock < 0 {
            anyhow::bail!("srt_create_socket failed: {}", ffi::last_error());
        }

        // Set socket options
        ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_TRANSTYPE, ffi::SRTT_LIVE)?;
        ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_LATENCY, self.config.latency_ms as c_int)?;
        ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_RCVLATENCY, self.config.latency_ms as c_int)?;
        ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_RCVSYN, 0)?;
        ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_RCVBUF, 1024 * 1024)?;

        if let Some(ref pw) = self.config.passphrase {
            ffi::set_sockflag_str(sock, ffi::SRT_SOCKOPT_PASSPHRASE, pw)?;
            ffi::set_sockflag_int(sock, ffi::SRT_SOCKOPT_PBKEYLEN, 16)?;
        }

        // Bind
        let (sockaddr, sockaddr_len) = ffi::socket_addr_to_sockaddr(&addr)?;
        let ret = unsafe {
            ffi::srt_bind(
                sock,
                &sockaddr as *const libc::sockaddr_in as *const libc::sockaddr,
                sockaddr_len as c_int,
            )
        };
        if ret != ffi::SRT_SUCCESS {
            let err = ffi::last_error();
            unsafe { ffi::srt_close(sock) };
            anyhow::bail!("srt_bind({}) failed: {}", addr, err);
        }

        // Listen
        let ret = unsafe { ffi::srt_listen(sock, 5) };
        if ret != ffi::SRT_SUCCESS {
            let err = ffi::last_error();
            unsafe { ffi::srt_close(sock) };
            anyhow::bail!("srt_listen failed: {}", err);
        }

        info!("SRT server listening on {}", addr);

        let sm = self.config.source_manager.clone();

        // Accept loop
        loop {
            // Check stop signal
            if stop.load(Ordering::Relaxed) {
                break;
            }

            let accept_fd = unsafe { ffi::srt_accept(sock, std::ptr::null_mut(), std::ptr::null_mut()) };

            if accept_fd < 0 {
                let state = unsafe { ffi::srt_getsockstate(sock) };
                if state == ffi::SRTS_BROKEN {
                    error!("SRT listener socket broken");
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }

            info!("SRT client connected (fd={})", accept_fd);

            let sm2 = sm.clone();
            tokio::spawn(async move {
                handle_srt_connection(accept_fd, sm2).await;
            });
        }

        unsafe { ffi::srt_close(sock) };
        unsafe { ffi::srt_cleanup() };
        info!("SRT server stopped");
        Ok(())
    }
}

/// Handles a single SRT connection: reads data in a loop and publishes
/// it as video frames. `srt_recv` is blocking, so we call it via
/// `spawn_blocking`.
async fn handle_srt_connection(fd: c_int, sm: Arc<MediaSourceManager>) {
    let source = sm.get_or_create("__defaultVhost__", "live", &format!("srt_{}", fd));
    let mut timestamp: u32 = 0;
    let buf_cap = 188 * 50;

    loop {
        let fd_copy = fd;
        let result = tokio::task::spawn_blocking(move || {
            let mut buf = vec![0u8; buf_cap];
            unsafe {
                let n = ffi::srt_recv(
                    fd_copy,
                    buf.as_mut_ptr() as *mut std::ffi::c_void,
                    buf.len() as c_int,
                );
                if n > 0 {
                    buf.truncate(n as usize);
                    Ok(buf)
                } else {
                    Err(n)
                }
            }
        })
        .await;

        match result {
            Ok(Ok(data)) => {
                if data.len() >= 5 {
                    let codec = detect_codec(&data);
                    let key_frame = is_keyframe(&data, codec);
                    let frame = MediaFrame::new_video(
                        0,
                        codec,
                        timestamp,
                        timestamp as u64,
                        timestamp as u64,
                        bytes::Bytes::from(data),
                        key_frame,
                    );
                    source.publish_and_cache(frame).await;
                }
                timestamp += 40;
            }
            Ok(Err(0)) => {
                info!("SRT connection closed (fd={})", fd);
                break;
            }
            Ok(Err(n)) => {
                let err = ffi::last_error();
                warn!("SRT recv error (fd={}): code={}, {}", fd, n, err);
                break;
            }
            Err(e) => {
                warn!("SRT spawn_blocking join error: {}", e);
                break;
            }
        }
    }

    unsafe { ffi::srt_close(fd) };
}

fn detect_codec(data: &[u8]) -> CodecId {
    for w in data.windows(4) {
        if w == [0x00, 0x00, 0x00, 0x01] {
            let offset = w.as_ptr() as usize - data.as_ptr() as usize;
            if let Some(&b) = data.get(offset + 4) {
                let nal_type = b & 0x1F;
                if nal_type > 0 && nal_type <= 21 {
                    return CodecId::H264;
                }
            }
            return CodecId::H264;
        }
    }
    for w in data.windows(3) {
        if w == [0x00, 0x00, 0x01] {
            return CodecId::H264;
        }
    }
    if !data.is_empty() && data[0] == 0x47 {
        return CodecId::Unknown(0);
    }
    CodecId::Unknown(0)
}

fn is_keyframe(data: &[u8], codec: CodecId) -> bool {
    match codec {
        CodecId::H264 => {
            for w in data.windows(5) {
                if (w[0] == 0 && w[1] == 0 && w[2] == 1)
                    || (w[0] == 0 && w[1] == 0 && w[2] == 0 && w[3] == 1)
                {
                    let nal_type = if w[2] == 1 { w[3] & 0x1F } else { w[4] & 0x1F };
                    return nal_type == 5 || nal_type == 7;
                }
            }
            false
        }
        CodecId::H265 => {
            for w in data.windows(6) {
                if (w[0] == 0 && w[1] == 0 && w[2] == 1)
                    || (w[0] == 0 && w[1] == 0 && w[2] == 0 && w[3] == 1)
                {
                    let h = if w[2] == 1 {
                        u16::from_be_bytes([w[3], w[4]])
                    } else {
                        u16::from_be_bytes([w[4], w[5]])
                    };
                    let nal_type = (h >> 1) & 0x3F;
                    return (16..=21).contains(&nal_type);
                }
            }
            false
        }
        _ => true,
    }
}
