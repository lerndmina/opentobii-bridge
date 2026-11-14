//! The control socket: live settings and a view of what the daemon sees,
//! for the GUI and the command line. Messages are in
//! [`opentobii_core::control`].

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use opentobii_core::config::game_key;
use opentobii_core::control::{GameStatus, GazePoint, Line, RawPose, Request, Status};

use super::{effective, spawn, Shared, State};

/// Longest request line accepted. A whole profile is under 2 kB.
const MAX_LINE: u64 = 64 * 1024;
/// A client that does not take a line within this long is dropped.
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);
const STATUS_EVERY: Duration = Duration::from_secs(1);
/// No pose for this long means the tracker is not tracking.
const TRACKING_WINDOW: Duration = Duration::from_secs(1);

/// One control client that lines are written to.
#[derive(Debug)]
pub(super) struct Watcher {
    id: u64,
    stream: Mutex<UnixStream>,
    /// The feeds this client asked for.
    poses: AtomicBool,
    gaze: AtomicBool,
}

impl Watcher {
    fn send(&self, line: &Line) -> io::Result<()> {
        let mut text = serde_json::to_string(line).map_err(io::Error::other)?;
        text.push('\n');
        let mut stream = self
            .stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stream.write_all(text.as_bytes())
    }

    fn close(&self) {
        let stream = self
            .stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = stream.shutdown(Shutdown::Both);
    }
}

/// Listen at `path`. A socket file left by a daemon that is gone is
/// replaced; one that a live daemon answers on is an error.
pub(super) fn bind(path: &Path) -> io::Result<UnixListener> {
    match UnixListener::bind(path) {
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!(
                        "another opentobii daemon is listening on {}",
                        path.display()
                    ),
                ));
            }
            std::fs::remove_file(path)?;
            UnixListener::bind(path)
        }
        other => other,
    }
}

