use clap::Parser;

fn main() {
    let cli = match jdb::cli::Cli::try_parse() {
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
    let machine = cli.machine_error_format().map(String::from);
    match jdb::cli::run(cli) {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            if machine.is_some() {
                eprintln!(
                    "{}",
                    serde_json::to_string(&error.diagnostic)
                        .expect("diagnostic serialization cannot fail")
                );
            } else {
                error.render_human();
            }
            std::process::exit(error.exit_code());
        }
    }
}
