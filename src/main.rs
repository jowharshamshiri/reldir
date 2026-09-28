use clap::Parser;

fn main() {
    let cli = match reldir::cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            use clap::error::ErrorKind;
            let code = if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                0
            } else {
                1
            };
            let _ = error.print();
            std::process::exit(code);
        }
    };
    std::process::exit(reldir::cli::run(cli));
}
