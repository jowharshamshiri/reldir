//! Any key: its file name is safe, decodes back to it, and fits or is refused.
#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    reldir::fuzzing::file_name(data);
});
