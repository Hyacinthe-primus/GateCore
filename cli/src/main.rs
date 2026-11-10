//! `rfid-cli` entry point: parse arguments, then either run one command or
//! start the interactive shell. All real work lives in the library crate.

use clap::Parser;

use rfid_cli::cli::Cli;
use rfid_cli::commands::{self, Ctx};
use rfid_cli::{interrupt, repl, ui, Error};

fn main() {
    interrupt::install();
    let cli = Cli::parse();
    if cli.no_color {
        ui::set_color(false);
    }

    let result = match &cli.command {
        None => repl::run(cli.port.clone(), cli.baud, cli.verbose),
        Some(cmd) => {
            // The context owns the (lazily opened) port; it is closed when
            // `ctx` drops at the end of this block, before process::exit.
            let mut ctx = Ctx::one_shot(cli.port.clone(), cli.baud, cli.verbose);
            commands::run(cmd, &mut ctx)
        }
    };

    let code = match result {
        Ok(()) => 0,
        Err(e @ (Error::Interrupted | Error::Cancelled)) => {
            ui::warning(&e.to_string());
            e.exit_code()
        }
        Err(e) => {
            ui::error(&e.to_string());
            e.exit_code()
        }
    };
    std::process::exit(code);
}
