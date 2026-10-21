//! Internal WaySL frame bridge between a Unix compositor and the Bevy renderer.
//!
//! This is deliberately not a Wayland protocol. Wayland remains the client-facing
//! protocol; this small framed stream carries already-composited RGBA surfaces to
//! a native Windows renderer when the compositor and renderer are separate.

use std::io::{self, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;

const MAGIC: &[u8; 4] = b"WSL1";
const MAX_TITLE: u32 = 4096;
const MAX_PIXELS: u32 = 256 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct Frame {
    pub id: u64,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
    pub generation: u64,
}

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
                        if let Err(error) = receive_stream(stream, &sender) {
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

/// Encode the fixed-size header for one frame: magic, geometry, identity, and
/// the payload lengths. The title bytes and pixels follow immediately after.
pub fn frame_header(
    id: u64,
    width: u32,
    height: u32,
    generation: u64,
    title_len: usize,
    pixel_len: usize,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(40);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&(title_len as u32).to_le_bytes());
    out.extend_from_slice(&(pixel_len as u32).to_le_bytes());
    out
}

pub fn receive_stream(mut stream: TcpStream, sender: &mpsc::Sender<Frame>) -> io::Result<()> {
    loop {
        let mut magic = [0; 4];
        stream.read_exact(&mut magic)?;
        if &magic != MAGIC {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid frame magic",
            ));
        }
        let width = read_u32(&mut stream)?;
        let height = read_u32(&mut stream)?;
        let id = read_u64(&mut stream)?;
        let generation = read_u64(&mut stream)?;
        let title_len = read_u32(&mut stream)?;
        let pixel_len = read_u32(&mut stream)?;
        if title_len > MAX_TITLE || pixel_len > MAX_PIXELS || width == 0 || height == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid frame dimensions",
            ));
        }
        let mut title_bytes = vec![0; title_len as usize];
        stream.read_exact(&mut title_bytes)?;
        let mut pixels = vec![0; pixel_len as usize];
        stream.read_exact(&mut pixels)?;
        sender
            .send(Frame {
                id,
                title: String::from_utf8(title_bytes)
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid title"))?,
                width,
                height,
                pixels,
                generation,
            })
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "frame receiver stopped"))?;
    }
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
    use std::io::Write;
    use std::net::TcpListener;

    fn encode(frame: &Frame) -> Vec<u8> {
        let title = frame.title.as_bytes();
        let mut out = frame_header(
            frame.id,
            frame.width,
            frame.height,
            frame.generation,
            title.len(),
            frame.pixels.len(),
        );
        out.extend_from_slice(title);
        out.extend_from_slice(&frame.pixels);
        out
    }

    /// `WSL1` magic + width + height + id + generation + two lengths.
    #[test]
    fn header_is_thirty_six_bytes() {
        assert_eq!(frame_header(1, 2, 3, 4, 5, 6).len(), 36);
    }

    #[test]
    fn round_trips_one_frame() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let expected = Frame {
            id: 7,
            title: "ok".to_string(),
            width: 2,
            height: 1,
            pixels: vec![1, 2, 3, 4],
            generation: 1,
        };
        let bytes = encode(&expected);
        let writer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("connect");
            stream.write_all(&bytes).expect("frame");
        });
        let (stream, _) = listener.accept().expect("accept");
        let (sender, receiver) = mpsc::channel();
        let _ = receive_stream(stream, &sender);
        let frame = receiver.recv().expect("frame");
        assert_eq!(frame.id, 7);
        assert_eq!(frame.title, "ok");
        assert_eq!(frame.width, 2);
        assert_eq!(frame.height, 1);
        assert_eq!(frame.pixels, vec![1, 2, 3, 4]);
        writer.join().expect("writer");
    }

    #[test]
    fn rejects_bad_magic() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let writer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("connect");
            stream.write_all(b"NOPE").expect("magic");
        });
        let (stream, _) = listener.accept().expect("accept");
        let (sender, _receiver) = mpsc::channel();
        let error = receive_stream(stream, &sender).expect_err("invalid magic must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        writer.join().expect("writer");
    }

    #[test]
    fn rejects_zero_width() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let frame = Frame {
            id: 1,
            title: String::new(),
            width: 0,
            height: 4,
            pixels: Vec::new(),
            generation: 1,
        };
        let bytes = encode(&frame);
        let writer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("connect");
            stream.write_all(&bytes).expect("frame");
        });
        let (stream, _) = listener.accept().expect("accept");
        let (sender, _receiver) = mpsc::channel();
        let error = receive_stream(stream, &sender).expect_err("zero width must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        writer.join().expect("writer");
    }

    /// The compositor writes frames with `frame_header`; the renderer must read
    /// them back with the same field order and lengths.
    #[test]
    fn header_written_by_forwarder_round_trips() {
        let title = "WaySL-Headless-E2E";
        let pixels: Vec<u8> = (0..(640 * 360 * 4)).map(|i| (i % 251) as u8).collect();
        let mut bytes = frame_header(12, 640, 360, 3, title.len(), pixels.len());
        bytes.extend_from_slice(title.as_bytes());
        bytes.extend_from_slice(&pixels);

        let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
        let address = listener.local_addr().expect("address");
        let writer = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).expect("connect");
            stream.write_all(&bytes).expect("frame");
        });
        let (stream, _) = listener.accept().expect("accept");
        let (sender, receiver) = mpsc::channel();
        let _ = receive_stream(stream, &sender);
        let frame = receiver.recv().expect("frame");
        assert_eq!(frame.id, 12);
        assert_eq!(frame.title, title);
        assert_eq!((frame.width, frame.height), (640, 360));
        assert_eq!(frame.generation, 3);
        assert_eq!(frame.pixels, pixels);
        writer.join().expect("writer");
    }
}
