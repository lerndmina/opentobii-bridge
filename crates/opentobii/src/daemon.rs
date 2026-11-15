//! The running bridge: listeners, connections and the path a sample takes
//! from `tobiid` to the game.

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use opentobii_core::config::{game_key, Config as Tuning};
use opentobii_core::control::{GazePoint, RawPose};
use opentobii_core::pipeline::{Pipeline, Pose, Profile};
use opentobii_proto::frames::{self, DisplayFrame, Template};
use opentobii_proto::tobiid::{self, GazeRaw, HeadPose, ServerMsg};
use opentobii_proto::ttp::{self, Message};
use opentobii_proto::{sesp, tobiid as driver};

use crate::recorded;

mod camera;
mod confirm;
mod control;
mod power;

pub const TTP_PORT: u16 = 4455;
pub const RELAY_PORT: u16 = 47455;

const CMD_DISPLAY_AREA: u32 = 1430;
const CMD_STREAM_START: u32 = 1220;
const CMD_STREAM_STOP: u32 = 1230;
const CMD_UNLOCK_PPU: u32 = 1600;
const STREAM_GAZE: u32 = 0x500;
const STREAM_PRESENCE: u32 = 0x504;

const AREA_REQUEST_ID: u32 = 1;
const DRIVER_STREAMS: u32 = driver::STREAM_GAZE_RAW
    | driver::STREAM_PRESENCE
    | driver::STREAM_HEAD_POSE
    | driver::STREAM_NOTIFICATIONS;
const CAPABILITIES: [u16; 3] = [
    sesp::CAP_HEAD_POSE,
    sesp::CAP_DISPLAY_INFO,
    sesp::CAP_STREAM_STATUS,
];

/// Largest TTP payload a game may send. Its real commands are under 100
/// bytes.
const MAX_COMMAND: usize = 1 << 16;
/// A client that does not take a frame within this long is dropped, so one
/// stuck game cannot hold up the samples for the others.
const WRITE_TIMEOUT: Duration = Duration::from_millis(250);
const RECONNECT_EVERY: Duration = Duration::from_secs(2);
/// The helper's hello: magic, channel, pid, slot, counter, monitor count.
const HELLO_LEN: usize = 4 + 4 + 4 + 4 + 8 + 4;
const MAX_MONITORS: u32 = 64;

/// What to listen on and where the driver is.
#[derive(Debug, Clone)]
pub struct Config {
    pub ttp_addr: SocketAddr,
    pub relay_addr: SocketAddr,
    pub tobiid_socket: PathBuf,
    /// The settings file's contents: a default profile and one per game.
    pub tuning: Tuning,
    /// Send head poses unsmoothed whatever the profiles say.
    pub force_unsmoothed: bool,
    /// Where settings changed over the control socket are saved.
    pub settings_path: Option<PathBuf>,
    /// Where the control socket listens; `None` for no control socket.
    pub control_socket: Option<PathBuf>,
    /// Run the gaze overlay program while the overlay is switched on.
    pub manage_overlay: bool,
    /// Which monitor to report to the game; `None` picks the primary.
    pub monitor_index: Option<usize>,
    /// Show the banner while a nod is being asked for. Without it nothing
    /// tells the person to nod, so only tests switch it off.
    pub confirm_banner: bool,
    /// How long the tracker stays awake after its last use, in place of the
    /// setting. Tests use it to avoid waiting the setting's shortest time.
    pub idle_after: Option<Duration>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ttp_addr: SocketAddr::from(([127, 0, 0, 1], TTP_PORT)),
            relay_addr: SocketAddr::from(([127, 0, 0, 1], RELAY_PORT)),
            tobiid_socket: driver::socket_path(),
            tuning: Tuning::default(),
            force_unsmoothed: false,
            settings_path: None,
            control_socket: Some(opentobii_core::control::socket_path()),
            manage_overlay: true,
            monitor_index: None,
            confirm_banner: true,
            idle_after: None,
        }
    }
}

/// Counters, readable while the daemon runs.
#[derive(Debug, Default)]
pub struct Stats {
    pub gaze_frames: AtomicU64,
    pub presence_frames: AtomicU64,
    pub poses_sent: AtomicU64,
    pub poses_dropped: AtomicU64,
}

