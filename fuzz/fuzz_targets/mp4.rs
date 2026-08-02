#![no_main]

use libfuzzer_sys::fuzz_target;
use zlmediakit_mp4::Mp4Demuxer;

fuzz_target!(|data: &[u8]| {
    let _ = Mp4Demuxer::from_bytes(data);
});
