//! Minimal dependency-free Wayland wire endpoint for transport experiments.
//!
//! This module speaks the standard Wayland message framing and the registry
//! handshake over a byte stream. It intentionally does not implement waypipe's
//! private transport. TCP cannot carry the Unix fd ancillary data used by
//! `wl_shm.create_pool`; the parser therefore accepts the request shape but
//! does not expose pixel buffers yet.

#![allow(dead_code)]

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};

const MAX_MESSAGE: usize = 1024 * 1024;
const DISPLAY_OBJECT: u32 = 1;
const WL_DISPLAY_GET_REGISTRY: u16 = 1;
const WL_DISPLAY_SYNC: u16 = 0;
const WL_REGISTRY_BIND: u16 = 0;
const WL_REGISTRY_GLOBAL: u16 = 0;
const WL_COMPOSITOR_CREATE_SURFACE: u16 = 0;
const WL_SURFACE_COMMIT: u16 = 6;
const WL_SHM_CREATE_POOL: u16 = 0;
const WL_SHM_FORMAT: u16 = 0;
const XDG_WM_BASE_GET_XDG_SURFACE: u16 = 2;
const XDG_WM_BASE_PONG: u16 = 3;
const XDG_SURFACE_GET_TOPLEVEL: u16 = 1;
const XDG_SURFACE_ACK_CONFIGURE: u16 = 4;
const XDG_TOPLEVEL_SET_TITLE: u16 = 2;
const XDG_TOPLEVEL_CONFIGURE: u16 = 0;
const XDG_SURFACE_CONFIGURE: u16 = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaylandMessage {
    pub object_id: u32,
    pub opcode: u16,
    pub payload: Vec<u8>,
}

impl WaylandMessage {
    pub fn parse(header: [u8; 8], payload: Vec<u8>) -> io::Result<Self> {
        let object_id = u32::from_le_bytes(header[..4].try_into().unwrap());
        let word = u32::from_le_bytes(header[4..].try_into().unwrap());
        let length = (word >> 16) as usize;
        let opcode = (word & 0xffff) as u16;
        if length < 8 || !length.is_multiple_of(4) || length - 8 != payload.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid Wayland message length",
            ));
        }
        if length > MAX_MESSAGE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Wayland message too large",
            ));
        }
        Ok(Self {
            object_id,
            opcode,
            payload,
        })
    }

    pub fn encode(&self, out: &mut impl Write) -> io::Result<()> {
        let length = self.payload.len().checked_add(8).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Wayland message length overflow",
            )
        })?;
        if length > MAX_MESSAGE || length % 4 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid Wayland message length",
            ));
        }
        out.write_all(&self.object_id.to_le_bytes())?;
        out.write_all(&(((length as u32) << 16) | u32::from(self.opcode)).to_le_bytes())?;
        out.write_all(&self.payload)
    }
}

pub fn read_message(stream: &mut impl Read) -> io::Result<Option<WaylandMessage>> {
    let mut header = [0; 8];
    match stream.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let word = u32::from_le_bytes(header[4..].try_into().unwrap());
    let length = (word >> 16) as usize;
    if length < 8 || !length.is_multiple_of(4) || length > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid Wayland message header",
        ));
    }
    let mut payload = vec![0; length - 8];
    stream.read_exact(&mut payload)?;
    WaylandMessage::parse(header, payload).map(Some)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Global {
    Compositor,
    Shm,
    XdgWmBase,
}

impl Global {
    fn name(self) -> &'static [u8] {
        match self {
            Self::Compositor => b"wl_compositor\0",
            Self::Shm => b"wl_shm\0",
            Self::XdgWmBase => b"xdg_wm_base\0",
        }
    }

    fn version(self) -> u32 {
        match self {
            Self::Compositor => 4,
            Self::Shm => 1,
            Self::XdgWmBase => 6,
        }
    }
}

#[derive(Debug, Default)]
pub struct RegistryState {
    registry_object: Option<u32>,
    bound_compositor: Option<u32>,
    bound_shm: Option<u32>,
    bound_xdg_wm_base: Option<u32>,
    surfaces: Vec<u32>,
    xdg_surfaces: Vec<u32>,
    toplevels: Vec<u32>,
    next_serial: u32,
}

pub struct WaylandTcpServer {
    listener: TcpListener,
}

impl WaylandTcpServer {
    pub fn bind(address: &str) -> io::Result<Self> {
        Ok(Self {
            listener: TcpListener::bind(address)?,
        })
    }

    pub fn local_addr(&self) -> io::Result<std::net::SocketAddr> {
        self.listener.local_addr()
    }

    pub fn accept_once(&self) -> io::Result<()> {
        let (mut stream, _) = self.listener.accept()?;
        serve_client(&mut stream)
    }
}

pub fn serve_client(stream: &mut TcpStream) -> io::Result<()> {
    let mut state = RegistryState::default();
    while let Some(message) = read_message(stream)? {
        handle_message(stream, &mut state, message)?;
    }
    Ok(())
}

