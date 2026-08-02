use crate::ffi;
use anyhow::{anyhow, bail, Context, Result};
use std::ffi::{c_int, c_void};
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionMode {
    Caller,
    Rendezvous,
}

#[derive(Clone, Debug)]
pub(crate) struct SrtEndpoint {
    pub host: String,
    pub port: u16,
    pub mode: ConnectionMode,
    pub local_addr: Option<SocketAddr>,
    pub latency_ms: u32,
    pub passphrase: Option<String>,
    pub stream_id: Option<String>,
}

impl SrtEndpoint {
    pub fn parse(url: &str) -> Result<Self> {
        let rest = url
            .strip_prefix("srt://")
            .ok_or_else(|| anyhow!("SRT URL must start with srt://"))?;
        let (location, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (authority, path) = location.find('/').map_or((location, ""), |index| {
            (&location[..index], location[index + 1..].trim_matches('/'))
        });
        let (host, port) = parse_authority(authority)?;

        let mut mode = ConnectionMode::Caller;
        let mut local_ip = None;
        let mut local_port: Option<u16> = None;
        let mut latency_ms = 120;
        let mut passphrase = None;
        let mut stream_id = (!path.is_empty()).then(|| format!("#!::r={path}"));
        for pair in query.split('&').filter(|pair| !pair.is_empty()) {
            let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = percent_decode(value)?;
            match name.to_ascii_lowercase().as_str() {
                "mode" if value.eq_ignore_ascii_case("caller") => mode = ConnectionMode::Caller,
                "mode" if value.eq_ignore_ascii_case("rendezvous") => {
                    mode = ConnectionMode::Rendezvous
                }
                "mode" => bail!("unsupported SRT connection mode: {value}"),
                "localip" | "adapter" => local_ip = Some(value),
                "localport" => local_port = Some(value.parse()?),
                "latency" => latency_ms = value.parse()?,
                "passphrase" => passphrase = Some(value),
                "streamid" => stream_id = Some(value),
                _ => {}
            }
        }
        let local_addr = if mode == ConnectionMode::Rendezvous {
            let port = local_port.ok_or_else(|| {
                anyhow!("SRT rendezvous mode requires a localport query parameter")
            })?;
            let local_ip = local_ip
                .as_deref()
                .unwrap_or(if host.parse::<std::net::Ipv6Addr>().is_ok() {
                    "::"
                } else {
                    "0.0.0.0"
                })
                .parse::<std::net::IpAddr>()?;
            Some(SocketAddr::new(local_ip, port))
        } else {
            None
        };
        Ok(Self {
            host,
            port,
            mode,
            local_addr,
            latency_ms,
            passphrase,
            stream_id,
        })
    }

    pub fn remote_addr(&self) -> Result<SocketAddr> {
        let expected_ip = self.host.parse::<std::net::IpAddr>().ok();
        let addresses = (self.host.as_str(), self.port)
            .to_socket_addrs()
            .context("failed to resolve SRT host")?
            .collect::<Vec<_>>();
        if let Some(expected_ip) = expected_ip {
            return addresses
                .into_iter()
                .find(|address| address.ip() == expected_ip)
                .ok_or_else(|| anyhow!("SRT peer address family did not resolve"));
        }
        addresses
            .iter()
            .copied()
            .find(SocketAddr::is_ipv4)
            .or_else(|| addresses.into_iter().next())
            .ok_or_else(|| anyhow!("SRT peer did not resolve to an IP address"))
    }

    pub fn display_target(&self) -> String {
        if self.host.contains(':') {
            format!("srt://[{}]:{}", self.host, self.port)
        } else {
            format!("srt://{}:{}", self.host, self.port)
        }
    }
}

fn parse_authority(authority: &str) -> Result<(String, u16)> {
    if let Some(bracketed) = authority.strip_prefix('[') {
        let end = bracketed
            .find(']')
            .ok_or_else(|| anyhow!("invalid bracketed IPv6 SRT host"))?;
        let host = bracketed[..end].to_string();
        let port = bracketed[end + 1..]
            .strip_prefix(':')
            .ok_or_else(|| anyhow!("SRT URL requires an explicit port"))?
            .parse()?;
        Ok((host, port))
    } else {
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or_else(|| anyhow!("SRT URL requires host:port"))?;
        if host.is_empty() {
            bail!("SRT URL host is empty");
        }
        Ok((host.to_string(), port.parse()?))
    }
}

fn percent_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let hex = bytes
                .get(index + 1..index + 3)
                .ok_or_else(|| anyhow!("truncated percent escape in SRT URL"))?;
            let hex = std::str::from_utf8(hex)?;
            decoded.push(u8::from_str_radix(hex, 16)?);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    Ok(String::from_utf8(decoded)?)
}

