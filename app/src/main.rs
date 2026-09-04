//! LightSpeed IDE.
//!
//! ```text
//! process start -> configuration -> logging -> window -> GPU -> first frame
//! ```
//!
//! Each of those steps is timed, because the startup contract is a number
//! (specification section 49), not an impression.

// A GUI application should not open a console window on Windows. Debug builds
// keep the console so `LIGHTSPEED_LOG=debug` output is visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod compose;
mod devpanel;
mod icons;
mod json;
mod keymap;
mod layout;
mod lsp;
mod menu;
mod quads;
mod renderer;
mod resources;
mod tabs;
mod terminal;
mod text;
mod theme;

use app::UserEvent;
use std::path::PathBuf;
use std::time::Instant;
use winit::event_loop::{ControlFlow, EventLoop};

const SUBSYSTEM: &str = "app";

fn main() {
    let process_start = Instant::now();
    let arguments = Arguments::parse(std::env::args().skip(1));

    if arguments.help {
        print_usage();
        return;
    }
    if arguments.version {
        println!("LightSpeed IDE {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    ls_log::init_from_env(if cfg!(debug_assertions) {
        ls_log::Level::Debug
    } else {
        ls_log::Level::Info
    });

    let config = load_configuration();

    ls_log::info!(
        SUBSYSTEM,
        "starting",
        fields: [
            ls_log::Field::str("version", env!("CARGO_PKG_VERSION")),
            ls_log::Field::str("platform", ls_platform::platform_name()),
            ls_log::Field::uint("config_sources", config.sources.len() as u64),
        ],
        "LightSpeed IDE starting"
    );

    // A user-event loop: a scheduler worker cannot touch the shell, but it can
    // post an event that wakes it (amendment section 3.3).
    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            ls_log::error!(SUBSYSTEM, "event_loop_failed", "{error}");
            eprintln!("LightSpeed could not start: {error}");
            std::process::exit(1);
        }
    };
    // Redraw only when something changes: an idle editor should use no CPU.
    // The application raises this to `WaitUntil` when the caret needs to blink
    // (ADR-0013), which is the only timer in the shell.
    event_loop.set_control_flow(ControlFlow::Wait);

    let proxy = event_loop.create_proxy();
    let mut application = app::LightSpeed::new(config, arguments.paths, process_start, proxy);
    if let Err(error) = event_loop.run_app(&mut application) {
        ls_log::error!(SUBSYSTEM, "event_loop_error", "{error}");
        eprintln!("LightSpeed stopped: {error}");
        std::process::exit(1);
    }

    ls_log::info!(SUBSYSTEM, "stopped", "LightSpeed IDE stopped");
}

/// Loads defaults, then the user file, then the workspace file. A broken
/// configuration file is reported and skipped rather than blocking startup.
fn load_configuration() -> ls_core::EffectiveConfig {
    let paths = ls_core::config::standard_paths(std::env::current_dir().ok().as_deref());
    match ls_core::config::load_layered(&paths) {
        Ok(config) => config,
        Err(error) => {
            ls_log::diag::log_error(&error);
            eprintln!("LightSpeed: {error}\nUsing default configuration.");
            ls_core::EffectiveConfig::default()
        }
    }
}

struct Arguments {
    paths: Vec<PathBuf>,
    help: bool,
    version: bool,
}

impl Arguments {
    fn parse(arguments: impl Iterator<Item = String>) -> Self {
        let mut parsed = Arguments { paths: Vec::new(), help: false, version: false };
        for argument in arguments {
            match argument.as_str() {
                "-h" | "--help" => parsed.help = true,
                "-v" | "--version" => parsed.version = true,
                other if other.starts_with('-') => {
                    eprintln!("LightSpeed: unknown option {other}");
                    parsed.help = true;
                }
                other => parsed.paths.push(PathBuf::from(other)),
            }
        }
        parsed
    }
}

fn print_usage() {
    println!(
        "LightSpeed IDE {version}

USAGE:
    lightspeed [FILE...]

OPTIONS:
    -h, --help       Print this message
    -v, --version    Print the version

ENVIRONMENT:
    LIGHTSPEED_LOG        Log level: error, warn, info, debug, trace
    LIGHTSPEED_LOG_FILE   Also write logs to this file

KEYS:
    Ctrl+N/O/S      New, open, save          Ctrl+Shift+S  Save As
    Ctrl+Z/Y        Undo, redo               Ctrl+W        Close tab
    Ctrl+C/X/V      Copy, cut, paste         Ctrl+A        Select all
    Ctrl+Tab        Next tab                 F12           Performance overlay",
        version = env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_collect_paths() {
        let arguments = Arguments::parse(["a.rs".to_string(), "b/c.py".to_string()].into_iter());
        assert_eq!(arguments.paths.len(), 2);
        assert!(!arguments.help && !arguments.version);
    }

    #[test]
    fn flags_are_recognized() {
        assert!(Arguments::parse(["--help".to_string()].into_iter()).help);
        assert!(Arguments::parse(["-v".to_string()].into_iter()).version);
    }

    #[test]
    fn unknown_options_fall_back_to_usage() {
        let arguments = Arguments::parse(["--wat".to_string()].into_iter());
        assert!(arguments.help);
        assert!(arguments.paths.is_empty());
    }
}
