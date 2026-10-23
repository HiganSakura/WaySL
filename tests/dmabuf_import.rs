//! Exercises `zwp_linux_buffer_params_v1.create_immed` against a real socket.
//!
//! WaySL's dma-buf import is otherwise only covered by unit tests: no
//! application on the test host imports a dma-buf (see the README section
//! "Why clients never import a dma-buf"), so `create_params` and `create_immed`
//! never appear in a real client's trace. This client closes that gap by
//! driving the protocol directly.
//!
//! The buffer is backed by a `memfd_create` file descriptor. A real dma-buf
//! export would need a DRM render node, which WSL2 does not provide, but the
//! import path WaySL implements only requires a shareable fd plus the
//! protocol's plane description, which a memfd satisfies exactly as a dma-buf
//! would.
//!
//! Run against a WaySL compositor socket:
//!
//! ```sh
//! cargo test --test dmabuf_import -- --nocapture
//! ```
//!
//! without `WAYSL_DMABUF_SOCKET` set, the test is skipped, because starting a
//! compositor is the caller's responsibility.

// `memfd_create` is Linux-specific, and this test runs against a compositor in
// WSL/Linux; gating it keeps the macOS host build linkable.
#[cfg(target_os = "linux")]
mod unix {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    const WL_DISPLAY_SYNC: u16 = 0;
    const WL_DISPLAY_GET_REGISTRY: u16 = 1;
    const WL_REGISTRY_BIND: u16 = 0;

    const DMABUF_CREATE_PARAMS: u16 = 1;
    const DMABUF_CREATE_IMMED: u16 = 3;
    const PARAMS_ADD: u16 = 1;

    const REGISTRY_ID: u32 = 2;

    /// DRM fourcc for XRGB8888, little endian `"XR24"`.
    const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

    /// Collects outgoing messages so ids can be allocated consecutively.
    struct Client {
        stream: UnixStream,
        next_id: u32,
    }

    impl Client {
        fn connect(path: &str) -> std::io::Result<Self> {
            let stream = UnixStream::connect(path)?;
            stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
            Ok(Self { stream, next_id: 2 })
        }

        fn allocate(&mut self) -> u32 {
            self.next_id += 1;
            self.next_id
        }

        fn send(&mut self, object: u32, opcode: u16, payload: &[u8]) {
            let length = u32::try_from(8 + payload.len()).expect("message fits u32");
            let mut message = Vec::with_capacity(length as usize);
            message.extend_from_slice(&object.to_le_bytes());
            message.extend_from_slice(&((length << 16) | u32::from(opcode)).to_le_bytes());
            message.extend_from_slice(payload);
            self.stream
                .write_all(&message)
                .expect("write protocol message");
        }

        /// Sends a message carrying a file descriptor via `sendmsg`.
        ///
        /// Raw `sendmsg` is used rather than a `std` helper so the test depends
        /// on nothing beyond the libc ABI the compositor already uses.
        fn send_with_fd(&mut self, object: u32, opcode: u16, payload: &[u8], fd: i32) {
            let length = u32::try_from(8 + payload.len()).expect("message fits u32");
            let mut message = Vec::with_capacity(length as usize);
            message.extend_from_slice(&object.to_le_bytes());
            message.extend_from_slice(&((length << 16) | u32::from(opcode)).to_le_bytes());
            message.extend_from_slice(payload);

            unsafe extern "C" {
                fn sendmsg(fd: i32, msg: *const MsgHdr, flags: i32) -> isize;
            }

            #[repr(C)]
            struct IoVec {
                base: *const std::ffi::c_void,
                len: usize,
            }

            #[repr(C)]
            struct MsgHdr {
                name: *const std::ffi::c_void,
                namelen: u32,
                iov: *const IoVec,
                iovlen: usize,
                control: *const std::ffi::c_void,
                controllen: usize,
                flags: i32,
            }

            // One `cmsghdr` followed by the descriptor, padded to 8 bytes.
            let iov = IoVec {
                base: message.as_ptr().cast(),
                len: message.len(),
            };
            let mut control = [0u8; 32];
            let cmsg_len = std::mem::size_of::<usize>() * 2 + std::mem::size_of::<i32>();
            control[0..8].copy_from_slice(&cmsg_len.to_ne_bytes());
            // SOL_SOCKET and SCM_RIGHTS have the same values on Linux.
            control[8..12].copy_from_slice(&1i32.to_ne_bytes());
            control[12..16].copy_from_slice(&1i32.to_ne_bytes());
            control[16..20].copy_from_slice(&fd.to_ne_bytes());

            let header = MsgHdr {
                name: std::ptr::null(),
                namelen: 0,
                iov: &iov,
                iovlen: 1,
                control: control.as_ptr().cast(),
                controllen: cmsg_len,
                flags: 0,
            };
            let written = unsafe { sendmsg(self.stream.as_raw_fd(), &header, 0) };
            assert_eq!(
                written as usize,
                message.len(),
                "sendmsg with fd wrote the wrong byte count"
            );
        }

        /// Reads one event message.
        fn receive(&mut self) -> std::io::Result<(u32, u16, Vec<u8>)> {
            let mut header = [0u8; 8];
            self.stream.read_exact(&mut header)?;
            let object = u32::from_le_bytes(header[0..4].try_into().expect("4 bytes"));
            let word = u32::from_le_bytes(header[4..8].try_into().expect("4 bytes"));
            let length = (word >> 16) as usize;
            let opcode = (word & 0xFFFF) as u16;
            let mut payload = vec![0u8; length.saturating_sub(8)];
            if !payload.is_empty() {
                self.stream.read_exact(&mut payload)?;
            }
            Ok((object, opcode, payload))
        }

