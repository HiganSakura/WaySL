//! Internal frame transport between the Linux compositor and the renderer.
//!
//! This is deliberately **not** a Wayland protocol. Wayland remains the
//! client-facing protocol; this framed stream only carries already-composited
//! RGBA surfaces from a headless compositor to a native renderer.
//!
//! A frame is a 36-byte little-endian header followed by the UTF-8 title and the
//! RGBA8 pixels:
//!
//! ```text
//! "WSL1" | width:u32 | height:u32 | id:u64 | generation:u64 |
//! title_len:u32 | pixel_len:u32 | title bytes | RGBA8 pixels
//! ```
//!
//! Two halves are provided so both sides share one definition:
//!
//! - [`FrameWriter`] connects out and streams frames (compositor side).
//! - [`start_listener`] accepts connections and reports frames (renderer side).

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

/// Frame magic. Kept for compatibility with existing producers.
const MAGIC: &[u8; 4] = b"WSL1";
/// Size of the fixed header, excluding title and pixels.
pub const HEADER_LEN: usize = 36;
/// Largest accepted title, in bytes.
pub const MAX_TITLE: u32 = 4096;
/// Largest accepted pixel payload, in bytes (256 MiB).
pub const MAX_PIXELS: u32 = 256 * 1024 * 1024;

/// One composited surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// Stable surface id assigned by the compositor.
    pub id: u64,
    /// Window title reported by the client.
    pub title: String,
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// Tightly packed RGBA8 pixels (`width * height * 4` bytes).
    pub pixels: Vec<u8>,
    /// Monotonic counter; the renderer skips unchanged generations.
    pub generation: u64,
}

impl Frame {
    /// Encodes this frame into wire bytes (header, title, pixels).
    pub fn encode(&self) -> Vec<u8> {
        let title = self.title.as_bytes();
        let mut out = encode_header(
            self.id,
            self.width,
            self.height,
            self.generation,
            title.len(),
            self.pixels.len(),
        );
        out.extend_from_slice(title);
        out.extend_from_slice(&self.pixels);
        out
    }
}

/// Encodes only the fixed header for one frame.
///
/// The title and pixels must follow immediately; [`Frame::encode`] does both.
#[must_use]
pub fn encode_header(
    id: u64,
    width: u32,
    height: u32,
    generation: u64,
    title_len: usize,
    pixel_len: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER_LEN);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&(title_len as u32).to_le_bytes());
    out.extend_from_slice(&(pixel_len as u32).to_le_bytes());
    out
}

/// Streams frames to a renderer, reconnecting after a failure.
#[derive(Debug, Default)]
pub struct FrameWriter {
    stream: Option<TcpStream>,
}

impl FrameWriter {
    /// Sends one frame, connecting first if needed.
    ///
    /// A failed write drops the connection so the next call reconnects; the
    /// caller sees the original error.
    pub fn send(&mut self, address: &str, frame: &Frame) -> io::Result<()> {
        if self.stream.is_none() {
            self.stream = Some(TcpStream::connect(address)?);
        }
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        if let Err(error) = stream
            .write_all(&frame.encode())
            .and_then(|()| stream.flush())
        {
            self.stream = None;
            return Err(error);
        }
        Ok(())
    }
}

/// Binds `address` and delivers every received frame to the returned receiver.
///
/// The listener runs on a background thread; malformed peers are logged and
/// disconnected without stopping the listener.
#[must_use]
pub fn start_listener(address: String) -> mpsc::Receiver<Frame> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let listener = match TcpListener::bind(&address) {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("WaySL frame bridge failed on {address}: {error}");
                return;
            }
        };
        eprintln!("WaySL frame bridge listening on {address}");
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let sender = sender.clone();
                    thread::spawn(move || {
                        if let Err(error) = read_frames(stream, &sender) {
                            eprintln!("WaySL frame bridge client stopped: {error}");
                        }
                    });
                }
                Err(error) => eprintln!("WaySL frame bridge accept failed: {error}"),
            }
        }
    });
    receiver
}

/// Reads frames from one connection until it closes or misbehaves.
pub fn read_frames(mut stream: TcpStream, sender: &mpsc::Sender<Frame>) -> io::Result<()> {
    loop {
        let frame = read_frame(&mut stream)?;
        sender
            .send(frame)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "frame receiver stopped"))?;
    }
}

