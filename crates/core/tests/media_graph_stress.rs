use bytes::Bytes;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;
use zlmediakit_core::{CodecId, EmbeddedMediaKit, MediaFrame};

const STREAMS: usize = 32;
const SUBSCRIBERS_PER_STREAM: usize = 4;
const FRAMES_PER_STREAM: usize = 200;

fn frame(sequence: usize) -> MediaFrame {
    MediaFrame::new_video(
        0,
        CodecId::H264,
        sequence as u32,
        sequence as u64,
        sequence as u64,
        Bytes::from_static(b"stress-frame"),
        sequence.is_multiple_of(25),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_media_graph_delivers_without_leaks() {
    let kit = Arc::new(EmbeddedMediaKit::default());
    let mut readers = Vec::new();
    let mut writers = Vec::new();

    for stream_index in 0..STREAMS {
        let stream = format!("stress-{stream_index}");
        let publisher = kit.publisher("__defaultVhost__", "stress", &stream);

        for _ in 0..SUBSCRIBERS_PER_STREAM {
            let mut subscription = kit
                .subscribe("__defaultVhost__", "stress", &stream)
                .await
                .expect("publisher should register the media source");
            readers.push(tokio::spawn(async move {
                let mut last_timestamp = None;
                for _ in 0..FRAMES_PER_STREAM {
                    let current = subscription.recv().await.unwrap().timestamp;
                    if let Some(previous) = last_timestamp {
                        assert!(current > previous, "frames must remain ordered");
                    }
                    last_timestamp = Some(current);
                }
            }));
        }

        writers.push(tokio::spawn(async move {
            for sequence in 0..FRAMES_PER_STREAM {
                publisher.publish(frame(sequence)).await;
                if sequence.is_multiple_of(32) {
                    tokio::task::yield_now().await;
                }
            }
            publisher
        }));
    }

    timeout(Duration::from_secs(10), async {
        for writer in writers {
            assert!(writer.await.unwrap().unpublish());
        }
        for reader in readers {
            reader.await.unwrap();
        }
    })
    .await
    .expect("media graph stress run timed out");

    assert_eq!(kit.media_source_manager().count(), 0);
    assert!(kit.list_sources().await.is_empty());
}
