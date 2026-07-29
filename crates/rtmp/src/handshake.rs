use bytes::{BufMut, BytesMut};
use rand::RngExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

const C0C1_SIZE: usize = 1536;
const C2_SIZE: usize = 1536;

pub struct RtmpHandshake;

impl RtmpHandshake {
    pub fn new() -> Self {
        Self
    }

    pub async fn server_handshake<S: AsyncReadExt + AsyncWriteExt + Unpin>(
        stream: &mut S,
    ) -> anyhow::Result<()> {
        let mut c0c1 = vec![0u8; 1 + C0C1_SIZE];
        stream.read_exact(&mut c0c1).await?;

        if c0c1[0] != 0x03 {
            anyhow::bail!("Invalid RTMP version: {}", c0c1[0]);
        }

        let c1_time = u32::from_be_bytes([c0c1[1], c0c1[2], c0c1[3], c0c1[4]]);
        let c1_version = u32::from_be_bytes([c0c1[5], c0c1[6], c0c1[7], c0c1[8]]);

        debug!("C1 time={}, version={}", c1_time, c1_version);

        let mut s0s1 = BytesMut::with_capacity(1 + C0C1_SIZE);
        s0s1.put_u8(0x03);

        let s1_time = 0u32;
        let s1_version = 0x00090000; // version 9.0.0
        s0s1.put_u32(s1_time);
        s0s1.put_u32(s1_version);
        let mut random = vec![0u8; 1528];
        rand::rng().fill(&mut random);
        s0s1.extend_from_slice(&random);
        stream.write_all(&s0s1).await?;

        let mut s2 = BytesMut::with_capacity(C2_SIZE);
        s2.extend_from_slice(&c0c1[1..]);
        stream.write_all(&s2).await?;

        let mut c2 = vec![0u8; C2_SIZE];
        stream.read_exact(&mut c2).await?;

        debug!("RTMP handshake completed");
        Ok(())
    }

    /// Client-side RTMP handshake.
    ///
    /// 1. Sends C0 (version 3) + C1 (4-byte timestamp, 4-byte zero, 1528 random bytes).
    /// 2. Reads S0 + S1 + S2.
    /// 3. Sends C2 (echo of S1).
    pub async fn client_handshake<S: AsyncReadExt + AsyncWriteExt + Unpin>(
        stream: &mut S,
    ) -> anyhow::Result<()> {
        let mut c0c1 = Vec::with_capacity(1 + C0C1_SIZE);
        c0c1.push(0x03);
        let mut c1 = vec![0u8; C0C1_SIZE];
        let ts = 0u32;
        c1[..4].copy_from_slice(&ts.to_be_bytes());
        rand::rng().fill(&mut c1[8..]);
        c0c1.extend_from_slice(&c1);
        stream.write_all(&c0c1).await?;

        let mut s0s1 = vec![0u8; 1 + C0C1_SIZE];
        stream.read_exact(&mut s0s1).await?;
        if s0s1[0] != 0x03 {
            anyhow::bail!("Invalid server RTMP version: {}", s0s1[0]);
        }

        let mut s2 = vec![0u8; C2_SIZE];
        stream.read_exact(&mut s2).await?;

        let c2 = &s0s1[1..];
        stream.write_all(c2).await?;

        debug!("RTMP client handshake completed");
        Ok(())
    }
}

impl Default for RtmpHandshake {
    fn default() -> Self {
        Self::new()
    }
}
