#![no_main]

use libfuzzer_sys::fuzz_target;
use zlmediakit_flv::FlvDemuxer;

fuzz_target!(|data: &[u8]| {
    let mut demuxer = FlvDemuxer::new();
    let mut header_parsed = false;

    for chunk in data.chunks(257) {
        demuxer.feed(chunk);
        if !header_parsed {
            header_parsed = demuxer.parse_header().is_some();
        }
        if header_parsed {
            while demuxer.parse_tag().is_some() {}
        }
    }
});
