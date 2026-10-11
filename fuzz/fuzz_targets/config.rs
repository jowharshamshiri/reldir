//! Any bytes as `.db/config`: accepted and valid, or refused.
#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    reldir_core::fuzzing::config(data);
});
