//! amux — wrap coding-agent CLIs (omp) in a dual-pane PTY control plane.

use std::io;

use anyhow::Result;
use std::io::Write;
use crossterm::terminal::{disable_raw_mode, LeaveAlternateScreen};
use crossterm::cursor::Show;
use crossterm::execute;

fn main() -> Result<()> {
    // Logging to stderr file only when AMUX_LOG is set — keep TUI clean.
    if std::env::var_os("AMUX_LOG").is_some() {
        let dir = amux::config::AmuxConfig::amux_dir();
        let _ = std::fs::create_dir_all(&dir);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("amux.log"));
        if let Ok(file) = file {
            tracing_subscriber::fmt()
                .with_writer(file)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::from_default_env()
                        .add_directive("amux=debug".parse().unwrap()),
                )
                .init();
        }
    }

    // Headless smoke without TTY
    if std::env::args().any(|a| a == "--smoke") {
        amux::pty::smoke_spawn_echo()?;
        println!("amux smoke ok");
        return Ok(());
    }

    if !crossterm::tty::IsTty::is_tty(&std::io::stdin()) {
        eprintln!(
            "amux requires a TTY.\n\
             Build: cargo build --release\n\
             Run:   cargo run --release\n\
             Smoke: cargo run -- --smoke\n\
             Tests: cargo test"
        );
        std::process::exit(2);
    }

    // Panic hook: restore terminal on any panic so the user's shell isn't
    // left in raw mode or with host private modes (mouse/paste/kitty) armed.
    // (§4.2.8: panic/signal hooks restore host terminal; §4.2.10 every exit
    // path.) Emitting these unconditionally is safe: each is a no-op when the
    // corresponding mode was never enabled by amux.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = io::stdout();
        let _ = disable_raw_mode();
        // Pop kitty keyboard disambiguation if amux armed it. (no-op if unset)
        let _ = write!(out, "\x1b[<u");
        // Exit restore: disable mouse / bracketed paste / focus.
        let _ = amux::shell::mode_mirror::baseline_host_modes(&mut out);
        let _ = execute!(out, LeaveAlternateScreen, Show);
        prev_hook(info);
    }));

    // Single-instance: if another amux is running, SIGTERM it (teardown +
    // child omp) then take the lock. Disk sessions remain resumable.
    let _instance = amux::lock::acquire_instance_lock_replacing()?;
    // Keep lock for process lifetime (flock released on exit when File drops).
    std::mem::forget(_instance);

    let mut app = amux::shell::App::new()?;

    if let Err(e) = app.run() {
        // Best-effort restore already done inside run; report error.
        eprintln!("amux error: {e:#}");
        if format!("{e:#}").contains("Resource temporarily unavailable")
            || format!("{e:#}").contains("os error 11")
        {
            eprintln!(
                "hint: this was often caused by O_NONBLOCK on the TTY (fixed in current builds).\n\
                 retry after: cargo build --release && ./target/release/amux\n\
                 if it persists, check another amux/omp isn't wedging the terminal."
            );
        }
        std::process::exit(1);
    }
    Ok(())
}
