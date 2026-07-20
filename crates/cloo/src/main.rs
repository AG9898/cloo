//! cloo — a terminal multiplexer.
//!
//! This binary is the composition root: it parses the command line and wires
//! `cloo-server` and `cloo-client` together. It holds no session state and
//! emits no escape sequences of its own.
//!
//! Today it runs the M0 smoke path — one pane, one child, in-process, no
//! socket. See [`local`]. The client/server split over a Unix socket, and the
//! `cloo attach` and `cloo new` subcommands that go with it, land in M1.

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

    match args.first().map(String::as_str) {
        Some("-V" | "--version") => {
            println!("cloo {VERSION}");
            ExitCode::SUCCESS
        }
        Some("-h" | "--help") => {
            help();
            ExitCode::SUCCESS
        }
        // An unrecognized flag is a mistake, not a program name. Treating it as
        // one would try to execute `--colour` and then blame the user's PATH.
        Some(flag) if flag.starts_with('-') => {
            eprintln!("cloo: unrecognized option '{flag}'");
            eprintln!("Try 'cloo --help'.");
            ExitCode::from(EXIT_USAGE)
        }
        Some(program) => run(program.to_owned(), &args[1..]),
        None => run(local::default_shell(), &[]),
    }
}

/// Runs one pane and translates its outcome into an exit code.
fn run(program: String, args: &[String]) -> ExitCode {
    match local::run(&program, args) {
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
    println!("    cloo [-V | --version]");
    println!("    cloo [-h | --help]");
}
