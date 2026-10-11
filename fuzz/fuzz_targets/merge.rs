//! Any three documents: the merge driver merges or marks conflicts, never panics, and keeping
//! "ours" leaves JSON.
#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    reldir_core::fuzzing::merge(data);
});
