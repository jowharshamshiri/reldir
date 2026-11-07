//! Where the documentation built into the binary is read from: the
//! repository's `docs/` in a checkout, or the copy packaged with the crate --
//! `crates/reldir/docs` is a link to the repository's, which `cargo package`
//! resolves into files.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("set by cargo"));
    let repository = manifest.join("../../docs");
    let docs = if repository.join("getting-started.md").is_file() {
        repository
    } else {
        manifest.join("docs")
    };
    let docs = docs.canonicalize().expect("the documentation is present");
    println!("cargo:rustc-env=RELDIR_DOCS={}", docs.display());
    // The target triple names the release archive `self update` installs.
    let target = std::env::var("TARGET").expect("set by cargo");
    println!("cargo:rustc-env=RELDIR_TARGET={target}");
    println!("cargo:rerun-if-changed={}", docs.display());
}