/// A started daemon. Its threads run until the process exits.
#[derive(Debug)]
pub struct Running {
    pub ttp_addr: SocketAddr,
    pub relay_addr: SocketAddr,
    /// The control socket's path, when there is one.
    pub control_socket: Option<PathBuf>,
    shared: Arc<Shared>,
}

impl Running {
    #[must_use]
    pub fn stats(&self) -> &Stats {
        &self.shared.stats
    }

    /// Whether the driver has told us the display area yet.
    #[must_use]
    pub fn has_display_area(&self) -> bool {
        self.shared.lock().display.is_some()
    }
}

/// One client connection that frames are pushed to.
#[derive(Debug)]
struct Conn {
    id: u64,
    stream: Mutex<TcpStream>,
}

impl Conn {
    /// Write one whole frame. An error means the peer is gone or stuck.
    fn send(&self, bytes: &[u8]) -> io::Result<()> {
        let mut stream = self
            .stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stream.write_all(bytes)
    }

    fn close(&self) {
        let stream = self
            .stream
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _ = stream.shutdown(Shutdown::Both);
    }
}

/// One game process: which executable it is and its pose pipeline.
#[derive(Debug)]
struct Game {
    executable: String,
    pipeline: Pipeline,
}

#[derive(Debug)]
struct State {
    /// Game clock minus `CLOCK_MONOTONIC`, microseconds, from the helper's
    /// hello. `None` until a helper has connected.
    clock_offset: Option<i64>,
    display: Option<DisplayFrame>,
    area_reply: Option<Vec<u8>>,
    presence: u8,
    monitor_id: String,
    gaze_sinks: Vec<Arc<Conn>>,
    presence_sinks: Vec<Arc<Conn>>,
    /// Subscription channel by game process id.
    pose_sinks: HashMap<u32, Arc<Conn>>,
    /// Game processes that asked for head pose.
    posing: HashSet<u32>,
    sequences: HashMap<u32, u32>,
    tuning: Tuning,
    /// What is known about each game process, by process id.
    games: HashMap<u32, Game>,
    /// The default profile's pipeline, run on every pose whether or not a
    /// game is connected: a recentre takes its neutral pose from here.
    preview: Pipeline,
    driver_connected: bool,
    last_pose: Option<Instant>,
    display_width_mm: Option<f64>,
    /// What the face login camera is doing, in words.
    camera_status: String,
    /// Control clients that subscribed to poses and status.
    watchers: Vec<Arc<control::Watcher>>,
    /// Where head poses also go while a nod is being asked for.
    confirm_tap: Option<std::sync::mpsc::Sender<RawPose>>,
}

#[derive(Debug)]
struct Shared {
    config: Config,
    state: Mutex<State>,
    replies: HashMap<u32, Vec<u8>>,
    gaze_template: Template,
    presence_template: Template,
    stats: Stats,
    next_conn: AtomicU64,
    started: Instant,
    /// Set when the overlay's settings change, to let its keeper try again.
    overlay_retry: std::sync::atomic::AtomicBool,
    /// A nod is being asked for; a second prompt is refused meanwhile.
    confirming: std::sync::atomic::AtomicBool,
    /// Open connections from games and their helpers.
    game_conns: std::sync::atomic::AtomicU32,
    /// A program is reading the face login camera.
    camera_reading: std::sync::atomic::AtomicBool,
    /// The tracker has been paused. The details are in `power`.
    asleep: std::sync::atomic::AtomicBool,
    power: Mutex<power::Power>,
    /// The daemon's driver connection, for the requests `power` sends.
    driver_writer: Mutex<Option<UnixStream>>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic in one connection thread must not take the daemon down.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn log(&self, text: &str) {
        eprintln!("{:8.2}s {text}", self.started.elapsed().as_secs_f64());
    }

    fn conn(&self, stream: &TcpStream) -> io::Result<Arc<Conn>> {
        let writer = stream.try_clone()?;
        writer.set_write_timeout(Some(WRITE_TIMEOUT))?;
        writer.set_nodelay(true)?;
        Ok(Arc::new(Conn {
            id: self.next_conn.fetch_add(1, Ordering::Relaxed),
            stream: Mutex::new(writer),
        }))
    }

    /// A `tobiid` host time on the clock the game reads.
    fn game_micros(&self, host_micros: Option<i64>, offset: Option<i64>) -> u64 {
        let host = host_micros.unwrap_or_else(monotonic_micros);
        u64::try_from(host.saturating_add(offset.unwrap_or(0))).unwrap_or(0)
    }

