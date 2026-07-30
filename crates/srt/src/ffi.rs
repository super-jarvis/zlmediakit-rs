//! Minimal FFI bindings to libsrt for SRT ingest.
//!
//! Links directly against `libsrt-gnutls.so`.
//! Only the functions needed for an SRT listener (server) are exposed.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::net::SocketAddr;

// ── constants ────────────────────────────────────────────────────────

pub const SRT_SOCKOPT_TRANSTYPE: c_int = 4;
pub const SRTT_LIVE: c_int = 0;
pub const SRTT_FILE: c_int = 1;

pub const SRT_SOCKOPT_LATENCY: c_int = 5;
pub const SRT_SOCKOPT_RCVSYN: c_int = 6;
pub const SRT_SOCKOPT_PASSPHRASE: c_int = 10;
pub const SRT_SOCKOPT_PBKEYLEN: c_int = 11;

pub const SRT_SOCKOPT_SNDBUF: c_int = 27;
pub const SRT_SOCKOPT_RCVBUF: c_int = 28;

pub const SRT_SOCKOPT_RCVLATENCY: c_int = 33;

// SRT socket states
pub const SRTS_LISTENING: c_int = 4;
pub const SRTS_BROKEN: c_int = 7;
pub const SRTS_CONNECTED: c_int = 5;

// Stream ID socket option
pub const SRT_SOCKOPT_STREAMID: c_int = 8;

// Error codes
pub const SRT_SUCCESS: c_int = 0;
pub const SRT_EASYNCFAIL: c_int = 6002;
pub const SRT_ECONNREJ: c_int = 2001;
pub const SRT_EASYNCRCV: c_int = 6003;

// ── FFI function declarations ────────────────────────────────────────

#[link(name = "srt-gnutls")]
extern "C" {
    pub fn srt_startup() -> c_int;
    pub fn srt_cleanup() -> c_int;

    /// Creates a socket. `family` = 2 (AF_INET), `type` = 1 (SOCK_DGRAM).
    pub fn srt_create_socket() -> c_int;

    /// Bind a socket to an address. Returns SRT error code.
    pub fn srt_bind(u: c_int, name: *const libc::sockaddr, namelen: c_int) -> c_int;

    /// Set socket to listening mode (max `backlog` pending connections).
    pub fn srt_listen(u: c_int, backlog: c_int) -> c_int;

    /// Accept an incoming SRT connection. Returns a new socket fd or SRT_ERROR.
    /// `addr` and `addrlen` receive the peer address (can be NULL).
    pub fn srt_accept(u: c_int, addr: *mut libc::sockaddr, addrlen: *mut c_int) -> c_int;

    /// Receive data. `len` is buffer size. Returns bytes received or SRT_ERROR.
    pub fn srt_recv(u: c_int, buf: *mut c_void, len: c_int) -> c_int;

    /// Receive with message flags. Returns bytes or SRT_ERROR.
    pub fn srt_recvmsg(
        u: c_int,
        buf: *mut c_void,
        len: c_int,
        ttl: c_int,
        inorder: c_int,
    ) -> c_int;

    /// Close a socket.
    pub fn srt_close(u: c_int) -> c_int;

    /// Set a socket option (integer value).
    pub fn srt_setsockopt(
        u: c_int,
        level: c_int,
        optname: c_int,
        optval: *const c_void,
        optlen: c_int,
    ) -> c_int;

    /// Set a socket flag (preferred newer API). Takes value-pointer pair.
    pub fn srt_setsockflag(
        u: c_int,
        optname: c_int,
        optval: *const c_void,
        optlen: c_int,
    ) -> c_int;

    /// Get socket state.
    pub fn srt_getsockstate(u: c_int) -> c_int;

    /// Get a socket option (string value).
    pub fn srt_getsockflag(
        u: c_int,
        optname: c_int,
        optval: *mut c_void,
        optlen: *mut c_int,
    ) -> c_int;

    /// Get last SRT error code (thread-local).
    pub fn srt_getlasterror(_: *mut c_int) -> c_int;

    /// Get last error message (thread-local).
    pub fn srt_getlasterror_str() -> *const c_char;
}

