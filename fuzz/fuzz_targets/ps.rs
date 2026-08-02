#![no_main]

use libfuzzer_sys::fuzz_target;
use zlmediakit_codec::ps::PsDemuxer;

fuzz_target!(|data: &[u8]| {
    let mut demuxer = PsDemuxer::new();
    for chunk in data.chunks(257) {
        demuxer.push(chunk);
        while demuxer.next_pes().is_some() {}
        assert!(demuxer.pending() <= 4 * 1024 * 1024);
    }
});
