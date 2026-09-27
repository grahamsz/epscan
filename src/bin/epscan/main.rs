// SPDX-License-Identifier: MIT
mod cancel;
mod cli;
mod progress;
mod retention;
mod scan;

use clap::Parser;
use cli::{Action, Cli};
use epscan::{Error, Result, Session};
use std::{
    io::{IsTerminal, Write, stderr},
    time::Duration,
};
use tracing_subscriber::EnvFilter;

// Legacy windows command prompt doesn't interpret ANSI escapes until a process opts in via SetConsoleMode.
// Without this, coloring anything on Windows prints raw escape codes instead of color.
#[cfg(target_os = "windows")]
fn enable_ansi_support() {
    use windows_sys::Win32::System::Console::{
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    // Safety: STD_OUTPUT_HANDLE and STD_ERROR_HANDLE identify standard handles, valid for
    // the life of the process, so GetStdHandle needs no cleanup. GetConsoleMode/SetConsoleMode
    // take each handle and an out-param/value of the right type; failure (handle redirected
    // to a file, not a console at all) is reported through the BOOL return, checked below,
    // not through UB. No pointers into Rust-managed memory escape this block.
    unsafe {
        for stream in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = GetStdHandle(stream);
            let mut mode = 0;
            if GetConsoleMode(handle, &mut mode) != 0 {
                SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
fn enable_ansi_support() {}

fn run(cli: Cli) -> Result<()> {
    let timeout = Duration::from_secs(cli.io_timeout);
    match cli.action {
        Action::List => println!(
            "{}",
            serde_json::to_string_pretty(&epscan::list_devices(cli.backend)?)?
        ),
        Action::Dump(args) => {
            let mut session = Session::connect(args.device.as_deref(), cli.backend, timeout)?;
            let text = serde_json::to_string_pretty(&session.diagnostics()?)?;
            if let Some(path) = args.output {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)?;
                writeln!(file, "{text}")?;
            }
            println!("{text}");
        }
        Action::Scan(args) => scan::scan(args, cli.backend, timeout)?,
        Action::Preview(args) => scan::preview(args, cli.backend, timeout)?,
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    enable_ansi_support();
    let cli = Cli::parse();

    // Set up logging. RUST_LOG overrides everything, since it can target individual modules
    // Otherwise fall back to --log (default: info). nusb logs one line per USB
    // transfer at debug, which drowns out everything else in a `--log debug` capture, so
    // the default keeps it at warn; RUST_LOG can still ask for it by name.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(format!("{},nusb=warn", cli.log))),
        )
        .with_target(false)
        // The bars and the log share stderr, so that is what decides both the
        // color and which stream a line has to be sequenced against
        .with_ansi(stderr().is_terminal())
        .with_writer(progress::Writer)
        .init();
    let outcome = cancel::install().and_then(|()| run(cli));
    progress::clear();
    match outcome {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::from(if matches!(error, Error::Cancelled) {
                130
            } else {
                2
            })
        }
    }
}
