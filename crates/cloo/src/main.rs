//! cloo — a terminal multiplexer.
//!
//! This binary is the composition root: it parses the command line and wires
//! `cloo-server` and `cloo-client` together. It holds no session state and
//! emits no escape sequences of its own.
//!
//! The bare command is the managed default workspace: it attaches to the
//! `default` session's daemon, creating one in the background first if none is
//! listening. `cloo <program>` retains the M0 local path — one pane, one child,
//! in process — and `cloo server` / `cloo attach` remain the explicit halves.
//! See [`local`] and [`cli`].

mod cli;
mod local;

use std::io;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = env!("CARGO_PKG_REPOSITORY");

/// The exit code for a cloo-level failure, as opposed to one the child chose.
///
/// 1 is deliberately avoided: a child that exits 1 is ordinary, and anything
/// scripting cloo needs to tell "the shell failed" from "cloo failed".
const EXIT_FAILURE: u8 = 125;

/// The exit code for a command line cloo could not parse.
const EXIT_USAGE: u8 = 64;

/// How long a bare `cloo` waits for the daemon it started to accept a
/// connection before giving up on it.
///
/// Generous, because the first run also spawns a login shell, and short enough
/// that a daemon which will never come up does not look like a hang.
const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// How often the readiness probe retries while waiting.
const DAEMON_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long a caller keeps probing after the daemon *it* started has exited.
///
/// Losing the start race is an ordinary outcome, and the loser's exit is not
/// proof that nothing will serve — only that this child will not. The winner
/// holds the lock from before it unlinks a stale socket until after it binds,
/// so a loser is refused inside a window in which nothing is listening yet.
/// Giving up on the first failed probe after that exit therefore turns a race
/// two callers are supposed to survive into a failure for whichever lost.
///
/// Bounded and short: a daemon that exited for a real reason never binds at
/// all, and that message must not be slow.
const DAEMON_HANDOFF_GRACE: Duration = Duration::from_secs(2);

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
        Ok(cli::Invocation::Workspace(request)) => workspace(&request),
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

/// Attaches to the default workspace, creating its daemon if there is none.
///
/// The whole point of this path is that a user never has to run two commands,
/// and the whole risk is that it now starts a process on their behalf. Three
/// rules keep that honest:
///
/// * **The socket answers "is there a workspace", not the socket file.** A
///   successful connect is the only proof, so a socket a killed daemon left
///   behind is treated as no workspace at all and replaced under
///   `Listener::bind`'s existing lock-first rules.
/// * **The bind is the authority, not this process.** Two simultaneous first
///   runs both start a daemon; exactly one wins the `flock` and the loser exits
///   as an ordinary refusal, so both callers converge on the survivor rather
///   than on whichever of them spawned first.
/// * **Nothing touches the terminal until a daemon is ready.** Raw mode is
///   entered inside `cloo attach`'s own path, which is only reached once a
///   connection has already succeeded — a failed bootstrap leaves the caller's
///   terminal exactly as it found it.
fn workspace(request: &cli::WorkspaceRequest) -> ExitCode {
    let socket = match cloo_server::socket::session_socket_path(&request.session) {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("cloo: {err}");
            return ExitCode::from(EXIT_USAGE);
        }
    };

    if !daemon_is_listening(&socket) {
        let started = match start_daemon(&request.session) {
            Ok(child) => child,
            Err(err) => {
                eprintln!("cloo: could not start a cloo daemon: {err}");
                return ExitCode::from(EXIT_FAILURE);
            }
        };
        if let Err(err) = wait_until_ready(&socket, started, &request.session) {
            eprintln!("cloo: {err}");
            return ExitCode::from(EXIT_FAILURE);
        }
    }

    attach_to(&socket)
}