// ── helpers ──────────────────────────────────────────────────────────

/// Returns the last SRT error as a Rust string.
pub fn last_error() -> String {
    unsafe {
        let ptr = srt_getlasterror_str();
        if ptr.is_null() {
            "unknown SRT error".to_string()
        } else {
            CStr::from_ptr(ptr).to_string_lossy().into_owned()
        }
    }
}

/// Sets an integer socket option via `srt_setsockflag`.
pub fn set_sockflag_int(sock: c_int, opt: c_int, val: c_int) -> anyhow::Result<()> {
    let ret = unsafe {
        srt_setsockflag(sock, opt, &val as *const c_int as *const c_void, std::mem::size_of::<c_int>() as c_int)
    };
    if ret != SRT_SUCCESS {
        anyhow::bail!("srt_setsockflag({}) failed: {}", opt, last_error());
    }
    Ok(())
}

/// Sets a string socket option (e.g. passphrase).
pub fn set_sockflag_str(sock: c_int, opt: c_int, val: &str) -> anyhow::Result<()> {
    let bytes = val.as_bytes();
    let ret = unsafe {
        srt_setsockflag(sock, opt, bytes.as_ptr() as *const c_void, bytes.len() as c_int)
    };
    if ret != SRT_SUCCESS {
        anyhow::bail!("srt_setsockflag({}) failed: {}", opt, last_error());
    }
    Ok(())
}

/// Converts a `SocketAddr` to a libc sockaddr_in for SRT bind.
pub fn socket_addr_to_sockaddr(addr: &SocketAddr) -> anyhow::Result<(libc::sockaddr_in, libc::socklen_t)> {
    match addr {
        SocketAddr::V4(v4) => {
            let octets = v4.ip().octets();
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: v4.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(octets),
                },
                sin_zero: [0u8; 8],
            };
            Ok((sin, std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t))
        }
        _ => anyhow::bail!("SRT only supports IPv4 in this implementation"),
    }
}

/// Retrieves the stream ID from an SRT socket (set by the caller via
/// `?streamid=`). Maximum length is 512 bytes.
pub fn get_streamid(sock: c_int) -> Option<String> {
    let mut buf = [0u8; 512];
    let mut len = buf.len() as c_int;
    let ret = unsafe {
        srt_getsockflag(
            sock,
            SRT_SOCKOPT_STREAMID,
            buf.as_mut_ptr() as *mut c_void,
            &mut len,
        )
    };
    if ret != SRT_SUCCESS {
        return None;
    }
    let len = len.max(0) as usize;
    if len == 0 {
        return None;
    }
    let s = String::from_utf8_lossy(&buf[..len]).to_string();
    Some(s)
}

/// Parses an SRT streamid of the form `#!::r=app/stream` and returns
/// `(app, stream)`. Falls back to `("live", "stream")` on parse failure.
pub fn parse_streamid(streamid: &str) -> (String, String) {
    // Common SRT streamid formats:
    //   #!::r=live/stream_name
    //   #!::r=stream_name
    //   app=live/stream=stream_name
    for token in streamid.split(',') {
        if let Some(rest) = token.trim().strip_prefix("r=") {
            let parts: Vec<&str> = rest.splitn(2, '/').collect();
            let app = parts.first().map_or("live", |v| *v);
            let stream = parts.get(1).map_or(rest, |v| *v);
            return (app.to_string(), stream.to_string());
        }
        if let Some(rest) = token.trim().strip_prefix("stream=") {
            return ("live".to_string(), rest.to_string());
        }
        if let Some(rest) = token.trim().strip_prefix("app=") {
            let parts: Vec<&str> = rest.splitn(2, '/').collect();
            let app = parts.first().map_or("live", |v| *v);
            let stream = parts.get(1).map_or("stream", |v| *v);
            return (app.to_string(), stream.to_string());
        }
    }
    // Try parsing as URL path
    if streamid.contains('/') {
        let parts: Vec<&str> = streamid.splitn(2, '/').collect();
        return (parts[0].to_string(), parts.get(1).map_or("stream", |v| *v).to_string());
    }
    ("live".to_string(), streamid.to_string())
}