    /// Send `bytes` to every sink; drop the ones that fail.
    fn broadcast(&self, sinks: &[Arc<Conn>], bytes: &[u8]) -> usize {
        let mut sent = 0;
        for sink in sinks {
            if sink.send(bytes).is_ok() {
                sent += 1;
            } else {
                sink.close();
                self.forget(sink.id);
            }
        }
        sent
    }

    /// The overlay's settings changed. A fresh start is allowed even if it
    /// kept failing under the old settings.
    fn overlay_changed(&self) {
        self.overlay_retry.store(true, Ordering::Relaxed);
    }

    fn forget(&self, id: u64) {
        let mut state = self.lock();
        state.gaze_sinks.retain(|c| c.id != id);
        state.presence_sinks.retain(|c| c.id != id);
        state.pose_sinks.retain(|_, c| c.id != id);
    }
}

/// `CLOCK_MONOTONIC` in microseconds: the clock `tobiid` stamps samples
/// with.
#[must_use]
pub fn monotonic_micros() -> i64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    now.tv_sec
        .saturating_mul(1_000_000)
        .saturating_add(now.tv_nsec / 1_000)
}

/// Bind both listeners and start the threads. Returns once the daemon is
/// accepting connections.
pub fn start(config: Config) -> io::Result<Running> {
    let ttp_listener = TcpListener::bind(config.ttp_addr)?;
    let relay_listener = TcpListener::bind(config.relay_addr)?;
    config
        .tuning
        .validate()
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err.to_string()))?;
    let preview = Pipeline::new(effective(&config, config.tuning.default))
        .map_err(|reason| io::Error::new(io::ErrorKind::InvalidInput, reason))?;
    let control_listener = match &config.control_socket {
        Some(path) => Some(control::bind(path)?),
        None => None,
    };
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            clock_offset: None,
            display: None,
            area_reply: None,
            presence: driver::PRESENCE_UNKNOWN,
            monitor_id: String::new(),
            gaze_sinks: Vec::new(),
            presence_sinks: Vec::new(),
            pose_sinks: HashMap::new(),
            posing: HashSet::new(),
            sequences: HashMap::new(),
            tuning: config.tuning.clone(),
            games: HashMap::new(),
            preview,
            driver_connected: false,
            last_pose: None,
            display_width_mm: None,
            camera_status: String::new(),
            watchers: Vec::new(),
            confirm_tap: None,
        }),
        replies: recorded::replies(),
        gaze_template: Template::new(recorded::gaze_frame()),
        presence_template: Template::new(recorded::presence_frame()),
        stats: Stats::default(),
        overlay_retry: std::sync::atomic::AtomicBool::new(false),
        confirming: std::sync::atomic::AtomicBool::new(false),
        game_conns: std::sync::atomic::AtomicU32::new(0),
        camera_reading: std::sync::atomic::AtomicBool::new(false),
        asleep: std::sync::atomic::AtomicBool::new(false),
        power: Mutex::new(power::Power::default()),
        driver_writer: Mutex::new(None),
        next_conn: AtomicU64::new(1),
        started: Instant::now(),
        config,
    });
    let running = Running {
        ttp_addr: ttp_listener.local_addr()?,
        relay_addr: relay_listener.local_addr()?,
        control_socket: shared.config.control_socket.clone(),
        shared: Arc::clone(&shared),
    };

    spawn("ttp-accept", Arc::clone(&shared), move |shared| {
        accept(&ttp_listener, shared, "ttp", serve_game)
    });
    spawn("relay-accept", Arc::clone(&shared), move |shared| {
        accept(&relay_listener, shared, "relay", serve_helper)
    });
    if let Some(listener) = control_listener {
        spawn("control-accept", Arc::clone(&shared), move |shared| {
            control::accept(&listener, shared)
        });
        spawn(
            "control-status",
            Arc::clone(&shared),
            control::report_status,
        );
        if shared.config.manage_overlay {
            spawn("overlay-keeper", Arc::clone(&shared), keep_overlay);
        }
    }
    spawn("camera", Arc::clone(&shared), camera::run);
    spawn("power", Arc::clone(&shared), power::run);
    spawn("tobiid", shared, follow_driver);
    Ok(running)
}

