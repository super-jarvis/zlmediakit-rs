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
        let mut local_ip = "0.0.0.0".to_string();
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
                "localip" | "adapter" => local_ip = value,
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
            Some(format!("{local_ip}:{port}").parse()?)
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
        (self.host.as_str(), self.port)
            .to_socket_addrs()
            .context("failed to resolve SRT host")?
            .find(SocketAddr::is_ipv4)
            .ok_or_else(|| anyhow!("SRT currently requires an IPv4 peer address"))
    }

    pub fn display_target(&self) -> String {
        format!("srt://{}:{}", self.host, self.port)
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
        if let Some(local_addr) = endpoint.local_addr {
            socket.bind(&local_addr)?;
        }
        let remote_addr = endpoint.remote_addr()?;
        let (remote, remote_len) = ffi::socket_addr_to_sockaddr(&remote_addr)?;
        let result = unsafe {
            ffi::srt_connect(
                socket.0,
                &remote as *const libc::sockaddr_in as *const libc::sockaddr,
                remote_len as c_int,
            )
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

    fn bind(&self, addr: &SocketAddr) -> Result<()> {
        let (local, local_len) = ffi::socket_addr_to_sockaddr(addr)?;
        let result = unsafe {
            ffi::srt_bind(
                self.0,
                &local as *const libc::sockaddr_in as *const libc::sockaddr,
                local_len as c_int,
            )
        };
        if result != ffi::SRT_SUCCESS {
            bail!("SRT rendezvous bind({addr}) failed: {}", ffi::last_error());
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

    fn reserve_udp_port() -> u16 {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().port()
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
    fn rendezvous_peers_connect_and_transfer_a_message() {
        let first_port = reserve_udp_port();
        let second_port = reserve_udp_port();
        let first = SrtEndpoint::parse(&format!(
            "srt://127.0.0.1:{second_port}?mode=rendezvous&localip=127.0.0.1&localport={first_port}&latency=20"
        ))
        .unwrap();
        let second = SrtEndpoint::parse(&format!(
            "srt://127.0.0.1:{first_port}?mode=rendezvous&localip=127.0.0.1&localport={second_port}&latency=20"
        ))
        .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let connected = Arc::new(Barrier::new(2));
        let first_barrier = barrier.clone();
        let first_connected = connected.clone();
        let sender = std::thread::spawn(move || {
            first_barrier.wait();
            let socket = SrtSocket::connect(&first, true).unwrap();
            first_connected.wait();
            socket.send_all(b"rendezvous").unwrap();
            std::thread::sleep(Duration::from_millis(500));
        });
        let receiver = std::thread::spawn(move || {
            barrier.wait();
            let socket = SrtSocket::connect(&second, false).unwrap();
            connected.wait();
            loop {
                if let Some(data) = socket.recv(2048).unwrap() {
                    if !data.is_empty() {
                        return data;
                    }
                }
            }
        });
        sender.join().unwrap();
        assert_eq!(receiver.join().unwrap(), b"rendezvous");
    }
}
