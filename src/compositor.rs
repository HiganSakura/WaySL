//! Headless standard Wayland compositor: no local window system required.
//!
//! Committed `wl_shm` frames are streamed to a separate renderer over the
//! internal frame bridge, so this mode works in WSL without WSLg or X11.

use std::io;
use std::sync::mpsc;
use std::thread;

use crate::config::Config;
use crate::frame_bridge::{Frame, FrameWriter};
use crate::wayland;

/// Runs the compositor until it fails or the process exits.
///
/// `address` is the renderer's frame-bridge address.
pub fn run_headless(config: &Config, address: &str) -> io::Result<()> {
    let socket = config.wayland_socket.clone();
    remove_stale_socket(&socket)?;

    let (sender, receiver) = mpsc::channel();
    let address = address.to_string();
    let forwarder = thread::spawn(move || forward_frames(receiver, &address));

    let result = wayland::run_with_sender(&socket, sender);
    let _ = forwarder.join();
    result
}

/// Removes a leftover socket from a previous run.
fn remove_stale_socket(socket: &std::path::Path) -> io::Result<()> {
    match std::fs::remove_file(socket) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!(
                "cannot remove old Wayland socket {}: {error}",
                socket.display()
            ),
        )),
    }
}

/// Streams compositor frames to the renderer for as long as frames arrive.
fn forward_frames(receiver: mpsc::Receiver<wayland::WaylandFrame>, address: &str) {
    let mut writer = FrameWriter::default();
    for frame in receiver {
        let frame = Frame {
            id: frame.id,
            title: frame.title,
            width: frame.width,
            height: frame.height,
            pixels: frame.pixels,
            generation: frame.generation,
        };
        if let Err(error) = writer.send(address, &frame) {
            eprintln!("WaySL frame forward failed on {address}: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_socket_is_not_an_error() {
        let path = std::env::temp_dir().join("waysl-absent-socket-test");
        let _ = std::fs::remove_file(&path);
        assert!(remove_stale_socket(&path).is_ok());
    }

    #[test]
    fn stale_socket_is_removed() {
        let path = std::env::temp_dir().join("waysl-stale-socket-test");
        std::fs::write(&path, b"").expect("write");
        assert!(remove_stale_socket(&path).is_ok());
        assert!(!path.exists());
    }
}
