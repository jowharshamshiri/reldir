//! Any text as SQL: classification and the authorizer refuse or admit, never
//! panic, and nothing refused reaches the mirror.
#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    reldir::fuzzing::sql_front_end(data);
});
