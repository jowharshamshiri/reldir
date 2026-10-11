//! Any bytes: a document that parses is edited -- every scalar replaced,
//! every member and element removed, a member inserted into every object --
//! without one byte outside the edit changing, and reads back as intended.
#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    reldir_core::json_edit::fuzz_roundtrip(data);
});
