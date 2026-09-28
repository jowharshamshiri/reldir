//! Any bytes as a schema file: decoding reports faults, never panics.
#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    reldir::fuzzing::schema_decode(data);
});
