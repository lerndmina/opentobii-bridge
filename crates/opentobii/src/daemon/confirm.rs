//! The nod that approves a face login.
//!
//! The login helper connects to the control socket after a face match and
//! sends `confirm`. This puts a banner on screen, watches the head pose for
//! a nod or a shake, and answers. One prompt at a time.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use opentobii_core::confirm::{Detector, Gesture};
use opentobii_core::control::{Answer, RawPose};
use opentobii_proto::tobiid as driver;
use rustix::event::{poll, PollFd, PollFlags};

use super::Shared;

/// How often the wait looks at the clock, the client and the banner.
const TICK: Duration = Duration::from_millis(50);
/// No pose for this long means the tracker is not tracking.
const TRACKING_WINDOW: Duration = Duration::from_secs(1);

/// Gives the one prompt slot back, and stops the pose feed, however the
/// prompt ends.
struct Slot<'a>(&'a Shared);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.lock().confirm_tap = None;
        self.0.confirming.store(false, Ordering::Release);
    }
}

/// The banner program and the pipe that tells it how the prompt ended.
struct Banner(Child);

impl Banner {
    fn start(seconds: f32, service: Option<&str>) -> std::io::Result<Self> {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("banner")
            .arg("--seconds")
            .arg(seconds.to_string());
        if let Some(service) = service {
            command.arg("--service").arg(service);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .map(Self)
    }

    /// False once the program has exited: nothing is on screen.
    fn showing(&mut self) -> bool {
        matches!(self.0.try_wait(), Ok(None))
    }

    /// Tell it the answer and leave it to show that for a moment and exit.
    /// With no answer it is closed at once.
    fn finish(mut self, answer: Option<Answer>) {
        let word = match answer {
            Some(Answer::Approved) => Some("approved"),
            Some(Answer::Denied) => Some("denied"),
            Some(Answer::Timeout) => Some("timeout"),
            Some(Answer::Unavailable) | None => None,
        };
        if let (Some(word), Some(pipe)) = (word, self.0.stdin.as_mut()) {
            let _ = writeln!(pipe, "{word}");
        }
        // Closing the pipe is what makes it exit when there was no answer.
        drop(self.0.stdin.take());
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(3));
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        });
    }
}

/// Whether the client that asked has gone: the dialog was cancelled.
fn hung_up(client: &UnixStream) -> bool {
    let mut fds = [PollFd::new(client, PollFlags::RDHUP)];
    let zero = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    match poll(&mut fds, Some(&zero)) {
        Ok(_) => fds[0]
            .revents()
            .intersects(PollFlags::RDHUP | PollFlags::HUP | PollFlags::ERR),
        Err(_) => false,
    }
}

/// A PAM service name fit to pass on to the banner.
fn plain(service: Option<String>) -> Option<String> {
    service.filter(|name| {
        !name.is_empty()
            && name.len() <= 32
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// Put the question and wait for the answer.
pub(super) fn run(shared: &Shared, client: &UnixStream, service: Option<String>) -> Answer {
    if shared.confirming.swap(true, Ordering::AcqRel) {
        shared.log("confirm: refused, another prompt is open");
        return Answer::Unavailable;
    }
    let _slot = Slot(shared);
    let service = plain(service);
    let asked_by = service.as_deref().unwrap_or("unknown");

    let (tap, poses) = mpsc::channel::<RawPose>();
    let seconds = {
        let mut state = shared.lock();
        let tracking = state
            .last_pose
            .is_some_and(|at| at.elapsed() < TRACKING_WINDOW);
        if !tracking || state.presence == driver::PRESENCE_AWAY {
            drop(state);
            shared.log(&format!(
                "confirm: for {asked_by}: unavailable, the tracker sees nobody"
            ));
            return Answer::Unavailable;
        }
        state.confirm_tap = Some(tap);
        state.tuning.confirm.seconds
    };

    let mut banner = if shared.config.confirm_banner {
        match Banner::start(seconds, service.as_deref()) {
            Ok(banner) => Some(banner),
            Err(err) => {
                shared.log(&format!("confirm: cannot start the banner: {err}"));
                return Answer::Unavailable;
            }
        }
    } else {
        None
    };
    shared.log(&format!("confirm: asking for {asked_by}, {seconds} s"));

    let deadline = Instant::now() + Duration::from_secs_f32(seconds);
    let mut detector = Detector::new();
    let answer = loop {
        match poses.recv_timeout(TICK) {
            Ok(pose) if pose.valid => {
                let [pitch, yaw, _] = pose.rotation_deg;
                match detector.push(pose.micros, pitch, yaw) {
                    Some(Gesture::Nod) => break Some(Answer::Approved),
                    Some(Gesture::Shake) => break Some(Answer::Denied),
                    None => {}
                }
            }
            Ok(_) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Some(Answer::Unavailable),
        }
        if Instant::now() >= deadline {
            break Some(Answer::Timeout);
        }
        if hung_up(client) {
            break None;
        }
        // A prompt nobody can see must not approve anything.
        if banner.as_mut().is_some_and(|banner| !banner.showing()) {
            shared.log("confirm: the banner is not showing");
            break Some(Answer::Unavailable);
        }
    };
    if let Some(banner) = banner {
        banner.finish(answer);
    }
    match answer {
        Some(answer) => {
            shared.log(&format!("confirm: for {asked_by}: {answer:?}"));
            answer
        }
        None => {
            shared.log(&format!("confirm: for {asked_by}: the asker went away"));
            Answer::Unavailable
        }
    }
}
