use bytes::Bytes;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use zlmediakit_core::{CodecId, EmbeddedMediaKit, MediaFrame};

const ACK_BATCH: usize = 128;

fn argument(name: &str, default: usize) -> usize {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg == name {
            return args
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(default);
        }
    }
    default
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let frames = argument("--frames", 50_000);
    let subscribers = argument("--subscribers", 4);
    let min_fps = argument("--min-fps", 20_000);
    anyhow::ensure!(frames > 0, "--frames must be positive");
    anyhow::ensure!(subscribers > 0, "--subscribers must be positive");

    let kit = EmbeddedMediaKit::default();
    let publisher = kit.publisher("__defaultVhost__", "bench", "media-graph");
    let (ack_tx, mut ack_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut readers = Vec::new();
    for _ in 0..subscribers {
        let ack_tx = ack_tx.clone();
        let mut subscription = kit
            .subscribe("__defaultVhost__", "bench", "media-graph")
            .await
            .expect("benchmark source should exist");
        readers.push(tokio::spawn(async move {
            for received in 1..=frames {
                subscription.recv().await?;
                if received.is_multiple_of(ACK_BATCH) || received == frames {
                    let _ = ack_tx.send(());
                }
            }
            Ok::<_, tokio::sync::broadcast::error::RecvError>(())
        }));
    }
    drop(ack_tx);

    let payload = Bytes::from_static(b"benchmark-media-frame");
    let started = Instant::now();
    for sequence in 0..frames {
        publisher
            .publish(MediaFrame::new_video(
                0,
                CodecId::H264,
                sequence as u32,
                sequence as u64,
                sequence as u64,
                payload.clone(),
                sequence.is_multiple_of(25),
            ))
            .await;
        let published = sequence + 1;
        if published.is_multiple_of(ACK_BATCH) || published == frames {
            for _ in 0..subscribers {
                let ack = timeout(Duration::from_secs(5), ack_rx.recv()).await?;
                anyhow::ensure!(
                    ack.is_some(),
                    "benchmark subscriber ended before acknowledging"
                );
            }
        }
    }
    timeout(Duration::from_secs(30), async {
        for reader in readers {
            reader.await??;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    let elapsed = started.elapsed();
    let fps = frames as f64 / elapsed.as_secs_f64();
    let deliveries = frames * subscribers;

    println!(
        "{{\"frames\":{frames},\"subscribers\":{subscribers},\"deliveries\":{deliveries},\"elapsed_ms\":{},\"source_fps\":{fps:.0}}}",
        elapsed.as_millis()
    );
    anyhow::ensure!(
        fps >= min_fps as f64,
        "media graph throughput {fps:.0} fps is below baseline {min_fps} fps"
    );
    assert!(publisher.unpublish());
    Ok(())
}