/// Is a daemon accepting connections on `socket` right now?
///
/// A connect, never a `Path::exists`: a socket file proves only that some
/// daemon once bound this path. The probe connection is dropped immediately,
/// which the daemon sees as a connection that closed before its attach — the
/// same thing an interrupted client does, and already handled.
fn daemon_is_listening(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

/// Starts a background daemon for `session` and returns the child.
///
/// The daemon is this same binary's `cloo server <session>` — one lifecycle,
/// not a second implementation of it — with its stdio detached so it can never
/// write over the frame the attached client is about to draw. `setsid` is
/// best effort: it keeps the daemon out of the caller's terminal session, but a
/// daemon's real lifetime is owned by its socket lock, so failing to get a new
/// session is not a reason to refuse to start one.
fn start_daemon(session: &str) -> io::Result<std::process::Child> {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .arg("server")
        .arg(session)
        // Inherits the environment on purpose: `CLOO_SOCKET`, `CLOO_CONFIG`,
        // `XDG_RUNTIME_DIR`, and `TERM` must mean the same thing to the daemon
        // this caller is about to attach to, and the first creation inherits
        // the caller's working directory for its first pane.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: `pre_exec` runs between fork and exec, where only
    // async-signal-safe calls are allowed. `setsid` qualifies, takes no
    // pointer, and its failure is discarded rather than acted on.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command.spawn()
}

/// Waits for `socket` to accept connections, or explains why it never will.
///
/// The child is polled alongside the socket rather than instead of it. A daemon
/// that exited because a *concurrent* first run won the lock is a success from
/// here — the socket is live, just not this child's — so its exit only starts a
/// bounded [`DAEMON_HANDOFF_GRACE`] in which the winner may still begin
/// serving, and is reported as a failure only once that has also elapsed.
fn wait_until_ready(
    socket: &Path,
    mut started: std::process::Child,
    session: &str,
) -> Result<(), String> {
    let deadline = Instant::now() + DAEMON_READY_TIMEOUT;
    // How this child ended, and how long a survivor still has to take over.
    let mut handed_off: Option<(std::process::ExitStatus, Instant)> = None;
    loop {
        if daemon_is_listening(socket) {
            return Ok(());
        }
        if handed_off.is_none() {
            if let Ok(Some(status)) = started.try_wait() {
                handed_off = Some((status, deadline.min(Instant::now() + DAEMON_HANDOFF_GRACE)));
            }
        }
        if let Some((status, until)) = handed_off {
            if Instant::now() >= until {
                return Err(format!(
                    "the cloo daemon exited ({status}) before it could serve {}; \
                     run `cloo server {session}` to see why",
                    socket.display()
                ));
            }
        }
        if Instant::now() >= deadline {
            // This process started it and it never became usable, so this
            // process cleans it up rather than leaving an orphan behind.
            let _ = started.kill();
            let _ = started.wait();
            return Err(format!(
                "the cloo daemon did not start serving {} within {}s; \
                 run `cloo server {session}` to see why",
                socket.display(),
                DAEMON_READY_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(DAEMON_POLL_INTERVAL);
    }
}

/// Runs one foreground daemon that owns the named session.
///
/// A server is deliberately not a terminal client: it neither enters raw mode
/// nor enables reporting. Its initial generic pane starts at the conventional
/// 80×24 until an attached terminal negotiates the session's real geometry.
fn server(request: cli::ServerRequest) -> ExitCode {
    // The daemon is the profile authority: a client's launch request names an
    // ID, and this is the table that ID is resolved against. Loaded before the
    // socket is bound so a bad document is reported by a process that has not
    // yet claimed a session path.
    let loaded = cloo_server::config::load_from_environment();
    for diagnostic in loaded.diagnostics {
        eprintln!("cloo: warning: {diagnostic}");
    }
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
        let mut daemon =
            cloo_server::daemon::Daemon::new(listener, &base, launch)?.with_config(loaded.config);
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
    let socket = match cloo_server::socket::session_socket_path(&request.session) {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("cloo: {err}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    attach_to(&socket)
}

/// Attaches to an already-resolved socket path.
///
/// Split from [`attach`] so the managed workspace resolves the socket exactly
/// once — the path it probed and the path it attaches to cannot drift apart.
fn attach_to(socket: &Path) -> ExitCode {
    let loaded = cloo_server::config::load_from_environment();
    for diagnostic in loaded.diagnostics {
        eprintln!("cloo: warning: {diagnostic}");
    }

    match cloo_client::attach::run(socket, loaded.config.keys().clone()) {
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
    println!("    Pre-alpha. `cloo` opens the default workspace; `cloo <program>`");
    println!("    still runs one local pane.");
    println!("    Design and roadmap: {REPO}");
    println!();
    println!("USAGE");
    println!("    cloo                       open the default workspace, starting it if needed");
    println!("    cloo <program> [args...]   run a program in a single local pane");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A private directory that takes its contents with it.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!("cloo-main-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("the fixture directory is creatable");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn only_a_daemon_that_answers_counts_as_a_live_workspace() {
        // The decision the bare command turns on. A socket *file* is not a
        // workspace: a daemon that was killed leaves one behind, and treating
        // it as live would make `cloo` fail instead of replacing it, while
        // treating a live one as absent would start a second daemon.
        let dir = TempDir::new("listening");
        let missing = dir.0.join("missing.sock");
        assert!(
            !daemon_is_listening(&missing),
            "a path with nothing at it is no workspace"
        );

        let path = dir.0.join("live.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&path).expect("the fixture socket binds");
        assert!(
            daemon_is_listening(&path),
            "a bound socket is a live workspace"
        );

        drop(listener);
        assert!(path.exists(), "the fixture leaves the socket file behind");
        assert!(
            !daemon_is_listening(&path),
            "a socket file whose daemon is gone is not a live workspace"
        );
    }

    #[test]
    fn the_readiness_probe_is_bounded_and_short_enough_to_look_like_startup() {
        assert!(DAEMON_POLL_INTERVAL < DAEMON_READY_TIMEOUT);
        assert!(
            DAEMON_READY_TIMEOUT <= Duration::from_secs(30),
            "a daemon that will never come up must not look like a hang"
        );
        // The grace after a losing daemon exits is a window inside the same
        // budget, not an extension of it: a start that genuinely failed still
        // reports inside the overall timeout, and it reports promptly.
        assert!(DAEMON_POLL_INTERVAL < DAEMON_HANDOFF_GRACE);
        assert!(DAEMON_HANDOFF_GRACE < DAEMON_READY_TIMEOUT);
    }
}
