//! The Rust guide's examples are code that compiles: every ```rust block of
//! docs/rust.md is, verbatim, a region of examples/guide.rs, which `cargo
//! test` compiles against this crate.

const PAGE: &str = include_str!("../../../docs/rust.md");
const EXAMPLES: &str = include_str!("../examples/guide.rs");

fn page_blocks() -> Vec<String> {
    let mut out = vec![];
    let mut current: Option<String> = None;
    for line in PAGE.lines() {
        match &mut current {
            None if line == "```rust" => current = Some(String::new()),
            Some(block) if line == "```" => {
                out.push(std::mem::take(block));
                current = None;
            }
            Some(block) => {
                block.push_str(line);
                block.push('\n');
            }
            None => {}
        }
    }
    out
}

fn example_regions() -> Vec<String> {
    let mut out = vec![];
    let mut current: Option<String> = None;
    for line in EXAMPLES.lines() {
        match &mut current {
            None if line.trim() == "// example" => current = Some(String::new()),
            Some(region) if line.trim() == "// end" => {
                out.push(std::mem::take(region));
                current = None;
            }
            Some(region) => {
                region.push_str(line.strip_prefix("    ").unwrap_or(line));
                region.push('\n');
            }
            None => {}
        }
    }
    out
}

#[test]
fn test9201_the_rust_guide_shows_code_that_compiles() {
    let page = page_blocks();
    assert!(page.len() >= 4, "the guide shows its API in use");
    assert_eq!(
        page,
        example_regions(),
        "docs/rust.md and examples/guide.rs must hold the same examples"
    );
}
