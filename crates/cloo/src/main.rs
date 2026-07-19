//! cloo — a terminal multiplexer.
//!
//! This is a placeholder release. The design is settled (see DESIGN.md in the
//! repository) but no functionality is implemented yet; the first milestone is
//! a single pane driven by an off-the-shelf terminal emulator, followed by
//! detach/reattach over a Unix socket.

const VERSION: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = env!("CARGO_PKG_REPOSITORY");

fn main() {
    let arg = std::env::args().nth(1);

    match arg.as_deref() {
        Some("-V" | "--version") => println!("cloo {VERSION}"),
        Some("-h" | "--help") => help(),
        _ => {
            help();
            eprintln!();
            eprintln!("cloo is not implemented yet — this release reserves the name.");
            eprintln!("Follow development at {REPO}");
            std::process::exit(1);
        }
    }
}

fn help() {
    println!("cloo {VERSION} — a terminal multiplexer");
    println!();
    println!("STATUS");
    println!("    Pre-alpha. No functionality is implemented yet.");
    println!("    Design and roadmap: {REPO}");
    println!();
    println!("USAGE");
    println!("    cloo [-V | --version] [-h | --help]");
}