fn spawn(name: &str, shared: Arc<Shared>, body: impl FnOnce(Arc<Shared>) + Send + 'static) {
    let spawned = thread::Builder::new().name(name.to_owned()).spawn({
        let shared = Arc::clone(&shared);
        move || body(shared)
    });
    if let Err(err) = spawned {
        shared.log(&format!("cannot start thread {name}: {err}"));
    }
}

fn accept(
    listener: &TcpListener,
    shared: Arc<Shared>,
    name: &'static str,
    serve: fn(&Shared, TcpStream) -> io::Result<()>,
) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => spawn(name, Arc::clone(&shared), move |shared| {
                // A game is there for as long as it holds a connection.
                shared.game_conns.fetch_add(1, Ordering::AcqRel);
                power::wake(&shared);
                let ended = serve(&shared, stream);
                shared.game_conns.fetch_sub(1, Ordering::AcqRel);
                if let Err(err) = ended {
                    if err.kind() != io::ErrorKind::UnexpectedEof {
                        shared.log(&format!("{name}: connection ended: {err}"));
                    }
                }
            }),
            Err(err) => {
                shared.log(&format!("{name}: accept failed: {err}"));
                thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

// --- the game's TTP connection ---------------------------------------------

fn serve_game(shared: &Shared, mut stream: TcpStream) -> io::Result<()> {
    let conn = shared.conn(&stream)?;
    let result = game_loop(shared, &mut stream, &conn);
    shared.forget(conn.id);
    result
}

fn game_loop(shared: &Shared, stream: &mut TcpStream, conn: &Arc<Conn>) -> io::Result<()> {
    loop {
        let mut head = [0u8; ttp::HEADER_LEN];
        stream.read_exact(&mut head)?;
        let header = ttp::Header::decode(&head).map_err(invalid)?;
        let length = header.payload_len as usize;
        if length > MAX_COMMAND {
            return Err(invalid(format!("command of {length} bytes")));
        }
        let mut payload = vec![0u8; length];
        stream.read_exact(&mut payload)?;
        let request = Message {
            marker: header.marker,
            seq: header.seq,
            status: header.status,
            id: header.id,
            error: header.error,
            payload,
        };
        let reply = answer(shared, &request);
        if reply.error != ttp::ERROR_NONE {
            shared.log(&format!(
                "ttp: command {} refused ({:#x})",
                request.id, reply.error
            ));
        }
        conn.send(&reply.encode())?;
        after_reply(shared, &request, conn);
    }
}

fn answer(shared: &Shared, request: &Message) -> Message {
    if request.id == CMD_DISPLAY_AREA {
        if let Some(live) = shared.lock().area_reply.clone() {
            return request.response(live, ttp::ERROR_NONE);
        }
    }
    if let Some(recorded) = shared.replies.get(&request.id) {
        return request.response(recorded.clone(), ttp::ERROR_NONE);
    }
    match request.id {
        // No recording of this one. Three u32 zeros is what the DLL accepts.
        CMD_UNLOCK_PPU => {
            let zero = ttp::u32_item(0);
            request.response(ttp::payload(&[&zero, &zero, &zero]), ttp::ERROR_NONE)
        }
        CMD_STREAM_START | CMD_STREAM_STOP => request.response(Vec::new(), ttp::ERROR_NONE),
        _ => request.response(Vec::new(), ttp::ERROR_UNSUPPORTED),
    }
}

fn after_reply(shared: &Shared, request: &Message, conn: &Arc<Conn>) {
    let stream = ttp::first_u32(&request.payload);
    match (request.id, stream) {
        (CMD_STREAM_START, Some(STREAM_GAZE)) => {
            let mut state = shared.lock();
            if !state.gaze_sinks.iter().any(|c| c.id == conn.id) {
                state.gaze_sinks.push(Arc::clone(conn));
            }
            drop(state);
            shared.log("ttp: game subscribed to gaze");
        }
        (CMD_STREAM_START, Some(STREAM_PRESENCE)) => {
            let (status, offset) = {
                let mut state = shared.lock();
                if !state.presence_sinks.iter().any(|c| c.id == conn.id) {
                    state.presence_sinks.push(Arc::clone(conn));
                }
                (state.presence, state.clock_offset)
            };
            if status != driver::PRESENCE_UNKNOWN {
                let micros = shared.game_micros(None, offset);
                let payload = frames::presence_frame(&shared.presence_template, status, micros);
                let _ = conn.send(&Message::stream(STREAM_PRESENCE, payload).encode());
            }
        }
        (CMD_STREAM_STOP, Some(STREAM_GAZE)) => {
            shared.lock().gaze_sinks.retain(|c| c.id != conn.id)
        }
        (CMD_STREAM_STOP, Some(STREAM_PRESENCE)) => {
            shared.lock().presence_sinks.retain(|c| c.id != conn.id)
        }
        _ => {}
    }
}

// --- the helper's relay connection -----------------------------------------

struct Hello {
    channel: u32,
    pid: u32,
    slot: u32,
    counter: u64,
    monitors: Vec<(bool, String)>,
    /// The game's executable path; empty from a helper before `OTB2`.
    executable: String,
}

fn read_hello(stream: &mut TcpStream) -> io::Result<Hello> {
    let mut fixed = [0u8; HELLO_LEN];
    stream.read_exact(&mut fixed)?;
    let with_executable = match &fixed[..4] {
        b"OTB1" => false,
        b"OTB2" => true,
        _ => return Err(invalid("not a helper hello")),
    };
    let word =
        |at: usize| u32::from_le_bytes([fixed[at], fixed[at + 1], fixed[at + 2], fixed[at + 3]]);
    let counter = u64::from_le_bytes(fixed[16..24].try_into().expect("eight bytes"));
    let count = word(24);
    if count > MAX_MONITORS {
        return Err(invalid(format!("{count} monitors")));
    }
    let mut monitors = Vec::new();
    for _ in 0..count {
        let mut head = [0u8; 3];
        stream.read_exact(&mut head)?;
        let mut name = vec![0u8; usize::from(u16::from_le_bytes([head[1], head[2]]))];
        stream.read_exact(&mut name)?;
        monitors.push((head[0] != 0, String::from_utf8_lossy(&name).into_owned()));
    }
    let mut executable = String::new();
    if with_executable {
        let mut length = [0u8; 2];
        stream.read_exact(&mut length)?;
        let mut path = vec![0u8; usize::from(u16::from_le_bytes(length))];
        stream.read_exact(&mut path)?;
        executable = String::from_utf8_lossy(&path).into_owned();
    }
    Ok(Hello {
        channel: word(4),
        pid: word(8),
        slot: word(12),
        counter,
        monitors,
        executable,
    })
}

/// A profile as the daemon runs it: the command line can turn smoothing off.
fn effective(config: &Config, mut profile: Profile) -> Profile {
    if config.force_unsmoothed {
        profile.smoothing.enabled = false;
    }
    profile
}

/// Give a game process its pipeline, from its own profile or the default.
/// Returns a line for the log when the process is new or changed.
fn register_game(shared: &Shared, state: &mut State, pid: u32, executable: &str) -> Option<String> {
    if state
        .games
        .get(&pid)
        .is_some_and(|game| game.executable == executable)
    {
        return None;
    }
    let profile = effective(&shared.config, *state.tuning.profile_for(executable));
    let source = if executable.is_empty() {
        "the default profile (this helper does not report the game)".to_owned()
    } else if state.tuning.has_profile(executable) {
        format!("its own profile ({})", game_key(executable))
    } else {
        format!("the default profile (none for {})", game_key(executable))
    };
    match Pipeline::new(profile) {
        Ok(pipeline) => {
            let game = Game {
                executable: executable.to_owned(),
                pipeline,
            };
            state.games.insert(pid, game);
            Some(format!("helper: game process {pid} uses {source}"))
        }
        Err(reason) => Some(format!(
            "helper: profile for process {pid} is unusable: {reason}"
        )),
    }
}

fn serve_helper(shared: &Shared, mut stream: TcpStream) -> io::Result<()> {
    let hello = read_hello(&mut stream)?;
    let conn = shared.conn(&stream)?;
    let offset = i64::try_from(hello.counter)
        .unwrap_or(i64::MAX)
        .saturating_sub(monotonic_micros());
    let chosen = shared
        .config
        .monitor_index
        .and_then(|index| hello.monitors.get(index))
        .or_else(|| hello.monitors.iter().find(|(primary, _)| *primary))
        .or_else(|| hello.monitors.first())
        .map(|(_, name)| name.clone());
    {
        let mut state = shared.lock();
        state.clock_offset = Some(offset);
        if let Some(name) = chosen {
            if name != state.monitor_id {
                shared.log(&format!("helper: display id for the game is {name}"));
                state.monitor_id = name;
            }
        }
        if let Some(note) = register_game(shared, &mut state, hello.pid, &hello.executable) {
            shared.log(&note);
        }
    }

    if hello.channel == sesp::CHANNEL_SUBSCRIPTION {
        shared
            .lock()
            .pose_sinks
            .insert(hello.pid, Arc::clone(&conn));
        // Nothing is expected on this channel; read until the game closes it.
        let mut sink = [0u8; 256];
        while stream.read(&mut sink)? != 0 {}
        shared.forget(conn.id);
        return Ok(());
    }

    shared.log(&format!(
        "helper: game process {} connected (slot {})",
        hello.pid, hello.slot
    ));
    let mut pending = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Ok(());
        }
        pending.extend_from_slice(&chunk[..read]);
        while let Some((buffer, rest)) = sesp::unframe(&pending).map_err(invalid)? {
            let request = sesp::parse(buffer).map_err(invalid)?;
            let monitor_id = shared.lock().monitor_id.clone();
            let reply = sesp::respond(&request, &CAPABILITIES, &monitor_id);
            let (kind, used) = (request.kind, pending.len() - rest.len());
            pending.drain(..used);
            conn.send(&sesp::frame(&reply))?;
            match kind {
                sesp::T_HEAD_POSE_START => {
                    if shared.lock().posing.insert(hello.pid) {
                        shared.log(&format!(
                            "helper: game process {} asked for head pose",
                            hello.pid
                        ));
                    }
                }
                sesp::T_HEAD_POSE_STOP => {
                    shared.lock().posing.remove(&hello.pid);
                }
                _ => {}
            }
        }
    }
}

