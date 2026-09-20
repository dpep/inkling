//! The lifecycle against a real kernel, since the unit tests fake the clock.
//!
//! This binary is both the caller and the server, so `ensure_running` spawning
//! `current_exe` re-runs it in daemon mode — which is exactly what `inkling`
//! will do once `serve` is wired into main.rs.
//!
//!   cargo run --example serve_demo -- ensure /tmp/demo alice
//!   cargo run --example serve_demo -- status /tmp/demo
//!
//! The server is busy while `<dir>/work` exists and idle once it's deleted.

use inkling::obs;
use inkling::serve::{self, Config, Ensured, Pulse, Ticked};
use std::path::PathBuf;

fn config(dir: &str) -> Config {
    // The server runs with cwd at the root, so a relative path would follow it.
    let dir = PathBuf::from(dir).canonicalize().expect("an existing dir");
    Config {
        interval: std::time::Duration::from_secs(1),
        idle_after: std::time::Duration::from_secs(3),
        max_lifetime: std::time::Duration::from_secs(120),
        ..Config::in_dir(dir)
    }
}

fn logging(dir: &str, quiet: bool) -> obs::Logging {
    obs::init(&obs::Options {
        directory: Some(PathBuf::from(dir).join("logs")),
        verbosity: 1,
        quiet,
        ..Default::default()
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command: Vec<&str> = args.iter().map(String::as_str).collect();

    match command.as_slice() {
        ["daemon", dir] => {
            // Detached: stderr is /dev/null, so the file is the only window.
            let _logging = logging(dir, true);
            let work = || -> Ticked {
                match PathBuf::from(dir).join("work").exists() {
                    true => Ok(Pulse::Busy),
                    false => Ok(Pulse::Idle),
                }
            };
            serve::run(config(dir), &work).expect("served");
        }
        ["ensure", dir, who] => {
            let _logging = logging(dir, false);
            let full = PathBuf::from(dir).canonicalize().unwrap();
            let args = ["daemon", full.to_str().unwrap()];
            match serve::ensure_running(&config(dir), &args, who).expect("ensured") {
                Ensured::AlreadyRunning => println!("{who}: already running"),
                Ensured::Spawned { pid } => println!("{who}: spawned {pid}"),
            }
        }
        ["status", dir] => {
            let status = serve::read_status(&config(dir));
            println!(
                "running={} record={}",
                status.running,
                serde_json::to_string(&status.record).unwrap()
            );
        }
        _ => {
            eprintln!("usage: serve_demo <daemon|ensure|status> <dir> [who]");
            std::process::exit(2);
        }
    }
}