fn handle_message(
    stream: &mut impl Write,
    state: &mut RegistryState,
    message: WaylandMessage,
) -> io::Result<()> {
    match (message.object_id, message.opcode) {
        (DISPLAY_OBJECT, WL_DISPLAY_GET_REGISTRY) => {
            let new_id = read_u32(&message.payload, 0)?;
            state.registry_object = Some(new_id);
            for (name, global) in [
                (1, Global::Compositor),
                (2, Global::Shm),
                (3, Global::XdgWmBase),
            ] {
                send_global(stream, name, global)?;
            }
        }
        (DISPLAY_OBJECT, WL_DISPLAY_SYNC) => {
            let callback = read_u32(&message.payload, 0)?;
            send_message(stream, callback, 0, &1u32.to_le_bytes())?;
            send_message(stream, DISPLAY_OBJECT, 1, &callback.to_le_bytes())?;
        }
        (object, WL_REGISTRY_BIND) if state.registry_object == Some(object) => {
            let name = read_u32(&message.payload, 0)?;
            let interface = read_string(&message.payload, 4)?;
            let string_end = string_wire_end(&message.payload, 4)?;
            let _version = read_u32(&message.payload, string_end)?;
            let new_id = read_u32(&message.payload, string_end + 4)?;
            match (name, interface.as_str()) {
                (1, "wl_compositor") => state.bound_compositor = Some(new_id),
                (2, "wl_shm") => {
                    state.bound_shm = Some(new_id);
                    send_message(stream, new_id, WL_SHM_FORMAT, &0u32.to_le_bytes())?;
                    send_message(stream, new_id, WL_SHM_FORMAT, &1u32.to_le_bytes())?;
                }
                (3, "xdg_wm_base") => state.bound_xdg_wm_base = Some(new_id),
                _ => {}
            }
        }
        (object, WL_COMPOSITOR_CREATE_SURFACE) if state.bound_compositor == Some(object) => {
            let surface = read_u32(&message.payload, 0)?;
            state.surfaces.push(surface);
        }
        (object, WL_SHM_CREATE_POOL) if state.bound_shm == Some(object) => {
            // The fd is transported as SCM_RIGHTS on Unix and is unavailable on
            // this TCP endpoint. Consume the request shape without pretending
            // that a pixel buffer was created.
            let _pool = read_u32(&message.payload, 0)?;
        }
        (object, XDG_WM_BASE_GET_XDG_SURFACE) if state.bound_xdg_wm_base == Some(object) => {
            let xdg_surface = read_u32(&message.payload, 0)?;
            let _surface = read_u32(&message.payload, 4)?;
            state.xdg_surfaces.push(xdg_surface);
        }
        (object, XDG_WM_BASE_PONG) if state.bound_xdg_wm_base == Some(object) => {}
        (object, XDG_SURFACE_GET_TOPLEVEL) if state.xdg_surfaces.contains(&object) => {
            let toplevel = read_u32(&message.payload, 0)?;
            state.toplevels.push(toplevel);
            state.next_serial = state.next_serial.wrapping_add(1).max(1);
            let serial = state.next_serial;
            let mut configure = Vec::with_capacity(12);
            configure.extend_from_slice(&960i32.to_le_bytes());
            configure.extend_from_slice(&540i32.to_le_bytes());
            configure.extend_from_slice(&0u32.to_le_bytes());
            send_message(stream, toplevel, XDG_TOPLEVEL_CONFIGURE, &configure)?;
            send_message(stream, object, XDG_SURFACE_CONFIGURE, &serial.to_le_bytes())?;
        }
        (object, XDG_SURFACE_ACK_CONFIGURE) if state.xdg_surfaces.contains(&object) => {}
        (object, XDG_TOPLEVEL_SET_TITLE) if state.toplevels.contains(&object) => {
            let _title = read_string(&message.payload, 0)?;
        }
        (object, XDG_TOPLEVEL_CONFIGURE) if state.toplevels.contains(&object) => {}
        (object, WL_SURFACE_COMMIT) if state.surfaces.contains(&object) => {}
        _ => {}
    }
    stream.flush()
}

fn send_global(stream: &mut impl Write, name: u32, global: Global) -> io::Result<()> {
    let interface = global.name();
    let mut payload = Vec::with_capacity(12 + interface.len());
    payload.extend_from_slice(&name.to_le_bytes());
    payload.extend_from_slice(&(interface.len() as u32).to_le_bytes());
    payload.extend_from_slice(interface);
    while payload.len() % 4 != 0 {
        payload.push(0);
    }
    payload.extend_from_slice(&global.version().to_le_bytes());
    send_message(stream, 2, WL_REGISTRY_GLOBAL, &payload)
}