// --- the gaze overlay -------------------------------------------------------

/// How often the keeper looks at the overlay program.
const OVERLAY_CHECK_EVERY: Duration = Duration::from_millis(500);
/// An overlay that exits sooner than this after starting counts as a
/// failed start.
const OVERLAY_QUICK_EXIT: Duration = Duration::from_secs(5);
/// After this many failed starts in a row the keeper stops trying until the
/// settings change.
const OVERLAY_MAX_FAILURES: u32 = 3;

/// Keep the overlay program running while the overlay is switched on, and
/// stopped while it is off. The program is this same executable.
fn keep_overlay(shared: Arc<Shared>) {
    let mut child: Option<(std::process::Child, Instant)> = None;
    let mut failures = 0u32;
    loop {
        thread::sleep(OVERLAY_CHECK_EVERY);
        let wanted = shared.lock().tuning.overlay.enabled;
        if shared.overlay_retry.swap(false, Ordering::Relaxed) {
            failures = 0;
        }
        if let Some((process, since)) = &mut child {
            match process.try_wait() {
                Ok(None) if wanted => {}
                Ok(None) => {
                    // Switched off: it exits by itself when told, so only
                    // one that has not gone after a moment is killed.
                    thread::sleep(Duration::from_secs(1));
                    if matches!(process.try_wait(), Ok(None)) {
                        let _ = process.kill();
                        let _ = process.wait();
                    }
                    child = None;
                    shared.log("overlay: stopped");
                }
                Ok(Some(status)) => {
                    if wanted && since.elapsed() < OVERLAY_QUICK_EXIT {
                        failures += 1;
                        shared.log(&format!(
                            "overlay: exited at once ({status}), try {failures}"
                        ));
                        if failures == OVERLAY_MAX_FAILURES {
                            shared.log("overlay: giving up until its settings change");
                        }
                    } else if wanted {
                        shared.log(&format!("overlay: exited ({status}); starting it again"));
                    }
                    child = None;
                }
                Err(_) => child = None,
            }
        } else if wanted && failures < OVERLAY_MAX_FAILURES {
            let started = std::env::current_exe().and_then(|exe| {
                std::process::Command::new(exe)
                    .arg("overlay")
                    .stdin(std::process::Stdio::null())
                    .spawn()
            });
            match started {
                Ok(process) => {
                    shared.log("overlay: started");
                    child = Some((process, Instant::now()));
                }
                Err(err) => {
                    failures += 1;
                    shared.log(&format!("overlay: cannot start: {err}"));
                }
            }
        }
    }
}