pub(crate) struct SrtSocket(c_int);

impl SrtSocket {
    pub fn connect(endpoint: &SrtEndpoint, sender: bool) -> Result<Self> {
        ffi::ensure_startup()?;
        let socket = unsafe { ffi::srt_create_socket() };
        if socket < 0 {
            bail!("srt_create_socket failed: {}", ffi::last_error());
        }
        let socket = Self(socket);
        socket.configure(endpoint, sender)?;
        let remote_addr = endpoint.remote_addr()?;
        let remote = ffi::socket_addr_to_sockaddr(&remote_addr);
        let result = if endpoint.mode == ConnectionMode::Rendezvous {
            let local_addr = endpoint
                .local_addr
                .ok_or_else(|| anyhow!("SRT rendezvous mode requires a local address"))?;
            let local = ffi::socket_addr_to_sockaddr(&local_addr);
            unsafe {
                ffi::srt_rendezvous(
                    socket.0,
                    local.as_ptr().cast(),
                    local.len() as c_int,
                    remote.as_ptr().cast(),
                    remote.len() as c_int,
                )
            }
        } else {
            unsafe { ffi::srt_connect(socket.0, remote.as_ptr().cast(), remote.len() as c_int) }
        };
        if result != ffi::SRT_SUCCESS {
            bail!(
                "SRT connect to {} failed: {}",
                endpoint.display_target(),
                ffi::last_error()
            );
        }
        let deadline = Instant::now() + Duration::from_millis(1_500);
        loop {
            let state = unsafe { ffi::srt_getsockstate(socket.0) };
            if state == ffi::SRTS_CONNECTED {
                break;
            }
            if state == ffi::SRTS_BROKEN || Instant::now() >= deadline {
                bail!(
                    "SRT socket did not reach connected state (state={state}): {}",
                    ffi::last_error()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Ok(socket)
    }

    fn configure(&self, endpoint: &SrtEndpoint, sender: bool) -> Result<()> {
        ffi::set_sockflag_int(self.0, ffi::SRT_SOCKOPT_TRANSTYPE, ffi::SRTT_LIVE)?;
        ffi::set_sockflag_int(
            self.0,
            ffi::SRT_SOCKOPT_LATENCY,
            endpoint.latency_ms as c_int,
        )?;
        ffi::set_sockflag_int(self.0, ffi::SRT_SOCKOPT_SNDTIMEO, 500)?;
        ffi::set_sockflag_int(self.0, ffi::SRT_SOCKOPT_RCVTIMEO, 200)?;
        ffi::set_sockflag_int(self.0, ffi::SRT_SOCKOPT_CONNTIMEO, 1_000)?;
        ffi::set_sockflag_bool(self.0, ffi::SRT_SOCKOPT_SNDSYN, true)?;
        ffi::set_sockflag_bool(self.0, ffi::SRT_SOCKOPT_RCVSYN, true)?;
        ffi::set_sockflag_bool(self.0, ffi::SRT_SOCKOPT_SENDER, sender)?;
        if endpoint.mode == ConnectionMode::Rendezvous {
            ffi::set_sockflag_bool(self.0, ffi::SRT_SOCKOPT_RENDEZVOUS, true)?;
            ffi::set_sockflag_bool(self.0, ffi::SRT_SOCKOPT_REUSEADDR, true)?;
        }
        if let Some(passphrase) = endpoint.passphrase.as_deref() {
            ffi::set_sockflag_str(self.0, ffi::SRT_SOCKOPT_PASSPHRASE, passphrase)?;
            ffi::set_sockflag_int(self.0, ffi::SRT_SOCKOPT_PBKEYLEN, 16)?;
        }
        if let Some(stream_id) = endpoint.stream_id.as_deref() {
            ffi::set_sockflag_str(self.0, ffi::SRT_SOCKOPT_STREAMID, stream_id)?;
        }
        Ok(())
    }

    pub fn recv(&self, capacity: usize) -> Result<Option<Vec<u8>>> {
        let mut buffer = vec![0u8; capacity];
        let count = unsafe {
            ffi::srt_recvmsg(
                self.0,
                buffer.as_mut_ptr() as *mut c_void,
                buffer.len() as c_int,
            )
        };
        if count > 0 {
            buffer.truncate(count as usize);
            return Ok(Some(buffer));
        }
        if count == 0 {
            return Ok(None);
        }
        let code = ffi::last_error_code();
        if matches!(code, ffi::SRT_EASYNCRCV | ffi::SRT_ETIMEOUT) {
            return Ok(Some(Vec::new()));
        }
        let state = unsafe { ffi::srt_getsockstate(self.0) };
        if matches!(
            state,
            ffi::SRTS_BROKEN | ffi::SRTS_CLOSING | ffi::SRTS_CLOSED
        ) {
            return Ok(None);
        }
        bail!(
            "SRT receive failed (fd={}, state={}, code={}): {}",
            self.0,
            state,
            code,
            ffi::last_error()
        )
    }

    pub fn send_all(&self, data: &[u8]) -> Result<()> {
        let mut offset = 0;
        while offset < data.len() {
            let count = unsafe {
                ffi::srt_sendmsg(
                    self.0,
                    data[offset..].as_ptr() as *const std::ffi::c_char,
                    (data.len() - offset) as c_int,
                    -1,
                    1,
                )
            };
            if count > 0 {
                offset += count as usize;
                continue;
            }
            let code = ffi::last_error_code();
            if matches!(code, ffi::SRT_EASYNCSND | ffi::SRT_ETIMEOUT) {
                continue;
            }
            let state = unsafe { ffi::srt_getsockstate(self.0) };
            bail!(
                "SRT send failed (fd={}, state={}, code={}): {}",
                self.0,
                state,
                code,
                ffi::last_error()
            );
        }
        Ok(())
    }
}

impl Drop for SrtSocket {
    fn drop(&mut self) {
        unsafe {
            ffi::srt_close(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::sync::{Arc, Barrier};

    fn reserve_rendezvous_port() -> u16 {
        let first_candidate = 30_000 + (std::process::id() % 20_000) as u16;
        for offset in 0..1_000u16 {
            let port = 30_000 + (first_candidate - 30_000 + offset) % 20_000;
            if let Ok(first) = UdpSocket::bind(("127.0.0.1", port)) {
                if let Ok(second) = UdpSocket::bind(("127.0.0.2", port)) {
                    drop((first, second));
                    return port;
                }
            }
        }
        panic!("failed to reserve a rendezvous UDP port pair")
    }

    #[test]
    fn parses_caller_url_and_decodes_options() {
        let endpoint = SrtEndpoint::parse(
            "srt://media.example:9000/live/cam?latency=80&passphrase=0123456789&streamid=%23!%3A%3Ar%3Dlive%2Fremote",
        )
        .unwrap();
        assert_eq!(endpoint.host, "media.example");
        assert_eq!(endpoint.port, 9000);
        assert_eq!(endpoint.latency_ms, 80);
        assert_eq!(endpoint.stream_id.as_deref(), Some("#!::r=live/remote"));
        assert_eq!(endpoint.mode, ConnectionMode::Caller);
    }

    #[test]
    fn rendezvous_requires_and_parses_local_port() {
        let endpoint = SrtEndpoint::parse(
            "srt://127.0.0.1:9001?mode=rendezvous&localip=127.0.0.1&localport=9002",
        )
        .unwrap();
        assert_eq!(endpoint.mode, ConnectionMode::Rendezvous);
        assert_eq!(endpoint.local_addr.unwrap().port(), 9002);
    }

    #[test]
    fn parses_ipv6_caller_and_rendezvous_addresses() {
        let caller = SrtEndpoint::parse("srt://[::1]:9000/live/camera").unwrap();
        assert_eq!(caller.remote_addr().unwrap(), "[::1]:9000".parse().unwrap());
        assert_eq!(caller.display_target(), "srt://[::1]:9000");

        let rendezvous =
            SrtEndpoint::parse("srt://[::1]:9001?mode=rendezvous&localip=::1&localport=9002")
                .unwrap();
        assert_eq!(
            rendezvous.local_addr.unwrap(),
            "[::1]:9002".parse().unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ipv6_caller_connects_to_listener() {
        let port = std::net::UdpSocket::bind("[::1]:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = stop.clone();
        let server = tokio::task::spawn_blocking(move || {
            let manager = Arc::new(zlmediakit_core::MediaSourceManager::new(None));
            let mut server = crate::server::SrtServer::new(crate::server::SrtServerConfig {
                addr: format!("[::1]:{port}"),
                latency_ms: 20,
                passphrase: None,
                source_manager: manager,
            });
            server.run_blocking(server_stop)
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let connected = tokio::task::spawn_blocking(move || {
            let endpoint = SrtEndpoint::parse(&format!("srt://[::1]:{port}?latency=20"))?;
            SrtSocket::connect(&endpoint, true)
        })
        .await
        .unwrap();
        let socket = connected.expect("IPv6 SRT caller failed");
        drop(socket);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        tokio::time::timeout(Duration::from_secs(3), server)
            .await
            .expect("IPv6 SRT listener did not stop")
            .expect("IPv6 SRT listener task panicked")
            .expect("IPv6 SRT listener failed");
    }

    #[test]
    fn rendezvous_peers_connect_and_transfer_a_message() {
        let port = reserve_rendezvous_port();
        let first = SrtEndpoint::parse(&format!(
            "srt://127.0.0.2:{port}?mode=rendezvous&localip=127.0.0.1&localport={port}&latency=120"
        ))
        .unwrap();
        let second = SrtEndpoint::parse(&format!(
            "srt://127.0.0.1:{port}?mode=rendezvous&localip=127.0.0.2&localport={port}&latency=120"
        ))
        .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let connected = Arc::new(Barrier::new(2));
        let (received_tx, received_rx) = std::sync::mpsc::channel();
        let expected = vec![0x47; 7 * 188];
        let payload = expected.clone();
        let first_barrier = barrier.clone();
        let first_connected = connected.clone();
        let sender = std::thread::spawn(move || -> Result<()> {
            first_barrier.wait();
            let socket = SrtSocket::connect(&first, true)?;
            first_connected.wait();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                socket.send_all(&payload)?;
                match received_rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(()) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                        if Instant::now() < deadline => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        bail!("rendezvous receiver did not acknowledge the message")
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        bail!("rendezvous receiver stopped before acknowledging the message")
                    }
                }
            }
            Ok(())
        });
        let receiver = std::thread::spawn(move || -> Result<Vec<u8>> {
            barrier.wait();
            let socket = SrtSocket::connect(&second, false)?;
            connected.wait();
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Some(data) = socket.recv(2048)? {
                    if !data.is_empty() {
                        let _ = received_tx.send(());
                        return Ok(data);
                    }
                } else {
                    bail!("rendezvous peer closed before delivering the message");
                }
            }
            bail!("timed out waiting for rendezvous message")
        });
        sender.join().unwrap().unwrap();
        assert_eq!(receiver.join().unwrap().unwrap(), expected);
    }
}
