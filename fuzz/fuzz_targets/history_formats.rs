//! Any bytes as history's binary formats: refused as corrupt, never followed.
#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    reldir_core::fuzzing::history_formats(data);
});