// --- the driver ------------------------------------------------------------

fn follow_driver(shared: Arc<Shared>) {
    let path = shared.config.tobiid_socket.clone();
    let mut reported = false;
    loop {
        match UnixStream::connect(&path) {
            Ok(stream) => {
                reported = false;
                shared.log(&format!("tobiid: connected to {}", path.display()));
                shared.lock().driver_connected = true;
                let ended = driver_loop(&shared, stream);
                power::disconnected(&shared);
                shared.lock().driver_connected = false;
                if let Err(err) = ended {
                    shared.log(&format!("tobiid: connection ended: {err}"));
                }
            }
            Err(err) if !reported => {
                shared.log(&format!(
                    "tobiid: cannot connect to {} ({err}); retrying",
                    path.display()
                ));
                reported = true;
            }
            Err(_) => {}
        }
        thread::sleep(RECONNECT_EVERY);
    }
}

fn driver_loop(shared: &Shared, mut stream: UnixStream) -> io::Result<()> {
    power::connected(shared, &stream)?;
    stream.write_all(&driver::frame(&driver::subscribe(DRIVER_STREAMS)))?;
    stream.write_all(&driver::frame(&driver::request(
        AREA_REQUEST_ID,
        driver::KIND_DISPLAY_AREA_GET,
        &[],
    )))?;
    let mut pending = Vec::new();
    let mut chunk = [0u8; 65536];
    let mut seen_gaze = false;
    let mut seen_pose = false;
    loop {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "tobiid closed the connection",
            ));
        }
        pending.extend_from_slice(&chunk[..read]);
        let mut used = 0;
        while let Some((body, rest)) = driver::unframe(&pending[used..])
            .map_err(|e| invalid(format!("frame of {} bytes", e.0)))?
        {
            used = pending.len() - rest.len();
            match tobiid::decode(body) {
                Some(ServerMsg::GazeRaw(raw)) => {
                    if !std::mem::replace(&mut seen_gaze, true) {
                        shared.log("tobiid: first gaze sample");
                    }
                    on_gaze(shared, &raw);
                }
                Some(ServerMsg::HeadPose(pose)) => {
                    if !std::mem::replace(&mut seen_pose, true) {
                        shared.log("tobiid: first head pose");
                    }
                    if shared.asleep.load(Ordering::Acquire) {
                        power::pose_while_asleep(shared);
                    }
                    on_pose(shared, &pose);
                }
                Some(ServerMsg::Presence {
                    host_micros,
                    status,
                }) => on_presence(shared, host_micros, status),
                Some(ServerMsg::Subscribed { ok }) => {
                    shared.log(&format!("tobiid: subscribed ok={ok}"))
                }
                Some(ServerMsg::Reply {
                    request_id: AREA_REQUEST_ID,
                    status,
                    payload,
                }) => on_display_area(shared, status, &payload),
                Some(ServerMsg::Notification) => {
                    // A calibration may have changed which screen the tracker
                    // is on or how big it is: ask again.
                    let again =
                        driver::request(AREA_REQUEST_ID, driver::KIND_DISPLAY_AREA_GET, &[]);
                    stream.write_all(&driver::frame(&again))?;
                }
                Some(ServerMsg::Reply {
                    request_id: request_id @ (power::PAUSE_REQUEST_ID | power::RESUME_REQUEST_ID),
                    status,
                    ..
                }) => {
                    if status != driver::STATUS_OK {
                        power::refused(shared, request_id, status);
                    }
                }
                Some(ServerMsg::Reply { .. }) | None => {}
            }
        }
        pending.drain(..used);
    }
}