/// Reads and validates exactly one frame.
pub fn read_frame(stream: &mut TcpStream) -> io::Result<Frame> {
    let mut magic = [0; 4];
    stream.read_exact(&mut magic)?;
    if &magic != MAGIC {
        return Err(invalid("invalid frame magic"));
    }
    let width = read_u32(stream)?;
    let height = read_u32(stream)?;
    let id = read_u64(stream)?;
    let generation = read_u64(stream)?;
    let title_len = read_u32(stream)?;
    let pixel_len = read_u32(stream)?;
    if width == 0 || height == 0 || title_len > MAX_TITLE || pixel_len > MAX_PIXELS {
        return Err(invalid("invalid frame dimensions"));
    }
    let mut title = vec![0; title_len as usize];
    stream.read_exact(&mut title)?;
    let mut pixels = vec![0; pixel_len as usize];
    stream.read_exact(&mut pixels)?;
    Ok(Frame {
        id,
        title: String::from_utf8(title).map_err(|_| invalid("invalid title"))?,
        width,
        height,
        pixels,
        generation,
    })
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn read_u32(stream: &mut TcpStream) -> io::Result<u32> {
    let mut bytes = [0; 4];
    stream.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(stream: &mut TcpStream) -> io::Result<u64> {
    let mut bytes = [0; 8];
    stream.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn sample(width: u32, height: u32) -> Frame {
        Frame {
            id: 7,
            title: "surface".to_string(),
            width,
            height,
            pixels: (0..(width * height * 4)).map(|i| (i % 251) as u8).collect(),
            generation: 3,
        }
    }

    /// Connects a writer to a local listener and returns the received frame.
    fn round_trip(frame: Frame) -> Frame {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let expected = frame.clone();
        let writer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("connect");
            stream.write_all(&expected.encode()).expect("frame");
        });
        let (stream, _) = listener.accept().expect("accept");
        let (sender, receiver) = mpsc::channel();
        let result = read_frames(stream, &sender);
        assert!(result.is_err(), "stream ends after one frame");
        let received = receiver.recv().expect("frame");
        writer.join().expect("writer");
        received
    }

    #[test]
    fn header_is_thirty_six_bytes() {
        assert_eq!(encode_header(1, 2, 3, 4, 5, 6).len(), HEADER_LEN);
        assert_eq!(HEADER_LEN, 36);
    }

    #[test]
    fn round_trips_a_full_frame() {
        let frame = sample(640, 360);
        assert_eq!(round_trip(frame.clone()), frame);
    }

    #[test]
    fn round_trips_an_empty_title() {
        let mut frame = sample(2, 1);
        frame.title.clear();
        assert_eq!(round_trip(frame.clone()), frame);
    }

    #[test]
    fn rejects_bad_magic() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let writer = std::thread::spawn(move || {
            TcpStream::connect(address)
                .expect("connect")
                .write_all(b"NOPE")
                .expect("magic");
        });
        let (mut stream, _) = listener.accept().expect("accept");
        let error = read_frame(&mut stream).expect_err("bad magic must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        writer.join().expect("writer");
    }

    #[test]
    fn rejects_zero_width() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let bytes = sample(0, 4).encode();
        let writer = std::thread::spawn(move || {
            TcpStream::connect(address)
                .expect("connect")
                .write_all(&bytes)
                .expect("frame");
        });
        let (mut stream, _) = listener.accept().expect("accept");
        let error = read_frame(&mut stream).expect_err("zero width must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        writer.join().expect("writer");
    }

    #[test]
    fn rejects_oversized_title() {
        let mut bytes = encode_header(1, 4, 4, 1, MAX_TITLE as usize + 1, 64);
        bytes.resize(HEADER_LEN + 64, 0);
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let writer = std::thread::spawn(move || {
            let _ = TcpStream::connect(address).and_then(|mut s| s.write_all(&bytes));
        });
        let (mut stream, _) = listener.accept().expect("accept");
        assert!(read_frame(&mut stream).is_err());
        writer.join().expect("writer");
    }

    #[test]
    fn writer_reconnects_after_failure() {
        let mut writer = FrameWriter::default();
        let error = writer
            .send("127.0.0.1:1", &sample(1, 1))
            .expect_err("nothing listens on port 1");
        assert!(writer.stream.is_none(), "failed connect must not cache");
        assert_ne!(error.kind(), io::ErrorKind::OutOfMemory);
    }
}