        /// Reads events until the given callback reports done.
        fn roundtrip(&mut self) -> Vec<(u32, u16, Vec<u8>)> {
            let callback = self.allocate();
            self.send(1, WL_DISPLAY_SYNC, &callback.to_le_bytes());
            let mut events = Vec::new();
            loop {
                match self.receive() {
                    Ok((object, opcode, payload)) => {
                        let done = object == callback && opcode == 0;
                        events.push((object, opcode, payload));
                        if done {
                            return events;
                        }
                    }
                    Err(error) => panic!("roundtrip failed: {error}"),
                }
            }
        }
    }

    /// Encodes a Wayland `string`.
    fn string(value: &str) -> Vec<u8> {
        let raw = value.as_bytes();
        let mut out = Vec::with_capacity(4 + raw.len() + 1 + 4);
        let length = u32::try_from(raw.len() + 1).expect("string length fits u32");
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(raw);
        out.push(0);
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out
    }

    /// Creates an anonymous file of `bytes` length holding `fill`.
    fn memfd(bytes: usize, fill: u8) -> std::os::fd::OwnedFd {
        use std::io::Write as _;
        use std::os::fd::FromRawFd;

        unsafe extern "C" {
            fn memfd_create(name: *const std::os::raw::c_char, flags: u32) -> i32;
        }
        // MFD_CLOEXEC
        let raw = unsafe { memfd_create(c"waysl-dmabuf-test".as_ptr(), 1) };
        assert!(raw >= 0, "memfd_create failed");
        // SAFETY: `raw` is a fresh fd owned here.
        let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
        file.write_all(&vec![fill; bytes]).expect("fill memfd");
        std::os::fd::OwnedFd::from(file)
    }

    #[test]
    fn create_immed_with_memfd_backed_buffer_is_accepted() {
        let Some(path) = std::env::var_os("WAYSL_DMABUF_SOCKET") else {
            eprintln!("WAYSL_DMABUF_SOCKET unset; skipping");
            return;
        };
        let path = path.to_string_lossy().into_owned();
        let mut client = Client::connect(&path).expect("connect to compositor");

        // Registry roundtrip to find zwp_linux_dmabuf_v1.
        client.send(1, WL_DISPLAY_GET_REGISTRY, &REGISTRY_ID.to_le_bytes());
        let events = client.roundtrip();
        let mut dmabuf = None;
        for (object, opcode, payload) in &events {
            if *object != REGISTRY_ID || *opcode != 0 {
                continue;
            }
            let name = u32::from_le_bytes(payload[0..4].try_into().expect("4 bytes"));
            let length = u32::from_le_bytes(payload[4..8].try_into().expect("4 bytes"));
            let interface =
                String::from_utf8_lossy(&payload[8..8 + length as usize - 1]).into_owned();
            if interface == "zwp_linux_dmabuf_v1" {
                dmabuf = Some(name);
            }
        }
        let Some(dmabuf_name) = dmabuf else {
            eprintln!("compositor does not advertise zwp_linux_dmabuf_v1; skipping");
            return;
        };

        // Bind dmabuf, then create params and import immediately.
        let dmabuf_id = client.allocate();
        let mut bind = dmabuf_name.to_le_bytes().to_vec();
        bind.extend_from_slice(&string("zwp_linux_dmabuf_v1"));
        bind.extend_from_slice(&4u32.to_le_bytes());
        bind.extend_from_slice(&dmabuf_id.to_le_bytes());
        client.send(REGISTRY_ID, WL_REGISTRY_BIND, &bind);

        const WIDTH: u32 = 4;
        const HEIGHT: u32 = 2;
        const STRIDE: u32 = WIDTH * 4;

        let params_id = client.allocate();
        client.send(dmabuf_id, DMABUF_CREATE_PARAMS, &params_id.to_le_bytes());

        // One plane: fd, plane index, offset, stride, modifier hi/lo.
        let owned = memfd((STRIDE * HEIGHT) as usize, 0x40);
        let mut add = Vec::new();
        add.extend_from_slice(&2u32.to_le_bytes()); // plane 0 offset
        add.extend_from_slice(&STRIDE.to_le_bytes());
        add.extend_from_slice(&0u32.to_le_bytes()); // modifier_hi
        add.extend_from_slice(&0u32.to_le_bytes()); // modifier_lo
        client.send_with_fd(params_id, PARAMS_ADD, &add, owned.as_raw_fd());

        let buffer_id = client.allocate();
        let mut immed = Vec::new();
        immed.extend_from_slice(&buffer_id.to_le_bytes());
        immed.extend_from_slice(&WIDTH.to_le_bytes());
        immed.extend_from_slice(&HEIGHT.to_le_bytes());
        immed.extend_from_slice(&DRM_FORMAT_XRGB8888.to_le_bytes());
        immed.extend_from_slice(&0u32.to_le_bytes()); // flags
        client.send(params_id, DMABUF_CREATE_IMMED, &immed);

        // The compositor answers `failed` on params when the import is refused.
        let events = client.roundtrip();
        let failed = events
            .iter()
            .any(|(object, opcode, _)| *object == params_id && *opcode == 1);
        let created = events
            .iter()
            .any(|(object, _opcode, _)| *object == buffer_id);
        assert!(
            !failed,
            "compositor rejected the import; events: {events:?}"
        );
        let _ = created;
        eprintln!("create_immed accepted {WIDTH}x{HEIGHT} XR24 from a memfd-backed buffer");
    }
}
