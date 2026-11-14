//! Writing pictures to a `v4l2loopback` device.
//!
//! The kernel's video interface is driven by `ioctl` calls that take C
//! structs, which Rust can only make through `unsafe`. This file holds all
//! of it, behind a small safe interface: open a device for output in one
//! picture format, write frames, and hear whether any program is reading.

// The only unsafe code in the program: three `ioctl` calls on structs laid
// out below to match the kernel's, each checked for size at compile time.
#![allow(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

use rustix::event::{poll, PollFd, PollFlags, Timespec};
use rustix::ioctl::{ioctl, opcode, Getter, Setter, Updater};

/// `V4L2_BUF_TYPE_VIDEO_OUTPUT`: we are the side that supplies pictures.
const BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
/// `V4L2_PIX_FMT_YUYV`: the format every webcam program accepts.
const PIX_FMT_YUYV: u32 = u32::from_le_bytes(*b"YUYV");
/// `V4L2_FIELD_NONE`: whole pictures, not interlaced halves.
const FIELD_NONE: u32 = 1;
/// `V4L2_COLORSPACE_SRGB`.
const COLORSPACE_SRGB: u32 = 8;
/// `V4L2_EVENT_PRI_CLIENT_USAGE`, from `v4l2loopback.c`: sent when a
/// reader starts or stops streaming. `V4L2_EVENT_PRIVATE_START` plus the
/// module's own offset plus one.
const EVENT_CLIENT_USAGE: u32 = 0x0800_0000 + 0x08E0_0000 + 1;
/// `V4L2_EVENT_SUB_FL_SEND_INITIAL`: send the state as it is now, too.
const EVENT_SEND_INITIAL: u32 = 1;

/// `struct v4l2_pix_format`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    private: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// `struct v4l2_format` on a 64-bit kernel: the type, padding to the
/// union's 8-byte alignment, then a 200-byte union of which only the
/// picture format is used here.
#[repr(C)]
struct Format {
    kind: u32,
    pad: u32,
    pix: PixFormat,
    rest: [u8; 152],
}

/// `struct v4l2_event_subscription`.
#[repr(C)]
#[derive(Default)]
struct EventSubscription {
    kind: u32,
    id: u32,
    flags: u32,
    reserved: [u32; 5],
}

/// `struct v4l2_event` on a 64-bit kernel.
#[repr(C)]
struct Event {
    kind: u32,
    pad: u32,
    /// The event's own data; for client usage, a `u32` count first.
    data: [u8; 64],
    pending: u32,
    sequence: u32,
    timestamp: [i64; 2],
    id: u32,
    reserved: [u32; 8],
    tail: u32,
}

const _: () = assert!(size_of::<PixFormat>() == 48);
const _: () = assert!(size_of::<Format>() == 208);
const _: () = assert!(size_of::<EventSubscription>() == 32);
const _: () = assert!(size_of::<Event>() == 136);

/// `VIDIOC_S_FMT`.
type SetFormat<'a> = Updater<'a, { opcode::read_write::<Format>(b'V', 5) }, Format>;
/// `VIDIOC_DQEVENT`.
type TakeEvent = Getter<{ opcode::read::<Event>(b'V', 89) }, Event>;
/// `VIDIOC_SUBSCRIBE_EVENT`.
type Subscribe = Setter<{ opcode::write::<EventSubscription>(b'V', 90) }, EventSubscription>;

/// A loopback device opened to supply YUYV pictures of one size.
#[derive(Debug)]
pub struct Output {
    file: File,
    frame_bytes: usize,
}

impl Output {
    /// Open `path` and declare pictures of `width` by `height`.
    pub fn open(path: &Path, width: u32, height: u32) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut format = Format {
            kind: BUF_TYPE_VIDEO_OUTPUT,
            pad: 0,
            pix: PixFormat {
                width,
                height,
                pixelformat: PIX_FMT_YUYV,
                field: FIELD_NONE,
                bytesperline: width * 2,
                sizeimage: width * height * 2,
                colorspace: COLORSPACE_SRGB,
                ..PixFormat::default()
            },
            rest: [0; 152],
        };
        // SAFETY: `Format` has the size and layout of the kernel's
        // `struct v4l2_format`, which is what VIDIOC_S_FMT reads and
        // writes; it lives until the call returns.
        unsafe { ioctl(&file, SetFormat::new(&mut format)) }?;
        let got = format.pix;
        if (got.width, got.height, got.pixelformat) != (width, height, PIX_FMT_YUYV) {
            return Err(io::Error::other(format!(
                "the device took {}x{} instead of {width}x{height}: is it a v4l2loopback device?",
                got.width, got.height
            )));
        }
        Ok(Self {
            file,
            frame_bytes: got.sizeimage as usize,
        })
    }

    /// Ask to be told when a program starts or stops reading. Fails on a
    /// `v4l2loopback` too old to say.
    pub fn watch_readers(&self) -> io::Result<()> {
        let subscription = EventSubscription {
            kind: EVENT_CLIENT_USAGE,
            flags: EVENT_SEND_INITIAL,
            ..EventSubscription::default()
        };
        // SAFETY: `EventSubscription` has the size and layout of the
        // kernel's `struct v4l2_event_subscription`, which
        // VIDIOC_SUBSCRIBE_EVENT only reads.
        unsafe { ioctl(&self.file, Subscribe::new(subscription)) }?;
        Ok(())
    }

    /// Wait up to `wait` for news of readers: `Some(true)` when a program
    /// is now reading, `Some(false)` when none is, `None` when nothing
    /// changed. Needs [`Self::watch_readers`] first.
    pub fn readers(&self, wait: Duration) -> io::Result<Option<bool>> {
        let mut news = None;
        let mut timeout = Timespec {
            tv_sec: i64::try_from(wait.as_secs()).unwrap_or(i64::MAX),
            tv_nsec: wait.subsec_nanos().into(),
        };
        loop {
            let mut fds = [PollFd::new(&self.file, PollFlags::PRI)];
            if poll(&mut fds, Some(&timeout))? == 0 || !fds[0].revents().contains(PollFlags::PRI) {
                return Ok(news);
            }
            // SAFETY: `Event` has the size and layout of the kernel's
            // `struct v4l2_event`, which VIDIOC_DQEVENT fills in.
            let event = unsafe { ioctl(&self.file, TakeEvent::new()) }?;
            if event.kind == EVENT_CLIENT_USAGE {
                let count = u32::from_ne_bytes([
                    event.data[0],
                    event.data[1],
                    event.data[2],
                    event.data[3],
                ]);
                news = Some(count > 0);
            }
            // Anything further is already waiting: do not wait again.
            timeout = Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
        }
    }

    /// Supply one picture. It must be exactly one frame long.
    pub fn write_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        if frame.len() != self.frame_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("a frame is {} bytes, not {}", self.frame_bytes, frame.len()),
            ));
        }
        self.file.write_all(frame)
    }
}
