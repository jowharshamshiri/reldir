//! Any bytes: parsing never panics, and every pointer the locator maps
//! resolves to a position inside the input.
#![no_main]
libfuzzer_sys::fuzz_target!(|data: &[u8]| {
    reldir::fuzzing::json_and_locator(data);
});