fn send_message(
    stream: &mut impl Write,
    object_id: u32,
    opcode: u16,
    payload: &[u8],
) -> io::Result<()> {
    WaylandMessage {
        object_id,
        opcode,
        payload: payload.to_vec(),
    }
    .encode(stream)
}

fn read_u32(payload: &[u8], offset: usize) -> io::Result<u32> {
    let bytes = payload
        .get(offset..offset + 4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing u32"))?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
}

fn string_wire_end(payload: &[u8], offset: usize) -> io::Result<usize> {
    let length = usize::try_from(read_u32(payload, offset)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "string too long"))?;
    let padded = length
        .checked_add(3)
        .map(|value| value & !3)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "string overflow"))?;
    offset
        .checked_add(4)
        .and_then(|value| value.checked_add(padded))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "string overflow"))
}

fn read_string(payload: &[u8], offset: usize) -> io::Result<String> {
    let length = usize::try_from(read_u32(payload, offset)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "string too long"))?;
    if length == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unterminated string",
        ));
    }
    let start = offset + 4;
    let end = start
        .checked_add(length)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "string overflow"))?;
    let bytes = payload
        .get(start..end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing string"))?;
    if bytes.last() != Some(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unterminated string",
        ));
    }
    String::from_utf8(bytes[..bytes.len() - 1].to_vec())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-8 string"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn round_trips_message_header() {
        let message = WaylandMessage {
            object_id: 2,
            opcode: 0,
            payload: b"wl_shm\0\0".to_vec(),
        };
        let mut bytes = Vec::new();
        message.encode(&mut bytes).unwrap();
        let parsed = read_message(&mut Cursor::new(bytes)).unwrap().unwrap();
        assert_eq!(parsed, message);
    }

    #[test]
    fn rejects_bad_length() {
        let error = WaylandMessage::parse([1, 0, 0, 0, 7, 0, 0, 0], Vec::new()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn registry_bind_uses_dynamic_new_id_order() {
        let mut state = RegistryState {
            registry_object: Some(2),
            ..Default::default()
        };
        let mut payload = Vec::new();
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&(14u32).to_le_bytes());
        payload.extend_from_slice(b"wl_compositor\0\0\0");
        payload.extend_from_slice(&1u32.to_le_bytes());
        payload.extend_from_slice(&3u32.to_le_bytes());
        handle_message(
            &mut Vec::new(),
            &mut state,
            WaylandMessage {
                object_id: 2,
                opcode: WL_REGISTRY_BIND,
                payload,
            },
        )
        .unwrap();
        assert_eq!(state.bound_compositor, Some(3));
    }

    #[test]
    fn xdg_surface_get_toplevel_sends_configure() {
        let mut state = RegistryState {
            bound_xdg_wm_base: Some(7),
            xdg_surfaces: vec![8],
            ..Default::default()
        };
        let mut output = Vec::new();
        handle_message(
            &mut output,
            &mut state,
            WaylandMessage {
                object_id: 8,
                opcode: XDG_SURFACE_GET_TOPLEVEL,
                payload: 9u32.to_le_bytes().to_vec(),
            },
        )
        .unwrap();
        assert_eq!(state.toplevels, vec![9]);
        let mut input = Cursor::new(output);
        let top = read_message(&mut input).unwrap().unwrap();
        assert_eq!((top.object_id, top.opcode), (9, XDG_TOPLEVEL_CONFIGURE));
        assert_eq!(top.payload.len(), 12);
        assert_eq!(&top.payload[8..12], &0u32.to_le_bytes());
        let surface = read_message(&mut input).unwrap().unwrap();
        assert_eq!(
            (surface.object_id, surface.opcode),
            (8, XDG_SURFACE_CONFIGURE)
        );
        assert_eq!(surface.payload, 1u32.to_le_bytes());
    }

    #[test]
    fn registry_handshake_advertises_three_globals() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let request = WaylandMessage {
            object_id: 1,
            opcode: 1,
            payload: 2u32.to_le_bytes().to_vec(),
        };
        let mut request_bytes = Vec::new();
        request.encode(&mut request_bytes).unwrap();
        let mut client_writer = client.try_clone().unwrap();
        client_writer.write_all(&request_bytes).unwrap();
        let mut state = RegistryState::default();
        handle_message(&mut server, &mut state, request).unwrap();
        let mut response = client;
        let first = read_message(&mut response).unwrap().unwrap();
        let second = read_message(&mut response).unwrap().unwrap();
        let third = read_message(&mut response).unwrap().unwrap();
        assert_eq!(
            [first.object_id, second.object_id, third.object_id],
            [2, 2, 2]
        );
        assert_eq!([first.opcode, second.opcode, third.opcode], [0, 0, 0]);
        assert_eq!(read_string(&first.payload, 4).unwrap(), "wl_compositor");
        let end = string_wire_end(&first.payload, 4).unwrap();
        assert_eq!(read_u32(&first.payload, end).unwrap(), 4);
    }
}