fn on_gaze(shared: &Shared, raw: &GazeRaw) {
    let (sinks, display, offset, watchers) = {
        let state = shared.lock();
        (
            state.gaze_sinks.clone(),
            state.display,
            state.clock_offset,
            state.watchers.clone(),
        )
    };
    if !watchers.is_empty() {
        let [x, y] = raw.combined_on_display;
        control::send_gaze(
            shared,
            &watchers,
            GazePoint {
                micros: raw.host_micros.unwrap_or_else(monotonic_micros),
                x,
                y,
                valid: raw.combined_validity == 1 && x.is_finite() && y.is_finite(),
            },
        );
    }
    if sinks.is_empty() {
        return;
    }
    let micros = shared.game_micros(raw.host_micros, offset);
    let payload = frames::gaze_frame(&shared.gaze_template, raw, micros, display.as_ref());
    if shared.broadcast(&sinks, &Message::stream(STREAM_GAZE, payload).encode()) > 0 {
        shared.stats.gaze_frames.fetch_add(1, Ordering::Relaxed);
    }
}

fn on_presence(shared: &Shared, host_micros: i64, status: u8) {
    let (sinks, offset, changed) = {
        let mut state = shared.lock();
        let changed = state.presence != status;
        state.presence = status;
        (state.presence_sinks.clone(), state.clock_offset, changed)
    };
    if changed {
        shared.log(&format!("tobiid: presence {status}"));
    }
    if status == driver::PRESENCE_UNKNOWN || sinks.is_empty() {
        return;
    }
    let micros = shared.game_micros(Some(host_micros), offset);
    let payload = frames::presence_frame(&shared.presence_template, status, micros);
    if shared.broadcast(&sinks, &Message::stream(STREAM_PRESENCE, payload).encode()) > 0 {
        shared.stats.presence_frames.fetch_add(1, Ordering::Relaxed);
    }
}

