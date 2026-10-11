//! The `reldir` command -- a shell, a CLI and an MCP server over the engine
//! in `reldir-core` -- as a library, so that whatever embeds the engine (the
//! Python wheel's `reldir` script) runs exactly this command.

mod cli;
mod command;
mod docs;
mod mcp;
mod output;
mod protocol;
mod serve;
mod shell;

use clap::Parser;
use std::ffi::OsString;

/// Run the command with these arguments, the program's name first, to its
/// exit status.
pub fn main(arguments: impl IntoIterator<Item = OsString>) -> i32 {
    // A defect must still end in the protocol a program reading the output
    // expects, and in the exit status that says it was a defect.
    cli::install_panic_report();
    let arguments: Vec<OsString> = arguments.into_iter().collect();
    let status = std::panic::catch_unwind(|| match cli::Cli::try_parse_from(arguments) {
        Ok(cli) => cli::run(cli),
        Err(error) => cli::refuse_arguments(error),
    });
    status.unwrap_or(reldir_core::code::Exit::Internal.code())
}
