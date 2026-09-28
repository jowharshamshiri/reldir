//! Any text as a reference path: a parsed path prints back to one that parses
//! to itself.
#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    reldir::fuzzing::reference_path(data);
});