pub(super) fn accept(listener: &UnixListener, shared: Arc<Shared>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => spawn("control", Arc::clone(&shared), move |shared| {
                if let Err(err) = serve(&shared, stream) {
                    shared.log(&format!("control: connection ended: {err}"));
                }
            }),
            Err(err) => {
                shared.log(&format!("control: accept failed: {err}"));
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn serve(shared: &Shared, stream: UnixStream) -> io::Result<()> {
    let writer = stream.try_clone()?;
    writer.set_write_timeout(Some(WRITE_TIMEOUT))?;
    let watcher = Arc::new(Watcher {
        id: shared.next_conn.fetch_add(1, Ordering::Relaxed),
        stream: Mutex::new(writer),
        poses: AtomicBool::new(false),
        gaze: AtomicBool::new(false),
    });
    let result = requests(shared, stream, &watcher);
    shared.lock().watchers.retain(|w| w.id != watcher.id);
    result
}

fn requests(shared: &Shared, stream: UnixStream, watcher: &Arc<Watcher>) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut text = String::new();
    loop {
        text.clear();
        let read = reader.by_ref().take(MAX_LINE).read_line(&mut text)?;
        if read == 0 {
            return Ok(());
        }
        if !text.ends_with('\n') && read as u64 == MAX_LINE {
            watcher.send(&Line::Error {
                reason: "request line too long".into(),
            })?;
            return Ok(());
        }
        if text.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Request>(&text) {
            Ok(request) => handle(shared, request, watcher),
            Err(err) => Line::Error {
                reason: format!("not a request: {err}"),
            },
        };
        watcher.send(&reply)?;
    }
}

fn refuse(reason: impl Into<String>) -> Line {
    Line::Error {
        reason: reason.into(),
    }
}

fn handle(shared: &Shared, request: Request, watcher: &Arc<Watcher>) -> Line {
    let mut state = shared.lock();
    let state = &mut *state;
    match request {
        Request::Get => Line::State {
            config: state.tuning.clone(),
            status: status(shared, state),
        },
        Request::SetOverlay { overlay } => {
            if let Err(reason) = overlay.validate() {
                return refuse(reason);
            }
            state.tuning.overlay = overlay.clone();
            let watchers = state.watchers.clone();
            let problem = apply(shared, state);
            // Told while the state is locked, so every subscriber sees the
            // changes in the order they were made.
            broadcast_locked(state, &watchers, &Line::Overlay(overlay));
            shared.overlay_changed();
            problem.unwrap_or(Line::Ok)
        }
        Request::SetCamera { camera } => {
            if let Err(reason) = camera.validate() {
                return refuse(reason);
            }
            // The camera's own thread sees the change within half a second.
            state.tuning.camera = camera.clone();
            let watchers = state.watchers.clone();
            let problem = apply(shared, state);
            broadcast_locked(state, &watchers, &Line::Camera(camera));
            problem.unwrap_or(Line::Ok)
        }
        Request::Subscribe { poses, gaze } => {
            watcher.poses.store(poses, Ordering::Relaxed);
            watcher.gaze.store(gaze, Ordering::Relaxed);
            if !state.watchers.iter().any(|w| w.id == watcher.id) {
                state.watchers.push(Arc::clone(watcher));
            }
            Line::Ok
        }
        Request::SetProfile { game, profile } => {
            if let Err(reason) = profile.validate() {
                return refuse(reason);
            }
            match game.as_deref().map(game_key) {
                None => state.tuning.default = profile,
                Some(key) if key.is_empty() => return refuse("a game needs a name"),
                Some(key) => {
                    state.tuning.games.insert(key, profile);
                }
            }
            apply(shared, state).unwrap_or(Line::Ok)
        }
        Request::DeleteProfile { game } => {
            if state.tuning.games.remove(&game_key(&game)).is_none() {
                return refuse(format!("no profile for {}", game_key(&game)));
            }
            apply(shared, state).unwrap_or(Line::Ok)
        }
        Request::Recentre { game } => {
            let Some(neutral) = state.preview.recentre() else {
                return refuse("no head pose yet: is the tracker running and can it see you?");
            };
            match game.as_deref().map(game_key) {
                None => state.tuning.default.neutral = neutral,
                Some(key) => match state.tuning.games.get_mut(&key) {
                    Some(profile) => profile.neutral = neutral,
                    None => return refuse(format!("no profile for {key}")),
                },
            }
            apply(shared, state).unwrap_or(Line::Recentred { neutral })
        }
    }
}

/// Put the settings in `state.tuning` to work and save them. Returns an
/// error line when something could not be done; the settings are in effect
/// either way.
fn apply(shared: &Shared, state: &mut State) -> Option<Line> {
    let mut problem = None;
    let default = effective(&shared.config, state.tuning.default);
    if let Err(reason) = state.preview.set_profile(default) {
        problem = Some(reason);
    }
    for game in state.games.values_mut() {
        let profile = effective(&shared.config, *state.tuning.profile_for(&game.executable));
        if let Err(reason) = game.pipeline.set_profile(profile) {
            problem = Some(reason);
        }
    }
    if let Some(path) = &shared.config.settings_path {
        if let Err(err) = state.tuning.save(path) {
            problem = Some(format!(
                "in effect, but not saved to {}: {err}",
                path.display()
            ));
        }
    }
    problem.map(refuse)
}

fn status(shared: &Shared, state: &State) -> Status {
    let mut games: Vec<GameStatus> = state
        .games
        .iter()
        .filter(|(pid, _)| state.pose_sinks.contains_key(pid))
        .map(|(pid, game)| GameStatus {
            pid: *pid,
            executable: game.executable.clone(),
            game: game_key(&game.executable),
            own_profile: state.tuning.has_profile(&game.executable),
            head_pose: state.posing.contains(pid),
        })
        .collect();
    games.sort_by_key(|game| game.pid);
    let now = Instant::now();
    Status {
        driver_connected: state.driver_connected,
        tracking: state
            .last_pose
            .is_some_and(|at| now.duration_since(at) < TRACKING_WINDOW),
        presence: state.presence,
        display_width_mm: state.display_width_mm,
        games,
        settings_path: shared.config.settings_path.clone(),
        camera: state.camera_status.clone(),
    }
}

/// As [`broadcast`], for a caller that already holds the state lock.
fn broadcast_locked(state: &mut State, watchers: &[Arc<Watcher>], line: &Line) {
    for watcher in watchers {
        if watcher.send(line).is_err() {
            watcher.close();
            state.watchers.retain(|w| w.id != watcher.id);
        }
    }
}

/// Send one line to every watcher; drop the ones that fail.
fn broadcast(shared: &Shared, watchers: &[Arc<Watcher>], line: &Line) {
    for watcher in watchers {
        if watcher.send(line).is_err() {
            watcher.close();
            shared.lock().watchers.retain(|w| w.id != watcher.id);
        }
    }
}

pub(super) fn send_pose(shared: &Shared, watchers: &[Arc<Watcher>], pose: RawPose) {
    let wanting: Vec<Arc<Watcher>> = watchers
        .iter()
        .filter(|w| w.poses.load(Ordering::Relaxed))
        .cloned()
        .collect();
    broadcast(shared, &wanting, &Line::Pose(pose));
}

pub(super) fn send_gaze(shared: &Shared, watchers: &[Arc<Watcher>], gaze: GazePoint) {
    let wanting: Vec<Arc<Watcher>> = watchers
        .iter()
        .filter(|w| w.gaze.load(Ordering::Relaxed))
        .cloned()
        .collect();
    broadcast(shared, &wanting, &Line::Gaze(gaze));
}

/// Tell subscribers what is connected, once a second.
pub(super) fn report_status(shared: Arc<Shared>) {
    loop {
        thread::sleep(STATUS_EVERY);
        let (watchers, status) = {
            let state = shared.lock();
            if state.watchers.is_empty() {
                continue;
            }
            (state.watchers.clone(), status(&shared, &state))
        };
        broadcast(&shared, &watchers, &Line::Status(status));
    }
}
