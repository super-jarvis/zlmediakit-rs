use bytes::Bytes;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::timeout;
use zlmediakit_core::{CodecId, EmbeddedMediaKit, MediaFrame};

const STREAMS_PER_CYCLE: usize = 8;
const SUBSCRIBERS_PER_STREAM: usize = 2;
const FRAMES_PER_STREAM: usize = 128;

fn soak_seconds() -> u64 {
    env::var("ZLM_SOAK_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(30)
}

fn frame(cycle: usize, sequence: usize) -> MediaFrame {
    let timestamp = cycle
        .saturating_mul(FRAMES_PER_STREAM)
        .saturating_add(sequence);
    MediaFrame::new_video(
        0,
        CodecId::H264,
        timestamp as u32,
        timestamp as u64,
        timestamp as u64,
        Bytes::from_static(b"soak-frame"),
        sequence.is_multiple_of(32),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "bounded long-running reliability gate; set ZLM_SOAK_SECONDS"]
async fn media_graph_repeated_publish_subscribe_unpublish_has_no_leaks() {
    let kit = Arc::new(EmbeddedMediaKit::default());
    let deadline = Instant::now() + Duration::from_secs(soak_seconds());
    let mut cycles = 0usize;

    while Instant::now() < deadline {
        let mut readers = Vec::with_capacity(STREAMS_PER_CYCLE * SUBSCRIBERS_PER_STREAM);
        let mut publishers = Vec::with_capacity(STREAMS_PER_CYCLE);

        for stream_index in 0..STREAMS_PER_CYCLE {
            let stream = format!("soak-{stream_index}");
            let publisher = kit.publisher("__defaultVhost__", "soak", &stream);
            for _ in 0..SUBSCRIBERS_PER_STREAM {
                let mut subscription = kit
                    .subscribe("__defaultVhost__", "soak", &stream)
                    .await
                    .expect("publisher should register the source");
                readers.push(tokio::spawn(async move {
                    let mut previous = None;
                    for _ in 0..FRAMES_PER_STREAM {
                        let timestamp = subscription.recv().await.unwrap().timestamp;
                        if let Some(previous) = previous {
                            assert!(timestamp > previous, "frames must remain ordered");
                        }
                        previous = Some(timestamp);
                    }
                }));
            }
            publishers.push(publisher);
        }

        for sequence in 0..FRAMES_PER_STREAM {
            for publisher in &publishers {
                publisher.publish(frame(cycles, sequence)).await;
            }
            if sequence.is_multiple_of(16) {
                tokio::task::yield_now().await;
            }
        }

        timeout(Duration::from_secs(10), async {
            for reader in readers {
                reader.await.unwrap();
            }
        })
        .await
        .expect("soak subscribers stopped making progress");

        for publisher in publishers {
            assert!(publisher.unpublish());
        }
        assert_eq!(kit.media_source_manager().count(), 0);
        assert!(kit.list_sources().await.is_empty());
        cycles += 1;
    }

    assert!(cycles > 0, "soak test must complete at least one cycle");
    eprintln!("media graph soak completed {cycles} cycles");
}
