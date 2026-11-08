//! zapper-agent: the guest side of The Office, installed in its sandboxes'
//! images as `office` (`zapper-office image`): the workers' command line to
//! the office's tools, the MCP server Claude Code or pi runs, and Claude
//! Code's Stop hook, all reaching the office edge over HTTP.
//!
//! It holds no credential: the sandbox's supervisor adds the worker's token
//! on the way out, and the office checks every call.

mod office;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Installed as `office`, or run as `zapper-agent office ...`.
    if args
        .first()
        .is_some_and(|a| a.rsplit('/').next() == Some("office"))
    {
        std::process::exit(office::cli(&args[1..]));
    }
    match args.get(1).map(String::as_str) {
        Some("--version") => println!("zapper-agent {}", env!("CARGO_PKG_VERSION")),
        Some("office") => std::process::exit(office::cli(&args[2..])),
        _ => {
            eprintln!("usage: office ... (zapper-agent office ...); office --help");
            std::process::exit(2);
        }
    }
}
