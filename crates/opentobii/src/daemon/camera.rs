//! The tracker's infrared picture, published as a camera for face login.
//!
//! While the camera is switched on, the daemon holds a `v4l2loopback`
//! device open as its writer. It asks the driver for pictures, and feeds
//! them on, only while some program is reading the device: face login
//! opens it for a second or two at a time, and the rest of the day no
//! picture of the user leaves the driver.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use opentobii_core::camera::{self, Camera, Levels};
use opentobii_proto::tobiid as driver;

use super::Shared;
use crate::v4l2::Output;

/// How often the settings are looked at while nothing else is happening.
const IDLE_CHECK: Duration = Duration::from_millis(500);
/// How long to leave a device or driver that failed before trying again.
const RETRY_AFTER: Duration = Duration::from_secs(5);
/// No picture from the driver for this long counts as a failure.
const PICTURE_TIMEOUT: Duration = Duration::from_secs(20);

/// Why a feed stopped without an error.
enum Stopped {
    /// The last reader went away.
    NoReaders,
    /// The camera's settings changed.
    Settings,
}

/// Run the camera for as long as the daemon lives.
pub(super) fn run(shared: Arc<Shared>) {
    loop {
        let settings = shared.lock().tuning.camera.clone();
        if !settings.enabled {
            report(&shared, "Off");
            thread::sleep(IDLE_CHECK);
            continue;
        }
        if let Err(err) = serve(&shared, &settings) {
            report(
                &shared,
                &format!("Cannot use {}: {}", settings.device, explain(&err)),
            );
            let since = Instant::now();
            while since.elapsed() < RETRY_AFTER && !changed(&shared, &settings) {
                thread::sleep(IDLE_CHECK);
            }
        }
    }
}

/// What to tell the user about a failure, with the fix where there is one.
fn explain(err: &io::Error) -> String {
    match err.kind() {
        io::ErrorKind::NotFound => {
            "there is no such device. Is the v4l2loopback module loaded?".to_owned()
        }
        io::ErrorKind::PermissionDenied => {
            "permission denied. Outside a desktop session the user needs to be in the video group."
                .to_owned()
        }
        _ => err.to_string(),
    }
}

/// Record what the camera is doing, logging it when it changes.
fn report(shared: &Shared, text: &str) {
    let mut state = shared.lock();
    if state.camera_status != text {
        text.clone_into(&mut state.camera_status);
        drop(state);
        shared.log(&format!("camera: {text}"));
    }
}

fn changed(shared: &Shared, settings: &Camera) -> bool {
    shared.lock().tuning.camera != *settings
}

/// Hold the device and feed readers until the settings change.
fn serve(shared: &Shared, settings: &Camera) -> io::Result<()> {
    let mut output = Output::open(
        Path::new(&settings.device),
        camera::WIDTH as u32,
        camera::HEIGHT as u32,
    )?;
    // One black frame makes this the device's writer, which is what lets a
    // reader start; until then a reader is refused.
    let mut frame = Vec::with_capacity(camera::FRAME_BYTES);
    for _ in 0..camera::WIDTH * camera::HEIGHT {
        frame.extend_from_slice(&[0, 128]);
    }
    output.write_frame(&frame)?;
    // An older v4l2loopback cannot say when a reader arrives: feed always.
    let on_demand = output.watch_readers().is_ok();
    let mut reading = !on_demand;
    loop {
        if changed(shared, settings) {
            return Ok(());
        }
        if !reading {
            report(
                shared,
                &format!(
                    "Ready on {}: feeds while a program reads it",
                    settings.device
                ),
            );
            if let Some(now) = output.readers(IDLE_CHECK)? {
                reading = now;
            }
            continue;
        }
        report(shared, &format!("Feeding {}", settings.device));
        match feed(shared, settings, &mut output, &mut frame, on_demand)? {
            Stopped::Settings => return Ok(()),
            Stopped::NoReaders => reading = false,
        }
    }
}

/// Take pictures from the driver and write them to the device.
fn feed(
    shared: &Shared,
    settings: &Camera,
    output: &mut Output,
    frame: &mut Vec<u8>,
    on_demand: bool,
) -> io::Result<Stopped> {
    let mut stream = UnixStream::connect(&shared.config.tobiid_socket)?;
    stream.set_read_timeout(Some(IDLE_CHECK))?;
    stream.write_all(&driver::frame(&driver::subscribe(driver::STREAM_IMAGE)))?;
    let mut levels = Levels::default();
    let mut pending = Vec::new();
    let mut chunk = vec![0u8; 256 * 1024];
    let mut last_picture = Instant::now();
    loop {
        if changed(shared, settings) {
            return Ok(Stopped::Settings);
        }
        if on_demand && output.readers(Duration::ZERO)? == Some(false) {
            return Ok(Stopped::NoReaders);
        }
        if last_picture.elapsed() > PICTURE_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the driver sent no picture",
            ));
        }
        let read = match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the driver closed the connection",
                ))
            }
            Ok(read) => read,
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue
            }
            Err(err) => return Err(err),
        };
        pending.extend_from_slice(&chunk[..read]);
        let mut used = 0;
        while let Some((body, rest)) = driver::unframe(&pending[used..]).map_err(|oversized| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("a driver message of {} bytes", oversized.0),
            )
        })? {
            used = pending.len() - rest.len();
            let Some(picture) = driver::image(body) else {
                continue;
            };
            if picture.bits != 8 {
                continue;
            }
            levels.update(picture.pixels);
            if camera::render(
                picture.pixels,
                picture.width as usize,
                picture.height as usize,
                &levels,
                frame,
            ) {
                output.write_frame(frame)?;
                last_picture = Instant::now();
            }
        }
        pending.drain(..used);
    }
}
