//! cloo — a terminal multiplexer.
//!
//! This binary is the composition root: it parses the command line and wires
//! `cloo-server` and `cloo-client` together. It holds no session state and
//! emits no escape sequences of its own.
//!
//! Today it runs the M0 smoke path — one pane, one child, in-process, no
//! socket — from an explicit launch. See [`local`] and [`cli`]. The client/server
//! split over a Unix socket, and the `cloo attach` and `cloo new` subcommands
//! that go with it, land in M1.

mod cli;
mod local;

use std::process::ExitCode;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = env!("CARGO_PKG_REPOSITORY");

/// The exit code for a cloo-level failure, as opposed to one the child chose.
///
/// 1 is deliberately avoided: a child that exits 1 is ordinary, and anything
/// scripting cloo needs to tell "the shell failed" from "cloo failed".
const EXIT_FAILURE: u8 = 125;

/// The exit code for a command line cloo could not parse.
const EXIT_USAGE: u8 = 64;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match cli::parse(&args) {
        Ok(cli::Invocation::Version) => {
            println!("cloo {VERSION}");
            ExitCode::SUCCESS
        }
        Ok(cli::Invocation::Help) => {
            help();
            ExitCode::SUCCESS
        }
        Ok(cli::Invocation::Run(request)) => match request.into_launch() {
            Ok(launch) => run(launch),
            Err(err) => {
                eprintln!("cloo: {err}");
                ExitCode::from(EXIT_USAGE)
            }
        },
        Err(err) => {
            eprintln!("cloo: {err}");
            eprintln!("Try 'cloo --help'.");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

/// Runs one pane and translates its outcome into an exit code.
fn run(launch: cloo_server::launch::Launch) -> ExitCode {
    match local::run(launch) {
        Ok(status) => match status.code() {
            Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(EXIT_FAILURE)),
            // Killed by a signal. The shell convention is 128 + signal, but the
            // number is not exposed portably here, so report a generic failure
            // rather than a wrong one.
            None => ExitCode::from(EXIT_FAILURE),
        },
        Err(err) => {
            eprintln!("cloo: {err}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

fn help() {
    println!("cloo {VERSION} — a terminal multiplexer");
    println!();
    println!("STATUS");
    println!("    Pre-alpha. One local pane; no sessions, detach, or splits yet.");
    println!("    Design and roadmap: {REPO}");
    println!();
    println!("USAGE");
    println!("    cloo                       run $SHELL in a single pane");
    println!("    cloo <program> [args...]   run a program in a single pane");
    println!("    cloo --profile <id>        run a launch profile in a single pane");
    println!("    cloo [-V | --version]");
    println!("    cloo [-h | --help]");
    println!();
    println!("PANE OPTIONS");
    println!(
        "    -p, --profile <id>   which profile to launch ({})",
        cli::profile_ids()
    );
    println!("    -n, --name <text>    what to call the pane (default: the profile's)");
    println!("    -t, --task <text>    what the pane is for; never inferred");
    println!("    -c, --cwd <dir>      where to start the child (default: this directory)");
}
