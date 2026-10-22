//! zapper-agent: PID 1 of a zapper microVM.
//!
//! It mounts the basics, fetches the session spec from the host over vsock,
//! prepares the guest, then serves terminals whose every exec is checked by
//! the host and whose process exits are reported to it.
//!
//! Nothing here is trusted by the host: the agent holds no secrets, and the
//! host enforces network, secret and disk policy on its own side.

mod acct;
mod control;
#[cfg(feature = "office")]
mod fake;
mod forward;
mod init;
#[cfg(feature = "office")]
mod office;
mod shell;
mod sys;
mod usage;

fn main() {
    if std::process::id() == 1 {
        // A panic in PID 1 would panic the kernel; power off instead.
        std::panic::set_hook(Box::new(|info| {
            eprintln!("zapper-agent: {info}");
            sys::power_off();
        }));
        init::run();
    }
    let args: Vec<String> = std::env::args().collect();
    // Installed in guests as `office` too.
    #[cfg(feature = "office")]
    if args
        .first()
        .is_some_and(|a| a.rsplit('/').next() == Some("office"))
    {
        std::process::exit(office::cli(&args[1..]));
    }
    match args.get(1).map(String::as_str) {
        Some("--version") => println!("zapper-agent {}", env!("CARGO_PKG_VERSION")),
        #[cfg(feature = "office")]
        Some("office") => std::process::exit(office::cli(&args[2..])),
        #[cfg(feature = "office")]
        Some(f @ ("fake-claude" | "fake-pi")) => std::process::exit(fake::run(
            args.get(2).map(String::as_str).unwrap_or("/dev/null"),
            if f == "fake-pi" {
                fake::Proto::Pi
            } else {
                fake::Proto::Claude
            },
        )),
        _ => {
            eprintln!("zapper-agent runs as PID 1 of a zapper microVM");
            std::process::exit(2);
        }
    }
}
