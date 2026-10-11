//! Any bytes as a migration file: read, or refused.
#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    reldir_core::fuzzing::migration(data);
});
