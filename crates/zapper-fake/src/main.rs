//! zapper-fake: a stand-in for Claude Code and pi in end-to-end tests. The
//! `office-fake` image installs it as `claude` and `pi`, so a profile on that
//! image runs a firm's workers exactly as it would the real agents: the same
//! command line, configuration and MCP server. It plays the script that
//! `ZAPPER_FAKE_SCRIPT` names (the profile sets it) instead of thinking.

mod fake;

/// Where the script is: set by the profile, e.g. on a read-only mount.
const SCRIPT_ENV: &str = "ZAPPER_FAKE_SCRIPT";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Which agent it stands in for, by the name it was run as.
    let proto = match args.first().map(|a| a.rsplit('/').next().unwrap_or(a)) {
        Some("pi") => fake::Proto::Pi,
        _ => fake::Proto::Claude,
    };
    if args.iter().any(|a| a == "--version") {
        println!(
            "zapper-fake {} (as {})",
            env!("CARGO_PKG_VERSION"),
            if proto == fake::Proto::Pi {
                "pi"
            } else {
                "claude"
            }
        );
        return;
    }
    let script = std::env::var(SCRIPT_ENV).unwrap_or_else(|_| "/dev/null".into());
    std::process::exit(fake::run(&script, proto));
}
