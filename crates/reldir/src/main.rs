//! The `reldir` command: a shell, a CLI and an MCP server over the engine in
//! `reldir-core`.

fn main() {
    std::process::exit(reldir::main(std::env::args_os()));
}
