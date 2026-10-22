//! WaySL entry point.
//!
//! All behaviour lives in the library so it can be tested and reused; this file
//! only reports the result.

fn main() {
    if let Err(error) = waysl::run() {
        eprintln!("WaySL stopped: {error}");
        std::process::exit(1);
    }
}
