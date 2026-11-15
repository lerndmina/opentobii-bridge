//! Putting the tracker to sleep while nothing uses it.
//!
//! The tracker's infrared lights are on whenever it is tracking. The driver
//! can pause it: it then sends nothing, the lights go out, and a resume has
//! it sending again within a tenth of a second. The daemon pauses it once
//! nothing has used it for the time in the settings, and resumes it the
//! moment something does: a game or its helper connecting, a control client
//! subscribing (the tuning window, the gaze overlay), a program reading the
//! face login camera, or a nod prompt.
//!
//! The pause is asked for on the daemon's own driver connection, and the
//! driver ends a pause when the client that asked for it goes away. So a
//! daemon that stops or crashes leaves the tracker awake.
//!
//! A pause is one state for the whole tracker: other programs reading the
//! driver get nothing while it lasts. That is why it can be switched off.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use opentobii_proto::tobiid as driver;

use super::{Shared, State};

pub(super) const PAUSE_REQUEST_ID: u32 = 2;
pub(super) const RESUME_REQUEST_ID: u32 = 3;

const TICK: Duration = Duration::from_millis(500);
/// Poses still on their way when the pause was sent are not a sign that the
/// tracker woke by itself; poses after this long are.
const IN_FLIGHT: Duration = Duration::from_secs(2);

#[derive(Debug, Default)]
pub(super) struct Power {
    /// Since when nothing has been using the tracker.
    idle_since: Option<Instant>,
    /// When the pause now in effect was sent.
    asleep_since: Option<Instant>,
}

fn lock(shared: &Shared) -> std::sync::MutexGuard<'_, Power> {
    shared
        .power
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Ask the driver to pause or resume. False when there is no driver to ask.
fn send(shared: &Shared, paused: bool) -> bool {
    let id = if paused {
        PAUSE_REQUEST_ID
    } else {
        RESUME_REQUEST_ID
    };
    let mut writer = shared
        .driver_writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match writer.as_mut() {
        Some(stream) => stream
            .write_all(&driver::frame(&driver::pause(id, paused)))
            .is_ok(),
        None => false,
    }
}

/// Something is using the tracker from now: have it awake.
pub(super) fn wake(shared: &Shared) {
    let mut power = lock(shared);
    power.idle_since = None;
    if power.asleep_since.take().is_some() {
        shared.asleep.store(false, Ordering::Release);
        // With no driver connection there is nothing to resume: the driver
        // ended the pause when the connection went.
        if send(shared, false) {
            drop(power);
            shared.log("tracker: awake");
        }
    }
}

/// The driver connection is up. A new connection finds the tracker awake.
pub(super) fn connected(shared: &Shared, stream: &UnixStream) -> std::io::Result<()> {
    let writer = stream.try_clone()?;
    *shared
        .driver_writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(writer);
    *lock(shared) = Power::default();
    shared.asleep.store(false, Ordering::Release);
    Ok(())
}

pub(super) fn disconnected(shared: &Shared) {
    *shared
        .driver_writer
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    *lock(shared) = Power::default();
    shared.asleep.store(false, Ordering::Release);
}

/// The driver answered a pause or resume with something other than success.
pub(super) fn refused(shared: &Shared, request_id: u32, status: u8) {
    if request_id == PAUSE_REQUEST_ID {
        let mut power = lock(shared);
        power.asleep_since = None;
        // Wait a whole idle time before asking again.
        power.idle_since = Some(Instant::now());
        shared.asleep.store(false, Ordering::Release);
        drop(power);
        shared.log(&format!(
            "tracker: the driver refused the pause (status {status}); it stays awake for now"
        ));
    } else {
        shared.log(&format!(
            "tracker: the driver refused the resume (status {status})"
        ));
    }
}

/// A head pose arrived while the tracker is meant to be asleep. Past the
/// poses that were in flight, that means the tracker started again by
/// itself (it was plugged back in, say); the next tick pauses it again.
pub(super) fn pose_while_asleep(shared: &Shared) {
    let mut power = lock(shared);
    if power
        .asleep_since
        .is_some_and(|since| since.elapsed() > IN_FLIGHT)
    {
        power.asleep_since = None;
        shared.asleep.store(false, Ordering::Release);
        drop(power);
        shared.log("tracker: woke by itself");
    }
}

/// Whether anything needs the tracker's data now.
fn in_use(shared: &Shared) -> bool {
    shared.game_conns.load(Ordering::Acquire) > 0
        || shared.camera_reading.load(Ordering::Acquire)
        || shared.confirming.load(Ordering::Acquire)
        || !shared.lock().watchers.is_empty()
}

/// What the tracker is doing, in words, for the status line.
pub(super) fn describe(shared: &Shared, state: &State) -> String {
    if !state.driver_connected {
        "No driver".to_owned()
    } else if shared.asleep.load(Ordering::Acquire) {
        "Asleep: nothing is using it".to_owned()
    } else if state.tuning.tracker.sleep_when_idle {
        "Awake".to_owned()
    } else {
        "Awake: set to stay on".to_owned()
    }
}

/// Pause the tracker once it has been idle long enough; wake it when the
/// setting is switched off or something turns out to be using it.
pub(super) fn run(shared: Arc<Shared>) {
    loop {
        thread::sleep(TICK);
        let settings = shared.lock().tuning.tracker;
        if !settings.sleep_when_idle || in_use(&shared) {
            wake(&shared);
            continue;
        }
        let idle = shared
            .config
            .idle_after
            .unwrap_or_else(|| Duration::from_secs_f32(settings.idle_seconds));
        let mut power = lock(&shared);
        let since = *power.idle_since.get_or_insert_with(Instant::now);
        if power.asleep_since.is_some() || since.elapsed() < idle || !send(&shared, true) {
            continue;
        }
        power.asleep_since = Some(Instant::now());
        shared.asleep.store(true, Ordering::Release);
        drop(power);
        // Who is in front of the tracker is not known while it sleeps.
        shared.lock().presence = driver::PRESENCE_UNKNOWN;
        shared.log("tracker: asleep, nothing is using it");
    }
}
