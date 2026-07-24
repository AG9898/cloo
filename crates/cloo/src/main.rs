//! cloo — a terminal multiplexer.
//!
//! This binary is the composition root: it parses the command line and wires
//! `cloo-server` and `cloo-client` together. It holds no session state and
//! emits no escape sequences of its own.
//!
//! The bare command retains the M0 local path — one pane, one child, in
//! process — while `cloo attach` connects the client-owned multipane render
//! loop to a daemon. See [`local`] and [`cli`].

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
        Ok(cli::Invocation::Attach(request)) => attach(request),
        Ok(cli::Invocation::Server(request)) => server(request),
        Err(err) => {
            eprintln!("cloo: {err}");
            eprintln!("Try 'cloo --help'.");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

/// Runs one foreground daemon that owns the named session.
///
/// A server is deliberately not a terminal client: it neither enters raw mode
/// nor enables reporting. Its initial generic pane starts at the conventional
/// 80×24 until an attached terminal negotiates the session's real geometry.
fn server(request: cli::ServerRequest) -> ExitCode {
    let launch = match cli::Request::default().into_launch() {
        Ok(launch) => launch,
        Err(err) => {
            eprintln!("cloo: {err}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let socket = match cloo_server::socket::session_socket_path(&request.session) {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("cloo: {err}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let listener = match cloo_server::socket::Listener::bind(&socket) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("cloo: {err}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    let size = cloo_proto::Size::new(
        cloo_server::pty::PtyConfig::DEFAULT_COLS,
        cloo_server::pty::PtyConfig::DEFAULT_ROWS,
    );
    let base = match cloo_server::pty::PtyConfig::session(size) {
        Ok(base) => base,
        Err(err) => {
            eprintln!("cloo: {err}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("cloo: could not start the runtime: {err}");
            return ExitCode::from(EXIT_FAILURE);
        }
    };

    let outcome = runtime.block_on(async {
        let mut daemon = cloo_server::daemon::Daemon::new(listener, &base, launch)?;
        daemon.run().await
    });
    match outcome {
        Ok(status) => match status.code() {
            Some(code) => ExitCode::from(u8::try_from(code).unwrap_or(EXIT_FAILURE)),
            None => ExitCode::from(EXIT_FAILURE),
        },
        Err(err) => {
            eprintln!("cloo: {err}");
            ExitCode::from(EXIT_FAILURE)
        }
    }
}

/// Attaches the interactive client to the named daemon session.
fn attach(request: cli::AttachRequest) -> ExitCode {
    let loaded = cloo_server::config::load_from_environment();
    for diagnostic in loaded.diagnostics {
        eprintln!("cloo: warning: {diagnostic}");
    }
    let socket = match cloo_server::socket::session_socket_path(&request.session) {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("cloo: {err}");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    match cloo_client::attach::run(&socket, loaded.config.keys().clone()) {
        Ok(status) => ExitCode::from(u8::try_from(status).unwrap_or(EXIT_FAILURE)),
        Err(err) => {
            eprintln!("cloo: {err}");
            ExitCode::from(EXIT_FAILURE)
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
    println!("    Pre-alpha. `cloo` runs one local pane; `cloo attach` joins a daemon.");
    println!("    Design and roadmap: {REPO}");
    println!();
    println!("USAGE");
    println!("    cloo                       run $SHELL in a single pane");
    println!("    cloo <program> [args...]   run a program in a single pane");
    println!("    cloo --profile <id>        run a launch profile in a single pane");
    println!("    cloo server [session]      run a foreground daemon (default: default)");
    println!("    cloo attach [session]      attach to a daemon (default: default)");
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
