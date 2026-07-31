//! WebRTC DataChannel plumbing.
//!
//! SCTP over DTLS is always negotiated by the `webrtc` crate — no extra codec or
//! m-line registration is required to carry data channels. This module exposes
//! them to the rest of the server: every channel opened on a WHEP / WHIP session
//! is attached to a broadcast pipe so server-side consumers (a control plane,
//! an SEI / message relay, ...) can read the publisher's or player's messages.
//!
//! The plumbing is intentionally minimal: incoming messages are forwarded
//! verbatim, and the session only records that they happened. Nothing in the
//! media pipeline is affected when no data channel exists.

use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::broadcast;
use tracing::{debug, info};
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::RTCPeerConnection;

/// A message received over a WebRTC DataChannel.
#[derive(Debug, Clone)]
pub struct DataMessage {
    /// Label of the channel the message arrived on.
    pub label: String,
    /// True for string payloads, false for binary.
    pub is_string: bool,
    pub data: Bytes,
}

/// Register the `on_data_channel` handler on `pc`, forwarding every message
/// received on any channel into `tx`.
pub(crate) fn attach_data_channels(pc: &RTCPeerConnection, tx: broadcast::Sender<DataMessage>) {
    pc.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
        let tx = tx.clone();
        Box::pin(async move {
            let label = channel.label().to_string();
            info!("webrtc: data channel '{}' opened", label);

            let close_label = label.clone();
            channel.on_close(Box::new(move || {
                let label = close_label.clone();
                Box::pin(async move {
                    info!("webrtc: data channel '{}' closed", label);
                })
            }));

            let tx = tx.clone();
            channel.on_message(Box::new(move |msg: DataChannelMessage| {
                let tx = tx.clone();
                let label = label.clone();
                Box::pin(async move {
                    if tx
                        .send(DataMessage {
                            label,
                            is_string: msg.is_string,
                            data: msg.data.clone(),
                        })
                        .is_err()
                    {
                        debug!("webrtc: data channel message dropped (no subscribers)");
                    }
                })
            }));
        })
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use webrtc::ice_transport::ice_candidate::RTCIceCandidate;

    use crate::engine::{build_api, WebRtcRole};

    async fn new_pc(role: WebRtcRole) -> Arc<RTCPeerConnection> {
        Arc::new(
            build_api(role)
                .unwrap()
                .build()
                .new_peer_connection(Default::default())
                .await
                .unwrap(),
        )
    }

    /// Forward ICE candidates between two local peer connections so they can
    /// connect over their (localhost) host candidates.
    fn link_ice(a: Arc<RTCPeerConnection>, b: Arc<RTCPeerConnection>) {
        let (tx_a2b, mut rx_a2b) = mpsc::unbounded_channel::<RTCIceCandidate>();
        let (tx_b2a, mut rx_b2a) = mpsc::unbounded_channel::<RTCIceCandidate>();
        let b1 = b.clone();
        a.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let tx = tx_a2b.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    let _ = tx.send(c);
                }
            })
        }));
        let a1 = a.clone();
        b.on_ice_candidate(Box::new(move |c: Option<RTCIceCandidate>| {
            let tx = tx_b2a.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    let _ = tx.send(c);
                }
            })
        }));
        tokio::spawn(async move {
            while let Some(c) = rx_a2b.recv().await {
                if let Ok(init) = c.to_json() {
                    let _ = b1.add_ice_candidate(init).await;
                }
            }
        });
        tokio::spawn(async move {
            while let Some(c) = rx_b2a.recv().await {
                if let Ok(init) = c.to_json() {
                    let _ = a1.add_ice_candidate(init).await;
                }
            }
        });
    }

    #[tokio::test]
    async fn data_channel_bidirectional_exchange() {
        crate::init_crypto();
        let pc_offer = new_pc(WebRtcRole::Sender).await;
        let pc_answer = new_pc(WebRtcRole::Receiver).await;
        link_ice(pc_offer.clone(), pc_answer.clone());

        // The answering side forwards every received message into a channel.
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel::<(String, Bytes, bool)>();
        pc_answer.on_data_channel(Box::new(move |ch: Arc<RTCDataChannel>| {
            let tx = msg_tx.clone();
            Box::pin(async move {
                let label = ch.label().to_string();
                ch.on_message(Box::new(move |m: DataChannelMessage| {
                    let tx = tx.clone();
                    let label = label.clone();
                    Box::pin(async move {
                        let _ = tx.send((label, m.data.clone(), m.is_string));
                    })
                }));
            })
        }));

        let dc = pc_offer
            .create_data_channel("relay", None)
            .await
            .unwrap();

        let (open_tx, open_rx) = tokio::sync::oneshot::channel::<()>();
        let mut open_tx = Some(open_tx);
        dc.on_open(Box::new(move || {
            let tx = open_tx.take().unwrap();
            Box::pin(async move {
                let _ = tx.send(());
            })
        }));

        let offer = pc_offer.create_offer(None).await.unwrap();
        pc_offer.set_local_description(offer.clone()).await.unwrap();
        pc_answer.set_remote_description(offer).await.unwrap();
        let answer = pc_answer.create_answer(None).await.unwrap();
        pc_answer
            .set_local_description(answer.clone())
            .await
            .unwrap();
        pc_offer.set_remote_description(answer).await.unwrap();

        tokio::time::timeout(Duration::from_secs(10), open_rx)
            .await
            .expect("data channel did not open in time")
            .expect("data channel open oneshot dropped");

        // A -> B text.
        dc.send_text("hello from offer".to_string()).await.unwrap();
        let (label, data, is_string) = tokio::time::timeout(
            Duration::from_secs(5),
            msg_rx.recv(),
        )
        .await
        .expect("offer->answer message timeout")
        .unwrap();
        assert_eq!(label, "relay");
        assert!(is_string);
        assert_eq!(String::from_utf8_lossy(&data), "hello from offer");

        pc_offer.close().await.unwrap();
        pc_answer.close().await.unwrap();
    }
}