fn on_pose(shared: &Shared, pose: &HeadPose) {
    let (watchers, tap) = {
        let state = shared.lock();
        (state.watchers.clone(), state.confirm_tap.clone())
    };
    if !watchers.is_empty() || tap.is_some() {
        let raw = RawPose {
            micros: pose.host_micros,
            position_mm: pose.position_mm,
            rotation_deg: pose.rotation_rad.map(f32::to_degrees),
            valid: pose.valid(),
        };
        if let Some(tap) = tap {
            // The prompt may have just ended; that is not an error.
            let _ = tap.send(raw);
        }
        if !watchers.is_empty() {
            control::send_pose(shared, &watchers, raw);
        }
    }
    if !pose.valid() {
        // The validity fields' width in the services message is not known
        // yet, so an invalid pose is dropped rather than sent as valid.
        shared.stats.poses_dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let input = Pose {
        position_mm: pose.position_mm,
        rotation_rad: pose.rotation_rad,
    };
    let (targets, offset) = {
        let mut state = shared.lock();
        let state = &mut *state;
        state.last_pose = Some(Instant::now());
        state.preview.process(pose.host_micros, input);
        // Every known game's pipeline sees every pose, wanted or not, so its
        // filter is warm when the game asks for head pose.
        let mut targets = Vec::new();
        for (pid, game) in &mut state.games {
            let shaped = game.pipeline.process(pose.host_micros, input);
            if !state.posing.contains(pid) {
                continue;
            }
            if let Some(sink) = state.pose_sinks.get(pid).cloned() {
                let sequence = state.sequences.entry(*pid).or_insert(0);
                *sequence = sequence.wrapping_add(1);
                targets.push((sink, *sequence, shaped));
            }
        }
        (targets, state.clock_offset)
    };
    let micros = shared.game_micros(Some(pose.host_micros), offset);
    for (sink, sequence, shaped) in targets {
        let message = sesp::frame(&sesp::head_pose(
            sequence,
            micros,
            shaped.position_mm,
            shaped.rotation_rad,
        ));
        if sink.send(&message).is_ok() {
            shared.stats.poses_sent.fetch_add(1, Ordering::Relaxed);
        } else {
            sink.close();
            shared.forget(sink.id);
        }
    }
}

fn on_display_area(shared: &Shared, status: u8, payload: &[u8]) {
    let corners = (status == driver::STATUS_OK)
        .then(|| driver::display_area(payload))
        .flatten();
    let Some(corners) = corners else {
        shared.log(&format!(
            "tobiid: no display area (status {status}); the recorded one stays in use"
        ));
        return;
    };
    let corners = corners.map(|corner| corner.map(f64::from));
    let reply = shared
        .replies
        .get(&CMD_DISPLAY_AREA)
        .and_then(|recorded| frames::display_area_reply(recorded, corners));
    let display = DisplayFrame::new(corners[0], corners[1], corners[2]);
    let (Some(reply), Some(display)) = (reply, display) else {
        shared.log("tobiid: display area is not usable; the recorded one stays in use");
        return;
    };
    let width = (corners[1][0] - corners[0][0]).abs();
    {
        let mut state = shared.lock();
        state.area_reply = Some(reply);
        state.display = Some(display);
        state.display_width_mm = Some(width);
    }
    shared.log(&format!("tobiid: display area is {width:.0} mm wide"));
}

fn invalid(reason: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason.to_string())
}
