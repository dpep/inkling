//! A stand-in for a Claude Code session's inbox: bind the messaging socket
//! and print whatever is written to it.
//!
//! What a real session does with these lines is its business; this exists so
//! the delivery leg can be watched end to end without posting into anybody's
//! actual session.
//!
//!   cargo run --example session_inbox -- /tmp/ink-e2e/s
//!
//! A connection that closes without sending a line is inkling asking whether
//! anyone is still here, and is printed as `probe` — a real session starts no
//! turn for one.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: session_inbox <socket path>");
        std::process::exit(2);
    };

    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).expect("bound");
    println!("listening on {path}");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let lines: Vec<String> = BufReader::new(stream)
            .lines()
            .map_while(Result::ok)
            .collect();
        let at = chrono::Local::now().format("%H:%M:%S");
        match lines.is_empty() {
            true => println!("{at}  probe"),
            false => {
                for line in lines {
                    println!("{at}  {line}");
                }
            }
        }
    }
}
