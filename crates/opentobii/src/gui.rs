//! The tuning window: `opentobii gui`.
//!
//! The window is a client of the daemon's control socket. One thread
//! follows the daemon's feed of head poses and status, another sends the
//! changes made in the window. The window keeps its own copy of the
//! settings and runs its own pipeline on the raw poses, so what it draws as
//! "sent to the game" is the profile being edited, whether or not that game
//! is running.

use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::games::{self, Source};
use crate::{install, prefix, service, update};
use opentobii_core::camera::Camera;
use opentobii_core::config::Config;
use opentobii_core::control::{self, Line, RawPose, Request, Status};
use opentobii_core::overlay::{self, Overlay, Style};
use opentobii_core::pipeline::{Axis, Neutral, Pipeline, Pose, Profile};
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};

/// How often an open window looks for a newer release.
const CHECK_EVERY: Duration = Duration::from_secs(60 * 60);
/// How soon it looks again after a look that failed.
const CHECK_RETRY: Duration = Duration::from_secs(5 * 60);

slint::include_modules!();

const RETRY_EVERY: Duration = Duration::from_secs(2);
const READ_TIMEOUT: Duration = Duration::from_secs(3);
/// No pose for this long and the live view goes dark.
const POSE_TIMEOUT: Duration = Duration::from_millis(600);
/// Changes are sent at most this often while a slider is dragged.
const SEND_EVERY: Duration = Duration::from_millis(80);

/// Half the width of the live view, degrees of yaw; and half its height,
/// degrees of pitch.
const VIEW_YAW: f64 = 45.0;
const VIEW_PITCH: f64 = 30.0;

const ROTATION_NAMES: [&str; 3] = ["Pitch", "Yaw", "Roll"];
const POSITION_NAMES: [&str; 3] = ["Side", "Up", "Depth"];
/// The colours offered for the gaze mark.
const OVERLAY_COLORS: [&str; 8] = [
    "#60cdff", "#3ecfb2", "#7bd389", "#ffd166", "#ff8000", "#ef476f", "#b388ff", "#ffffff",
];

const DEFAULT_PROFILE_NAME: &str = "Default (every game)";

/// What the background threads tell the window.
enum Event {
    Connected(Box<Config>, Status),
    Disconnected,
    Pose(RawPose),
    Status(Status),
    Recentred(Option<String>, Neutral),
    Failed(String),
    /// What the driver's calibration tool reports, or `None` when the tool
    /// is not installed.
    Calibration(Option<Calibration>),
    /// The calibration tool finished, with what to tell the user.
    Calibrated(String),
    /// A look for a newer release finished: the release if there is one, or
    /// why the look failed.
    UpdateChecked(Result<Option<Box<update::Release>>, String>),
    /// An update finished; what to tell the user, or why it failed.
    UpdateDone(Result<String, String>),
    /// Installing for this user finished.
    Installed(Result<(), String>),
    /// Whether the driver's daemon program is on this system.
    Driver(bool),
    /// What systemd says about the daemon.
    Service(service::State),
    /// A start, stop or autostart change finished; an error if it failed.
    ServiceDone(Result<(), String>),
    /// The Steam games that can use the tracker, and their helpers.
    Games(Vec<GameInfo>),
    /// The folder picker closed: `None` when nothing was picked, else
    /// whether the folder could be added.
    GameAdded(Option<Result<(), String>>),
    /// A set-up or removal finished for the game with this app id.
    GameDone(String, Result<(), String>),
}

/// What the window asks the daemon to do.
enum Command {
    Set(Option<String>, Box<Profile>),
    Delete(String),
    Recentre(Option<String>),
    SetOverlay(Box<Overlay>),
    SetCamera(Camera),
}

// --- the driver's calibration tool -------------------------------------------

/// The driver's own calibration program. It sets up which monitor the
/// tracker is on and how big it is, then runs the eye calibration.
const CALIBRATE: &str = "tobii-calibrate";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Monitor {
    /// What `--monitor` takes: the connector name, such as `DP-4`.
    name: String,
    /// What the list shows: name and size.
    label: String,
    /// The tool would calibrate here by default.
    current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Calibration {
    monitors: Vec<Monitor>,
    /// The calibration and display area lines of `--status`.
    summary: String,
}

/// Read `tobii-calibrate --list-monitors`: lines like
/// `2: DP-4 (2560x1440, 590 x 333 mm)  <- calibrates here`.
fn parse_monitors(text: &str) -> Vec<Monitor> {
    text.lines()
        .filter_map(|line| {
            let (index, rest) = line.split_once(':')?;
            index.trim().parse::<u32>().ok()?;
            let current = rest.contains("<-");
            let label = rest.split("<-").next()?.trim().to_owned();
            let name = label.split_whitespace().next()?.to_owned();
            Some(Monitor {
                name,
                label,
                current,
            })
        })
        .collect()
}

/// The two lines of `tobii-calibrate --status` worth showing, with the
/// column padding and the file paths taken out.
fn parse_status(text: &str) -> String {
    let lines: Vec<String> = text
        .lines()
        .filter(|line| line.starts_with("calibration") || line.starts_with("display area"))
        .map(|line| {
            let line = line.split(" (~").next().unwrap_or(line);
            let (label, value) = line.split_at(line.find("  ").unwrap_or(line.len()));
            let mut label = label.to_owned();
            if let Some(first) = label.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            format!("{label}: {}", value.trim())
        })
        .collect();
    lines.join("\n")
}

fn tool_output(argument: &str) -> Option<String> {
    let output = std::process::Command::new(CALIBRATE)
        .arg(argument)
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Ask the tool what it knows. `None` when it is not installed.
fn read_calibration() -> Option<Calibration> {
    let monitors = parse_monitors(&tool_output("--list-monitors")?);
    let summary = tool_output("--status")
        .map(|text| parse_status(&text))
        .unwrap_or_default();
    Some(Calibration { monitors, summary })
}

/// What to say when the tool exits, from its documented exit statuses.
fn calibration_result(code: Option<i32>) -> String {
    match code {
        Some(0) => "Calibration saved.".to_owned(),
        Some(2) => "Calibration saved. The tracker takes it at its next start.".to_owned(),
        _ => "Calibration was not completed.".to_owned(),
    }
}

/// Run the calibration tool to the end, then report and re-read its state.
fn run_calibration(monitor: Option<String>, keep_display_setup: bool, events: &Sender<Event>) {
    let mut command = std::process::Command::new(CALIBRATE);
    if let Some(monitor) = monitor {
        command.arg("--monitor").arg(monitor);
    }
    if keep_display_setup {
        command.arg("--no-display-setup");
    }
    let message = match command.status() {
        Ok(status) => calibration_result(status.code()),
        Err(err) => format!("Could not start {CALIBRATE}: {err}"),
    };
    let _ = events.send(Event::Calibrated(message));
    let _ = events.send(Event::Calibration(read_calibration()));
}

// --- getting started -----------------------------------------------------------

/// What the checklist is worked out from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Readiness {
    driver_installed: bool,
    calibrated: bool,
    installed: bool,
    daemon_running: bool,
    can_start: bool,
    game_set_up: bool,
    any_game: bool,
    installing: bool,
    starting: bool,
}

const CHECK_INSTALL: usize = 2;
const CHECK_DAEMON: usize = 3;

/// The steps from nothing to a working setup, in the order they have to
/// happen. Each is done or not, and says what to do when it is not.
fn checklist(ready: Readiness) -> Vec<CheckRow> {
    let row = |text: &str, detail: &str, done: bool, action: &str, busy: bool| CheckRow {
        text: text.into(),
        detail: detail.into(),
        done,
        action: action.into(),
        busy,
    };
    vec![
        row(
            "The tracker's driver is installed",
            "opentobii needs the tobii-linux driver from github.com/Skyggedans/tobii-linux. Install it, then plug the tracker in again.",
            ready.driver_installed,
            "",
            false,
        ),
        row(
            "The tracker is calibrated to your screen and eyes",
            "Use the Calibration card below.",
            ready.calibrated,
            "",
            false,
        ),
        row(
            "opentobii is installed for your user",
            "Copies this program to ~/.local/bin and adds it to your app menu. No root needed. An installed copy can update itself.",
            ready.installed,
            "Install",
            ready.installing,
        ),
        row(
            "The daemon is running",
            if ready.can_start { "" } else { "Start it in a terminal with: opentobii daemon" },
            ready.daemon_running,
            if ready.can_start { "Start" } else { "" },
            ready.starting,
        ),
        row(
            "A game is set up",
            if ready.any_game {
                "Use the Games card below, with the game closed. Then switch eye tracking on in the game's options."
            } else {
                "No installed Steam game with Tobii support was found yet."
            },
            ready.game_set_up,
            "",
            false,
        ),
    ]
}

fn home_copy_exists() -> bool {
    install::Layout::for_user().is_ok_and(|layout| layout.binary.is_file())
}

/// What `opentobii setup` does, for the Install button.
fn install_for_user() -> Result<(), String> {
    let layout = install::Layout::for_user()?;
    let exe = std::env::current_exe()
        .map_err(|err| format!("cannot find this program's own path: {err}"))?;
    install::place(&exe, &layout)?;
    if service::state().systemd {
        // The unit file is new or changed: have systemd read it.
        let _ = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status();
    }
    Ok(())
}

fn driver_present() -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|folder| folder.join("tobiid").is_file())
    })
}

// --- games --------------------------------------------------------------------

/// One game that can use the tracker, and the helper in its prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GameInfo {
    /// A Steam app id, else the prefix's path.
    id: String,
    name: String,
    source: Source,
    prefix: std::path::PathBuf,
    /// `None` when the game has never been run, so it has no prefix yet.
    helper: Option<Result<prefix::Installed, String>>,
}

fn read_games() -> Vec<GameInfo> {
    let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) else {
        return Vec::new();
    };
    let added = games::added_path()
        .map(|file| games::load_added(&file))
        .unwrap_or_default();
    games::all(std::path::Path::new(&home), &added)
        .into_iter()
        .map(|game| GameInfo {
            helper: game
                .prefix_exists
                .then(|| prefix::status(&game.prefix).map_err(|err| err.to_string())),
            id: game.id,
            name: game.name,
            source: game.source,
            prefix: game.prefix,
        })
        .collect()
}

/// How a game shows in the list: its state in words, the main button's
/// label (empty for none), and whether Remove is offered.
fn game_row(game: &GameInfo, note: Option<&str>, busy: bool) -> GameRow {
    let (state, action, removable) = match &game.helper {
        None => (
            "Run the game once, close it, then set it up here.".to_owned(),
            "",
            false,
        ),
        Some(Err(reason)) => (reason.clone(), "", false),
        Some(Ok(found)) if found.service_key && found.helper_current => {
            ("Set up.".to_owned(), "", true)
        }
        Some(Ok(found)) if found.service_key && found.helper => {
            ("Set up with an older helper.".to_owned(), "Update", true)
        }
        Some(Ok(found)) if found.service_key || found.helper => {
            ("Partly set up.".to_owned(), "Repair", true)
        }
        Some(Ok(_)) => ("Not set up.".to_owned(), "Set up", false),
    };
    let running = matches!(&game.helper, Some(Ok(found)) if found.in_use);
    let mut state = state;
    if running && (!action.is_empty() || removable) {
        state.push_str(" Close the game to change this.");
    }
    if let Some(note) = note {
        state = note.to_owned();
    }
    GameRow {
        name: game.name.as_str().into(),
        // A Steam game is known by its name; any other by where it is.
        place: match game.source {
            Source::Steam => "".into(),
            Source::Found | Source::Added => short_path(&game.prefix).into(),
        },
        state: state.into(),
        action: if running { "".into() } else { action.into() },
        removable: removable && !running,
        // An added folder can be dropped from the list once nothing of
        // ours is left in it.
        forgettable: game.source == Source::Added && !removable && !busy,
        busy,
    }
}

/// A path as a person writes it: `~` for the home folder.
fn short_path(path: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    match home.and_then(|home| path.strip_prefix(home).ok().map(std::path::Path::to_owned)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

/// What the Daemon card says.
fn service_summary(service: service::State, connected: bool) -> String {
    if !service.systemd {
        return "This system has no systemd for your user, so the daemon cannot be managed from here. \
                Start it with: opentobii daemon"
            .to_owned();
    }
    match (connected, service.active) {
        (true, true) => "Running.".to_owned(),
        (true, false) => "Running, but started by hand and not by this window. \
                          Stop it where it was started."
            .to_owned(),
        (false, true) => "Starting…".to_owned(),
        (false, false) => "Not running.".to_owned(),
    }
}

/// The first few non-empty lines of some release notes, for the banner.
fn first_lines(notes: &str, count: usize) -> String {
    // Release notes are written in Markdown; the banner shows plain text.
    let lines: Vec<String> = notes
        .lines()
        .map(|line| {
            let line = line.trim().trim_start_matches('#').trim_start();
            let line = match line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
                Some(item) => format!("• {item}"),
                None => line.to_owned(),
            };
            line.replace("**", "").replace('`', "")
        })
        .filter(|line| !line.is_empty())
        .collect();
    let mut shown = lines
        .iter()
        .take(count)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > count {
        shown.push_str("\n…");
    }
    shown
}

// --- pure helpers, tested without a window ---------------------------------

/// Steadiness slider (0 to 100) to the filter's cutoff at rest, Hz: 3 Hz at
/// 0, 0.1 Hz at 100, evenly spaced by ratio.
fn cutoff_for(steadiness: f64) -> f64 {
    3.0 * (0.1f64 / 3.0).powf(steadiness.clamp(0.0, 100.0) / 100.0)
}

fn steadiness_for(cutoff: f64) -> f64 {
    ((cutoff.clamp(0.1, 3.0) / 3.0).ln() / (0.1f64 / 3.0).ln() * 100.0).clamp(0.0, 100.0)
}

/// "Fast turns" slider (0 to 100) to the rotation filter's response.
fn response_for(slider: f64) -> f64 {
    slider.clamp(0.0, 100.0) / 100.0 * 0.5
}

fn slider_for_response(response: f64) -> f64 {
    (response / 0.5 * 100.0).clamp(0.0, 100.0)
}

fn snap(value: f64, step: f64) -> f64 {
    (value / step).round() * step
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Group {
    Rotation,
    Position,
}

impl Group {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "rotation" => Some(Self::Rotation),
            "position" => Some(Self::Position),
            _ => None,
        }
    }

    fn unit(self) -> &'static str {
        match self {
            Self::Rotation => "°",
            Self::Position => " mm",
        }
    }

    fn dead_zone_max(self) -> f64 {
        match self {
            Self::Rotation => 10.0,
            Self::Position => 50.0,
        }
    }

    fn limit_max(self) -> f64 {
        match self {
            Self::Rotation => 180.0,
            Self::Position => 500.0,
        }
    }
}

fn axis_mut(profile: &mut Profile, group: Group, index: usize) -> Option<&mut Axis> {
    match (group, index) {
        (Group::Rotation, 0) => Some(&mut profile.pitch),
        (Group::Rotation, 1) => Some(&mut profile.yaw),
        (Group::Rotation, 2) => Some(&mut profile.roll),
        (Group::Position, 0) => Some(&mut profile.x),
        (Group::Position, 1) => Some(&mut profile.y),
        (Group::Position, 2) => Some(&mut profile.z),
        _ => None,
    }
}

fn axes(profile: &Profile, group: Group) -> [Axis; 3] {
    match group {
        Group::Rotation => [profile.pitch, profile.yaw, profile.roll],
        Group::Position => [profile.x, profile.y, profile.z],
    }
}

/// Put a slider's value into one field of an axis, snapped to a sensible
/// step. Returns false for a field this window does not know.
fn set_field(axis: &mut Axis, group: Group, field: &str, value: f64) -> bool {
    let fine = if group == Group::Rotation { 0.1 } else { 1.0 };
    match field {
        "dead_zone" => axis.dead_zone = snap(value, fine).clamp(0.0, group.dead_zone_max()),
        "sensitivity" => axis.sensitivity = snap(value, 0.05).clamp(0.0, 5.0),
        "curve" => axis.curve = snap(value, 0.05).clamp(0.5, 3.0),
        "limit" => {
            let limit = snap(value, 1.0).clamp(0.0, group.limit_max());
            // The bottom of the slider is "no limit".
            axis.limit = if limit < 1.0 { 0.0 } else { limit };
        }
        _ => return false,
    }
    true
}

fn amount(value: f64, group: Group) -> String {
    match group {
        Group::Rotation => format!("{value:.1}°"),
        Group::Position => format!("{value:.0} mm"),
    }
}

fn signed(value: f64, group: Group) -> String {
    match group {
        Group::Rotation => format!("{value:+.1}°"),
        Group::Position => format!("{value:+.0} mm"),
    }
}

fn row(name: &str, axis: &Axis, group: Group, raw: Option<f64>, out: Option<f64>) -> AxisRow {
    let reading = |value: Option<f64>| value.map_or_else(|| "–".to_owned(), |v| signed(v, group));
    AxisRow {
        name: name.into(),
        dead_zone: axis.dead_zone as f32,
        dead_zone_max: group.dead_zone_max() as f32,
        dead_zone_text: amount(axis.dead_zone, group).into(),
        sensitivity: axis.sensitivity as f32,
        sensitivity_text: format!("{:.2}×", axis.sensitivity).into(),
        curve: axis.curve as f32,
        curve_text: format!("{:.2}", axis.curve).into(),
        limit: axis.limit as f32,
        limit_max: group.limit_max() as f32,
        limit_text: if axis.limit > 0.0 {
            format!("{:.0}{}", axis.limit, group.unit()).into()
        } else {
            "Off".into()
        },
        invert: axis.invert,
        raw_text: reading(raw).into(),
        out_text: reading(out).into(),
    }
}

/// The name a profile goes by in the list.
fn profile_names(config: &Config) -> Vec<String> {
    std::iter::once(DEFAULT_PROFILE_NAME.to_owned())
        .chain(config.games.keys().cloned())
        .collect()
}

/// A connected game that has no profile of its own yet.
fn game_without_profile(config: &Config, status: &Status) -> Option<String> {
    status
        .games
        .iter()
        .map(|game| game.game.clone())
        .find(|key| !key.is_empty() && !config.games.contains_key(key))
}

// --- the window's state -----------------------------------------------------

struct Ui {
    app: App,
    commands: Sender<Command>,
    config: Config,
    /// The game whose profile is shown; `None` is the default profile.
    selected: Option<String>,
    status: Status,
    connected: bool,
    pipeline: Pipeline,
    last_pose: Option<(Instant, RawPose, Pose)>,
    failure: Option<String>,
    calibration: Option<Calibration>,
    calibration_note: String,
    driver_installed: bool,
    home_installed: bool,
    installing: bool,
    check_rows: Rc<VecModel<CheckRow>>,
    update: Option<update::Release>,
    update_busy: bool,
    update_result: Option<String>,
    /// How the last look for a release went, for the About box.
    update_status: String,
    update_checking: bool,
    /// Asks the checking thread to look now.
    check_now: Sender<()>,
    service: service::State,
    service_busy: bool,
    games: Vec<GameInfo>,
    /// The folder picker is open.
    adding_game: bool,
    /// App ids with a set-up or removal under way.
    games_busy: Vec<String>,
    /// The last result for a game, by app id, shown in place of its state.
    game_notes: Vec<(String, String)>,
    game_rows: Rc<VecModel<GameRow>>,
    events: Sender<Event>,
    /// The newest unsent change, and when the last one went out.
    pending: Option<(Option<String>, Profile)>,
    /// The newest unsent overlay settings.
    pending_overlay: Option<Overlay>,
    last_sent: Instant,
    last_overlay_sent: Instant,
    rotation: Rc<VecModel<AxisRow>>,
    position: Rc<VecModel<AxisRow>>,
}

impl Ui {
    fn profile(&self) -> Profile {
        match &self.selected {
            Some(game) => self
                .config
                .games
                .get(game)
                .copied()
                .unwrap_or(self.config.default),
            None => self.config.default,
        }
    }

    /// Store an edited profile, show it, and queue it for the daemon.
    fn commit(&mut self, profile: Profile) {
        if let Err(reason) = profile.validate() {
            self.failure = Some(reason);
            self.refresh_chrome();
            return;
        }
        match &self.selected {
            Some(game) => {
                self.config.games.insert(game.clone(), profile);
            }
            None => self.config.default = profile,
        }
        let _ = self.pipeline.set_profile(profile);
        self.pending = Some((self.selected.clone(), profile));
        self.refresh_settings();
    }

    fn flush(&mut self, force: bool) {
        if self.pending.is_some() && (force || self.last_sent.elapsed() >= SEND_EVERY) {
            if let Some((game, profile)) = self.pending.take() {
                let _ = self.commands.send(Command::Set(game, Box::new(profile)));
                self.last_sent = Instant::now();
            }
        }
        if self.pending_overlay.is_some()
            && (force || self.last_overlay_sent.elapsed() >= SEND_EVERY)
        {
            if let Some(overlay) = self.pending_overlay.take() {
                let _ = self.commands.send(Command::SetOverlay(Box::new(overlay)));
                self.last_overlay_sent = Instant::now();
            }
        }
    }

    /// Store edited overlay settings, show them, and queue them for the
    /// daemon.
    fn commit_overlay(&mut self, overlay: Overlay) {
        if let Err(reason) = overlay.validate() {
            self.failure = Some(reason);
            self.refresh_chrome();
            return;
        }
        self.config.overlay = overlay.clone();
        self.pending_overlay = Some(overlay);
        self.refresh_overlay();
    }

    /// Switch the face login camera on or off.
    fn set_camera(&mut self, on: bool) {
        self.config.camera.enabled = on;
        let _ = self
            .commands
            .send(Command::SetCamera(self.config.camera.clone()));
        self.refresh_camera();
    }

    /// The face login camera card.
    fn refresh_camera(&self) {
        let app = &self.app;
        let camera = &self.config.camera;
        app.set_camera_enabled(camera.enabled);
        let status = if !self.connected {
            "The daemon is not running."
        } else if self.status.camera.is_empty() {
            "This daemon is older than the camera: restart it."
        } else {
            self.status.camera.as_str()
        };
        app.set_camera_status(status.into());
        let nod = match crate::login::installed() {
            crate::login::Installed::No => "Nod to approve: the login helper is not installed. \
                 Run `opentobii login-helper` in a terminal for how."
                .to_owned(),
            other => format!(
                "Nod to approve: login helper {}. `opentobii login-helper` shows the PAM lines.",
                crate::login::describe(&other)
            ),
        };
        app.set_nod_status(nod.into());
    }

    /// The overlay card, from the settings.
    fn refresh_overlay(&self) {
        let app = &self.app;
        let overlay = &self.config.overlay;
        app.set_overlay_enabled(overlay.enabled);
        let style = Style::ALL
            .iter()
            .position(|style| *style == overlay.style)
            .unwrap_or(0);
        app.set_overlay_style_index(i32::try_from(style).unwrap_or(0));
        let color = OVERLAY_COLORS
            .iter()
            .position(|hex| hex.eq_ignore_ascii_case(&overlay.color));
        app.set_overlay_color_index(color.and_then(|at| i32::try_from(at).ok()).unwrap_or(-1));
        app.set_overlay_size(overlay.size as f32);
        app.set_overlay_size_text(format!("{:.0} px", overlay.size).into());
        app.set_overlay_opacity((overlay.opacity * 100.0) as f32);
        app.set_overlay_opacity_text(format!("{:.0}%", overlay.opacity * 100.0).into());
        app.set_overlay_smoothing(overlay.smoothing as f32);
        app.set_overlay_smoothing_text(format!("{:.0}", overlay.smoothing).into());
        app.set_overlay_trail(overlay.trail as f32);
        let trail = if overlay.trail > 0.0 {
            format!("{:.0} ms", overlay::trail_seconds(overlay.trail) * 1000.0)
        } else {
            "Off".to_owned()
        };
        app.set_overlay_trail_text(trail.into());
    }

    fn select(&mut self, selected: Option<String>) {
        self.flush(true);
        self.selected = selected.filter(|game| self.config.games.contains_key(game));
        let _ = self.pipeline.set_profile(self.profile());
        self.refresh_all();
    }

    fn refresh_all(&mut self) {
        self.refresh_chrome();
        self.refresh_checklist();
        self.refresh_service();
        self.refresh_overlay();
        self.refresh_camera();
        self.refresh_settings();
        self.refresh_live();
    }

    /// Status pills, the notice line and the profile list.
    /// The calibration card. `reselect` puts the monitor list back on the
    /// one the tool would use.
    fn refresh_calibration(&self, reselect: bool) {
        let app = &self.app;
        let Some(calibration) = &self.calibration else {
            app.set_calibration_available(false);
            app.set_calibration_text(
                format!("The driver's calibration tool ({CALIBRATE}) was not found.").into(),
            );
            return;
        };
        app.set_calibration_available(!calibration.monitors.is_empty());
        let mut text = calibration.summary.clone();
        if !self.calibration_note.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&self.calibration_note);
        }
        if calibration.monitors.is_empty() {
            text = "The calibration tool found no monitor.".to_owned();
        }
        app.set_calibration_text(text.into());
        if reselect {
            let labels: Vec<SharedString> = calibration
                .monitors
                .iter()
                .map(|monitor| monitor.label.as_str().into())
                .collect();
            let current = calibration
                .monitors
                .iter()
                .position(|monitor| monitor.current);
            app.set_monitors(ModelRc::new(VecModel::from(labels)));
            app.set_monitor_index(i32::try_from(current.unwrap_or(0)).unwrap_or(0));
        }
    }

    fn calibrate(&mut self) {
        let Some(calibration) = &self.calibration else {
            return;
        };
        let monitor = usize::try_from(self.app.get_monitor_index())
            .ok()
            .and_then(|index| calibration.monitors.get(index))
            .map(|monitor| monitor.name.clone());
        let keep = self.app.get_keep_display_setup();
        self.calibration_note = "The calibration screen is open on that monitor.".to_owned();
        self.app.set_calibrating(true);
        self.refresh_calibration(false);
        let events = self.events.clone();
        thread::spawn(move || run_calibration(monitor, keep, &events));
    }

    /// The Getting started card. It goes away once everything is done.
    fn refresh_checklist(&self) {
        let ready = Readiness {
            driver_installed: self.driver_installed,
            calibrated: self
                .calibration
                .as_ref()
                .is_some_and(|c| c.summary.contains("Calibration: yours")),
            installed: self.home_installed
                || matches!(
                    install::kind(),
                    Ok(install::Kind::Home | install::Kind::Packaged)
                ),
            daemon_running: self.connected,
            can_start: self.service.systemd,
            game_set_up: self.games.iter().any(
                |game| matches!(&game.helper, Some(Ok(found)) if found.service_key && found.helper),
            ),
            any_game: !self.games.is_empty(),
            installing: self.installing,
            starting: self.service_busy,
        };
        let rows = checklist(ready);
        self.app
            .set_checklist_visible(rows.iter().any(|row| !row.done));
        if self.check_rows.row_count() == rows.len() {
            for (index, row) in rows.into_iter().enumerate() {
                if self.check_rows.row_data(index).as_ref() != Some(&row) {
                    self.check_rows.set_row_data(index, row);
                }
            }
        } else {
            self.check_rows.set_vec(rows);
        }
    }

    fn checklist_action(&mut self, index: usize) {
        match index {
            CHECK_DAEMON => self.change_service(service::start),
            CHECK_INSTALL if !self.installing => {
                self.installing = true;
                self.refresh_checklist();
                let events = self.events.clone();
                thread::spawn(move || {
                    let _ = events.send(Event::Installed(install_for_user()));
                });
            }
            _ => {}
        }
    }

    /// The update banner.
    fn refresh_update(&self) {
        let app = &self.app;
        app.set_update_busy(self.update_busy);
        app.set_update_checking(self.update_checking);
        app.set_update_status(if self.update_checking {
            "Checking for updates…".into()
        } else {
            self.update_status.as_str().into()
        });
        if let Some(done) = &self.update_result {
            app.set_update_text(done.as_str().into());
            app.set_update_notes(
                "Close this window and open it again to use the new version.".into(),
            );
            app.set_update_possible(false);
            return;
        }
        let Some(release) = &self.update else {
            app.set_update_text("".into());
            app.set_update_notes("".into());
            return;
        };
        app.set_update_text(
            format!(
                "opentobii {} is available. You have {}.",
                release.version,
                update::current()
            )
            .into(),
        );
        // A package-managed or loose copy cannot update itself: say how.
        let refusal = install::kind().ok().and_then(|kind| update::refusal(&kind));
        app.set_update_possible(refusal.is_none());
        let notes = match refusal {
            Some(reason) => format!("This copy does not update itself: {reason}."),
            None => first_lines(&release.notes, 4),
        };
        app.set_update_notes(notes.into());
    }

    /// Look for a newer release now, as asked from the About box.
    fn check_for_updates(&mut self) {
        if self.update_checking || self.check_now.send(()).is_err() {
            return;
        }
        self.update_checking = true;
        self.refresh_update();
    }

    fn update_now(&mut self) {
        let Some(release) = self.update.clone() else {
            return;
        };
        if self.update_busy {
            return;
        }
        self.update_busy = true;
        self.refresh_update();
        let events = self.events.clone();
        thread::spawn(move || {
            let _ = events.send(Event::UpdateDone(update::install_release(
                &update::Http,
                &release,
            )));
        });
    }

    /// The Daemon card.
    fn refresh_service(&self) {
        let app = &self.app;
        app.set_service_available(self.service.systemd);
        app.set_service_active(self.service.active);
        app.set_service_enabled(self.service.enabled);
        app.set_service_busy(self.service_busy);
        app.set_service_text(service_summary(self.service, self.connected).into());
        self.refresh_checklist();
    }

    /// The Games card.
    fn refresh_games(&self) {
        let rows: Vec<GameRow> = self
            .games
            .iter()
            .map(|game| {
                let note = self
                    .game_notes
                    .iter()
                    .find(|(id, _)| *id == game.id)
                    .map(|(_, note)| note.as_str());
                game_row(game, note, self.games_busy.contains(&game.id))
            })
            .collect();
        self.app.set_games_text(
            if rows.is_empty() {
                "No installed game with Tobii support was found. A game in an unusual place can be added below."
            } else {
                ""
            }
            .into(),
        );
        if self.game_rows.row_count() == rows.len() {
            for (index, row) in rows.into_iter().enumerate() {
                if self.game_rows.row_data(index).as_ref() != Some(&row) {
                    self.game_rows.set_row_data(index, row);
                }
            }
        } else {
            self.game_rows.set_vec(rows);
        }
    }

    /// Ask for a folder and add the Wine prefix it belongs to.
    fn add_game(&mut self) {
        if self.adding_game {
            return;
        }
        self.adding_game = true;
        self.app.set_game_adding(true);
        let events = self.events.clone();
        thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new()
                .set_title("Pick the game's Wine prefix: the folder with drive_c in it");
            if let Some(home) = std::env::var_os("HOME") {
                dialog = dialog.set_directory(home);
            }
            // `None` is the user closing the dialog: nothing to say.
            let added = dialog.pick_folder().map(|picked| {
                let file = games::added_path().ok_or("HOME is not set")?;
                games::add(&file, &picked).map(|_| ())
            });
            let _ = events.send(Event::GameAdded(added));
            let _ = events.send(Event::Games(read_games()));
        });
    }

    /// Drop an added folder from the list. The folder is not touched.
    fn forget_game(&mut self, index: usize) {
        let Some(game) = self.games.get(index).cloned() else {
            return;
        };
        if game.source != Source::Added {
            return;
        }
        let events = self.events.clone();
        thread::spawn(move || {
            let result = games::added_path()
                .ok_or_else(|| "HOME is not set".to_owned())
                .and_then(|file| games::forget(&file, &game.prefix));
            if let Err(reason) = result {
                let _ = events.send(Event::Failed(reason));
            }
            let _ = events.send(Event::Games(read_games()));
        });
    }

    /// Set up or remove the helper for the game in row `index`.
    fn change_game(&mut self, index: usize, remove: bool) {
        let Some(game) = self.games.get(index).cloned() else {
            return;
        };
        if self.games_busy.contains(&game.id) {
            return;
        }
        self.games_busy.push(game.id.clone());
        self.game_notes.retain(|(id, _)| *id != game.id);
        self.refresh_games();
        let events = self.events.clone();
        thread::spawn(move || {
            let result = if remove {
                prefix::uninstall(&game.prefix)
            } else {
                prefix::install(&game.prefix, prefix::HELPER).map(|_| ())
            };
            let _ = events.send(Event::GameDone(
                game.id,
                result.map_err(|err| err.to_string()),
            ));
            let _ = events.send(Event::Games(read_games()));
        });
    }

    /// Run one systemd change in the background.
    fn change_service(&mut self, change: fn() -> Result<(), String>) {
        if self.service_busy {
            return;
        }
        self.service_busy = true;
        self.refresh_service();
        let events = self.events.clone();
        thread::spawn(move || {
            let _ = events.send(Event::ServiceDone(change()));
            let _ = events.send(Event::Service(service::state()));
        });
    }

    fn refresh_chrome(&self) {
        let app = &self.app;
        app.set_daemon_connected(self.connected);
        app.set_driver_connected(self.status.driver_connected);
        let tracker = if !self.connected {
            "Unknown"
        } else if !self.status.driver_connected {
            "Driver not running"
        } else if self.status.tracking {
            "Tracking"
        } else if self.status.presence == 1 {
            "Nobody in view"
        } else {
            "Idle"
        };
        app.set_tracker_text(tracker.into());
        let game_text = match self.status.games.as_slice() {
            [] => "None connected".to_owned(),
            [game] if game.game.is_empty() => "Connected".to_owned(),
            [game] => game.game.clone(),
            many => format!("{} connected", many.len()),
        };
        app.set_game_text(game_text.into());
        app.set_game_connected(!self.status.games.is_empty());

        let notice = if !self.connected {
            if self.service.systemd {
                "The opentobii daemon is not running.".to_owned()
            } else {
                "The opentobii daemon is not running. Start it with: opentobii daemon".to_owned()
            }
        } else {
            self.failure.clone().unwrap_or_default()
        };
        app.set_notice(notice.into());

        let names: Vec<SharedString> = profile_names(&self.config)
            .into_iter()
            .map(Into::into)
            .collect();
        let index = self
            .selected
            .as_ref()
            .and_then(|game| self.config.games.keys().position(|key| key == game))
            .map_or(0, |at| at + 1);
        app.set_profiles(ModelRc::new(VecModel::from(names)));
        app.set_profile_index(i32::try_from(index).unwrap_or(0));
        app.set_can_delete(self.selected.is_some());
        let creatable = game_without_profile(&self.config, &self.status);
        app.set_can_create(creatable.is_some());
        app.set_create_text(
            creatable
                .map(|game| format!("Create a profile for {game}"))
                .unwrap_or_default()
                .into(),
        );
    }

    /// Sliders and switches, from the selected profile.
    fn refresh_settings(&self) {
        let profile = self.profile();
        let app = &self.app;
        app.set_smoothing_enabled(profile.smoothing.enabled);
        app.set_steadiness(steadiness_for(profile.smoothing.rotation_steadiness) as f32);
        app.set_steadiness_text(format!("{:.2} Hz", profile.smoothing.rotation_steadiness).into());
        app.set_response(slider_for_response(profile.smoothing.rotation_response) as f32);
        app.set_response_text(format!("{:.2}", profile.smoothing.rotation_response).into());
        app.set_look_dead_x((profile.yaw.dead_zone / VIEW_YAW) as f32);
        app.set_look_dead_y((profile.pitch.dead_zone / VIEW_PITCH) as f32);
        app.set_look_range_text(
            format!("±{VIEW_YAW:.0}° across, ±{VIEW_PITCH:.0}° up and down").into(),
        );
        self.refresh_rows();
    }

    fn refresh_rows(&self) {
        let profile = self.profile();
        let live = self
            .last_pose
            .as_ref()
            .filter(|(at, _, _)| at.elapsed() < POSE_TIMEOUT);
        for (group, names, model) in [
            (Group::Rotation, ROTATION_NAMES, &self.rotation),
            (Group::Position, POSITION_NAMES, &self.position),
        ] {
            for (index, (name, axis)) in names.iter().zip(axes(&profile, group)).enumerate() {
                let (raw, out) = match (live, group) {
                    (Some((_, raw, out)), Group::Rotation) => (
                        Some(f64::from(raw.rotation_deg[index]) - profile.neutral.rotation[index]),
                        Some(f64::from(out.rotation_rad[index]).to_degrees()),
                    ),
                    (Some((_, raw, out)), Group::Position) => {
                        let centre = profile.neutral.position.map_or(0.0, |centre| centre[index]);
                        (
                            Some(f64::from(raw.position_mm[index]) - centre),
                            Some(f64::from(out.position_mm[index]) - centre),
                        )
                    }
                    (None, _) => (None, None),
                };
                let fresh = row(name, &axis, group, raw, out);
                if model.row_data(index).as_ref() != Some(&fresh) {
                    model.set_row_data(index, fresh);
                }
            }
        }
    }

    /// The live view, from the newest pose.
    fn refresh_live(&self) {
        let app = &self.app;
        let live = self
            .last_pose
            .as_ref()
            .filter(|(at, _, _)| at.elapsed() < POSE_TIMEOUT);
        app.set_tracking(live.is_some());
        if let Some((_, raw, out)) = live {
            let neutral = self.profile().neutral.rotation;
            let unit = |value: f64, range: f64| (value / range).clamp(-1.0, 1.0) as f32;
            // Positive yaw is a turn to the user's left; positive pitch lifts
            // the chin; positive roll tilts towards the left shoulder.
            app.set_look_raw_x(unit(
                -(f64::from(raw.rotation_deg[1]) - neutral[1]),
                VIEW_YAW,
            ));
            app.set_look_raw_y(unit(
                f64::from(raw.rotation_deg[0]) - neutral[0],
                VIEW_PITCH,
            ));
            app.set_look_out_x(unit(-f64::from(out.rotation_rad[1]).to_degrees(), VIEW_YAW));
            app.set_look_out_y(unit(
                f64::from(out.rotation_rad[0]).to_degrees(),
                VIEW_PITCH,
            ));
            app.set_look_roll(-out.rotation_rad[2].to_degrees());
        }
        self.refresh_rows();
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Connected(config, status) => {
                self.config = *config;
                self.status = status;
                self.connected = true;
                self.failure = None;
                let selected = self.selected.take();
                self.select(selected);
            }
            Event::Disconnected => {
                self.connected = false;
                self.status = Status::default();
                self.last_pose = None;
                self.refresh_all();
            }
            Event::Status(status) => {
                self.status = status;
                self.refresh_chrome();
                self.refresh_camera();
            }
            Event::Pose(raw) => {
                if raw.valid {
                    let input = Pose {
                        position_mm: raw.position_mm,
                        rotation_rad: raw.rotation_deg.map(f32::to_radians),
                    };
                    let out = self.pipeline.process(raw.micros, input);
                    self.last_pose = Some((Instant::now(), raw, out));
                }
            }
            Event::Recentred(game, neutral) => {
                match &game {
                    Some(key) => {
                        if let Some(profile) = self.config.games.get_mut(key) {
                            profile.neutral = neutral;
                        }
                    }
                    None => self.config.default.neutral = neutral,
                }
                self.failure = None;
                let _ = self.pipeline.set_profile(self.profile());
                self.refresh_all();
            }
            Event::Calibration(calibration) => {
                self.calibration = calibration;
                self.refresh_calibration(true);
                self.refresh_checklist();
            }
            Event::UpdateChecked(found) => {
                self.update_checking = false;
                match found {
                    Ok(Some(release)) => {
                        self.update_status = format!("Version {} is available.", release.version);
                        self.update = Some(*release);
                    }
                    Ok(None) => {
                        self.update_status = "You have the latest version.".to_owned();
                        self.update = None;
                    }
                    // Keep showing a release found earlier: one failed look
                    // does not make it go away.
                    Err(reason) => {
                        self.update_status = format!("Could not check for updates: {reason}");
                    }
                }
                self.refresh_update();
            }
            Event::UpdateDone(result) => {
                self.update_busy = false;
                match result {
                    Ok(message) => self.update_result = Some(message),
                    Err(reason) => self.failure = Some(format!("Update failed: {reason}")),
                }
                self.refresh_update();
                self.refresh_chrome();
            }
            Event::Driver(present) => {
                self.driver_installed = present;
                self.refresh_checklist();
            }
            Event::Installed(result) => {
                self.installing = false;
                match result {
                    Ok(()) => self.home_installed = true,
                    Err(reason) => self.failure = Some(format!("Install failed: {reason}")),
                }
                self.refresh_checklist();
                self.refresh_chrome();
            }
            Event::Service(state) => {
                self.service = state;
                self.refresh_service();
                self.refresh_chrome();
            }
            Event::ServiceDone(result) => {
                self.service_busy = false;
                self.failure = result.err();
                self.refresh_service();
                self.refresh_chrome();
            }
            Event::Games(games) => {
                self.games = games;
                self.refresh_games();
                self.refresh_checklist();
            }
            Event::GameAdded(added) => {
                self.adding_game = false;
                self.app.set_game_adding(false);
                if let Some(Err(reason)) = added {
                    self.failure = Some(reason);
                    self.refresh_chrome();
                }
            }
            Event::GameDone(app_id, result) => {
                self.games_busy.retain(|id| *id != app_id);
                if let Err(reason) = result {
                    self.game_notes.push((app_id, reason));
                }
                self.refresh_games();
            }
            Event::Calibrated(message) => {
                self.calibration_note = message;
                self.app.set_calibrating(false);
                self.refresh_calibration(false);
            }
            Event::Failed(reason) => {
                self.failure = Some(reason);
                self.refresh_chrome();
            }
        }
    }
}

// --- talking to the daemon ---------------------------------------------------

fn ask(
    stream: &mut UnixStream,
    reader: &mut impl BufRead,
    request: &Request,
) -> Result<Line, String> {
    let text = serde_json::to_string(request).map_err(|err| err.to_string())?;
    writeln!(stream, "{text}").map_err(|err| err.to_string())?;
    let mut answer = String::new();
    match reader.read_line(&mut answer) {
        Ok(0) => Err("the daemon closed the connection".into()),
        Ok(_) => serde_json::from_str(&answer).map_err(|err| format!("unexpected answer: {err}")),
        Err(err) => Err(err.to_string()),
    }
}

/// Follow the daemon: its settings on connect, then poses and status.
fn follow(events: Sender<Event>) {
    loop {
        if let Ok(mut stream) = UnixStream::connect(control::socket_path()) {
            let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
            if let Ok(clone) = stream.try_clone() {
                let mut reader = BufReader::new(clone);
                if let Ok(Line::State { config, status }) =
                    ask(&mut stream, &mut reader, &Request::Get)
                {
                    let _ = events.send(Event::Connected(Box::new(config), status));
                    if ask(
                        &mut stream,
                        &mut reader,
                        &Request::Subscribe {
                            poses: true,
                            gaze: false,
                        },
                    )
                    .is_ok()
                    {
                        let mut text = String::new();
                        loop {
                            text.clear();
                            match reader.read_line(&mut text) {
                                Ok(n) if n > 0 => {}
                                // The status line comes every second, so a
                                // timeout means the daemon is gone or stuck.
                                _ => break,
                            }
                            let sent = match serde_json::from_str::<Line>(&text) {
                                Ok(Line::Pose(pose)) => events.send(Event::Pose(pose)),
                                Ok(Line::Status(status)) => events.send(Event::Status(status)),
                                _ => Ok(()),
                            };
                            if sent.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }
        if events.send(Event::Disconnected).is_err() {
            return;
        }
        thread::sleep(RETRY_EVERY);
    }
}

/// Carry out the window's commands, one connection each.
fn send_commands(commands: Receiver<Command>, events: Sender<Event>) {
    for command in commands {
        let (request, recentre) = match command {
            Command::Set(game, profile) => (
                Request::SetProfile {
                    game,
                    profile: *profile,
                },
                None,
            ),
            Command::Delete(game) => (Request::DeleteProfile { game }, None),
            Command::Recentre(game) => (Request::Recentre { game: game.clone() }, Some(game)),
            Command::SetOverlay(overlay) => (Request::SetOverlay { overlay: *overlay }, None),
            Command::SetCamera(camera) => (Request::SetCamera { camera }, None),
        };
        let answer = UnixStream::connect(control::socket_path())
            .map_err(|err| format!("the daemon is not running: {err}"))
            .and_then(|mut stream| {
                let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
                let clone = stream.try_clone().map_err(|err| err.to_string())?;
                ask(&mut stream, &mut BufReader::new(clone), &request)
            });
        let event = match (answer, recentre) {
            (Ok(Line::Recentred { neutral }), Some(game)) => Some(Event::Recentred(game, neutral)),
            (Ok(Line::Error { reason }), _) | (Err(reason), _) => Some(Event::Failed(reason)),
            (Ok(_), _) => None,
        };
        if let Some(event) = event {
            if events.send(event).is_err() {
                return;
            }
        }
    }
}

/// Open the tuning window and run until it is closed.
pub fn run() -> Result<(), String> {
    let app = App::new().map_err(|err| format!("cannot open a window: {err}"))?;
    let (event_tx, event_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    thread::spawn({
        let events = event_tx.clone();
        move || follow(events)
    });
    thread::spawn({
        let events = event_tx.clone();
        move || {
            let _ = events.send(Event::Calibration(read_calibration()));
        }
    });
    // Look for a newer release when the window opens and every so often
    // after, unless the settings say not to, and whenever the About box
    // asks. A window left open for days still hears about a release.
    // Nothing about the user or the tracker is sent: it is a plain request
    // for the project's release list.
    let (check_now, check_asked) = mpsc::channel::<()>();
    thread::spawn({
        let events = event_tx.clone();
        move || {
            let automatic = opentobii_core::config::default_path()
                .and_then(|path| Config::load(&path).ok())
                .is_none_or(|config| config.updates.check);
            let mut look = automatic;
            loop {
                let mut wait = CHECK_EVERY;
                if look {
                    let found = update::newer_release(&update::Http, update::current())
                        .map(|release| release.map(Box::new));
                    if found.is_err() {
                        wait = CHECK_RETRY;
                    }
                    if events.send(Event::UpdateChecked(found)).is_err() {
                        return;
                    }
                }
                look = match check_asked.recv_timeout(wait) {
                    Ok(()) => true,
                    Err(mpsc::RecvTimeoutError::Timeout) => automatic,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                };
            }
        }
    });
    thread::spawn({
        let events = event_tx.clone();
        move || {
            let _ = events.send(Event::Driver(driver_present()));
        }
    });
    // Keep the Daemon and Games cards current: both can change behind the
    // window's back.
    thread::spawn({
        let events = event_tx.clone();
        move || loop {
            if events.send(Event::Service(service::state())).is_err() {
                return;
            }
            thread::sleep(Duration::from_secs(3));
        }
    });
    thread::spawn({
        let events = event_tx.clone();
        move || loop {
            if events.send(Event::Games(read_games())).is_err() {
                return;
            }
            thread::sleep(Duration::from_secs(5));
        }
    });
    thread::spawn({
        let events = event_tx.clone();
        move || send_commands(command_rx, events)
    });

    let blank = |names: [&str; 3], group| {
        let rows: Vec<AxisRow> = names
            .iter()
            .map(|name| row(name, &Axis::default(), group, None, None))
            .collect();
        Rc::new(VecModel::from(rows))
    };
    let rotation = blank(ROTATION_NAMES, Group::Rotation);
    let position = blank(POSITION_NAMES, Group::Position);
    app.set_rotation(ModelRc::from(Rc::clone(&rotation)));
    app.set_position(ModelRc::from(Rc::clone(&position)));

    let check_rows: Rc<VecModel<CheckRow>> = Rc::new(VecModel::default());
    app.set_checklist(ModelRc::from(Rc::clone(&check_rows)));
    let game_rows: Rc<VecModel<GameRow>> = Rc::new(VecModel::default());
    app.set_games(ModelRc::from(Rc::clone(&game_rows)));

    let ui = Rc::new(RefCell::new(Ui {
        app: app.clone_strong(),
        commands: command_tx,
        config: Config::default(),
        selected: None,
        status: Status::default(),
        connected: false,
        pipeline: Pipeline::new(Profile::default())?,
        last_pose: None,
        failure: None,
        calibration: None,
        calibration_note: String::new(),
        driver_installed: false,
        home_installed: home_copy_exists(),
        installing: false,
        check_rows: Rc::clone(&check_rows),
        update: None,
        update_busy: false,
        update_result: None,
        update_status: String::new(),
        update_checking: false,
        check_now,
        service: service::State::default(),
        service_busy: false,
        games: Vec::new(),
        adding_game: false,
        games_busy: Vec::new(),
        game_notes: Vec::new(),
        game_rows: Rc::clone(&game_rows),
        events: event_tx.clone(),
        pending: None,
        pending_overlay: None,
        last_sent: Instant::now(),
        last_overlay_sent: Instant::now(),
        rotation,
        position,
    }));
    ui.borrow_mut().refresh_all();

    // Every callback edits a copy of the selected profile and commits it.
    let edit = |ui: &Rc<RefCell<Ui>>, change: &dyn Fn(&mut Profile)| {
        if let Ok(mut ui) = ui.try_borrow_mut() {
            let mut profile = ui.profile();
            change(&mut profile);
            ui.commit(profile);
        }
    };

    app.on_axis_changed({
        let ui = Rc::clone(&ui);
        move |group, index, field, value| {
            let (Some(group), Ok(index)) = (Group::parse(&group), usize::try_from(index)) else {
                return;
            };
            edit(&ui, &|profile| {
                if let Some(axis) = axis_mut(profile, group, index) {
                    set_field(axis, group, &field, f64::from(value));
                }
            });
        }
    });
    app.on_axis_invert({
        let ui = Rc::clone(&ui);
        move |group, index, on| {
            let (Some(group), Ok(index)) = (Group::parse(&group), usize::try_from(index)) else {
                return;
            };
            edit(&ui, &|profile| {
                if let Some(axis) = axis_mut(profile, group, index) {
                    axis.invert = on;
                }
            });
        }
    });
    app.on_smoothing_toggled({
        let ui = Rc::clone(&ui);
        move |on| edit(&ui, &|profile| profile.smoothing.enabled = on)
    });
    app.on_steadiness_changed({
        let ui = Rc::clone(&ui);
        move |value| {
            edit(&ui, &|profile| {
                let cutoff = snap(cutoff_for(f64::from(value)), 0.01).max(0.1);
                profile.smoothing.rotation_steadiness = cutoff;
                profile.smoothing.position_steadiness = cutoff;
            });
        }
    });
    app.on_response_changed({
        let ui = Rc::clone(&ui);
        move |value| {
            edit(&ui, &|profile| {
                let response = snap(response_for(f64::from(value)), 0.01);
                profile.smoothing.rotation_response = response;
                // Position moves in bigger numbers, so it opens more slowly;
                // the ratio is the one tuned by hand in ETS2.
                profile.smoothing.position_response = response / 5.0;
            });
        }
    });
    app.on_reset_profile({
        let ui = Rc::clone(&ui);
        move || {
            edit(&ui, &|profile| {
                // Keep where straight ahead is: that is not a tuning value.
                *profile = Profile {
                    neutral: profile.neutral,
                    ..Profile::default()
                };
            });
        }
    });
    app.on_profile_selected({
        let ui = Rc::clone(&ui);
        move |index| {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                let game = usize::try_from(index)
                    .ok()
                    .and_then(|index| index.checked_sub(1))
                    .and_then(|at| ui.config.games.keys().nth(at).cloned());
                ui.select(game);
            }
        }
    });
    app.on_create_profile({
        let ui = Rc::clone(&ui);
        move || {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                if let Some(game) = game_without_profile(&ui.config, &ui.status) {
                    let start = ui.config.default;
                    ui.config.games.insert(game.clone(), start);
                    ui.select(Some(game));
                    ui.commit(start);
                    ui.flush(true);
                }
            }
        }
    });
    app.on_delete_profile({
        let ui = Rc::clone(&ui);
        move || {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                if let Some(game) = ui.selected.clone() {
                    ui.pending = None;
                    ui.config.games.remove(&game);
                    let _ = ui.commands.send(Command::Delete(game));
                    ui.select(None);
                }
            }
        }
    });
    app.set_overlay_styles(ModelRc::new(VecModel::from(
        Style::ALL
            .iter()
            .map(|style| SharedString::from(style.label()))
            .collect::<Vec<_>>(),
    )));
    app.set_overlay_colors(ModelRc::new(VecModel::from(
        OVERLAY_COLORS
            .iter()
            .filter_map(|hex| overlay::parse_color(hex))
            .map(|[r, g, b]| slint::Color::from_rgb_u8(r, g, b))
            .collect::<Vec<_>>(),
    )));
    // Every overlay control edits a copy of the settings and commits it.
    let edit_overlay = |ui: &Rc<RefCell<Ui>>, change: &dyn Fn(&mut Overlay)| {
        if let Ok(mut ui) = ui.try_borrow_mut() {
            let mut overlay = ui.config.overlay.clone();
            change(&mut overlay);
            ui.commit_overlay(overlay);
        }
    };
    app.on_camera_toggled({
        let ui = Rc::clone(&ui);
        move |on| {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                ui.set_camera(on);
            }
        }
    });
    app.on_overlay_toggled({
        let ui = Rc::clone(&ui);
        move |on| {
            edit_overlay(&ui, &|overlay| overlay.enabled = on);
            // Switching it on or off should not wait for the next tick.
            if let Ok(mut ui) = ui.try_borrow_mut() {
                ui.flush(true);
            }
        }
    });
    app.on_overlay_style_selected({
        let ui = Rc::clone(&ui);
        move |index| {
            let style = usize::try_from(index)
                .ok()
                .and_then(|at| Style::ALL.get(at).copied());
            if let Some(style) = style {
                edit_overlay(&ui, &|overlay| overlay.style = style);
            }
        }
    });
    app.on_overlay_color_selected({
        let ui = Rc::clone(&ui);
        move |index| {
            let color = usize::try_from(index)
                .ok()
                .and_then(|at| OVERLAY_COLORS.get(at).copied());
            if let Some(color) = color {
                edit_overlay(&ui, &|overlay| overlay.color = color.to_owned());
            }
        }
    });
    app.on_overlay_size_changed({
        let ui = Rc::clone(&ui);
        move |value| {
            edit_overlay(&ui, &|overlay| {
                overlay.size = snap(f64::from(value), 10.0)
                    .clamp(overlay::SIZE_RANGE.0, overlay::SIZE_RANGE.1);
            });
        }
    });
    app.on_overlay_opacity_changed({
        let ui = Rc::clone(&ui);
        move |value| {
            edit_overlay(&ui, &|overlay| {
                let opacity = snap(f64::from(value), 5.0) / 100.0;
                overlay.opacity = opacity.clamp(overlay::OPACITY_RANGE.0, overlay::OPACITY_RANGE.1);
            });
        }
    });
    app.on_overlay_smoothing_changed({
        let ui = Rc::clone(&ui);
        move |value| {
            edit_overlay(&ui, &|overlay| {
                overlay.smoothing = snap(f64::from(value), 5.0).clamp(0.0, 100.0)
            });
        }
    });
    app.on_overlay_trail_changed({
        let ui = Rc::clone(&ui);
        move |value| {
            edit_overlay(&ui, &|overlay| {
                overlay.trail = snap(f64::from(value), 5.0).clamp(0.0, 100.0)
            });
        }
    });
    app.on_service_start({
        let ui = Rc::clone(&ui);
        move || {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                ui.change_service(service::start);
            }
        }
    });
    app.on_service_stop({
        let ui = Rc::clone(&ui);
        move || {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                ui.change_service(service::stop);
            }
        }
    });
    app.on_service_restart({
        let ui = Rc::clone(&ui);
        move || {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                ui.change_service(service::restart);
            }
        }
    });
    app.on_service_autostart({
        let ui = Rc::clone(&ui);
        move |on| {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                ui.change_service(if on {
                    || service::set_enabled(true)
                } else {
                    || service::set_enabled(false)
                });
            }
        }
    });
    app.on_game_add({
        let ui = Rc::clone(&ui);
        move || {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                ui.add_game();
            }
        }
    });
    app.on_game_forget({
        let ui = Rc::clone(&ui);
        move |index| {
            if let (Ok(mut ui), Ok(index)) = (ui.try_borrow_mut(), usize::try_from(index)) {
                ui.forget_game(index);
            }
        }
    });
    app.on_game_action({
        let ui = Rc::clone(&ui);
        move |index| {
            if let (Ok(mut ui), Ok(index)) = (ui.try_borrow_mut(), usize::try_from(index)) {
                ui.change_game(index, false);
            }
        }
    });
    app.on_game_remove({
        let ui = Rc::clone(&ui);
        move |index| {
            if let (Ok(mut ui), Ok(index)) = (ui.try_borrow_mut(), usize::try_from(index)) {
                ui.change_game(index, true);
            }
        }
    });
    app.set_version_text(format!("Version {}", update::current()).into());
    app.on_check_updates({
        let ui = Rc::clone(&ui);
        move || {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                ui.check_for_updates();
            }
        }
    });
    app.on_update_now({
        let ui = Rc::clone(&ui);
        move || {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                ui.update_now();
            }
        }
    });
    app.on_checklist_action({
        let ui = Rc::clone(&ui);
        move |index| {
            if let (Ok(mut ui), Ok(index)) = (ui.try_borrow_mut(), usize::try_from(index)) {
                ui.checklist_action(index);
            }
        }
    });
    app.on_calibrate({
        let ui = Rc::clone(&ui);
        move || {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                ui.calibrate();
            }
        }
    });
    app.on_recentre({
        let ui = Rc::clone(&ui);
        move || {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                ui.flush(true);
                let _ = ui.commands.send(Command::Recentre(ui.selected.clone()));
            }
        }
    });

    // The window's heartbeat: take in what the threads sent, redraw the
    // live view, and send any change a slider left waiting.
    let timer = Timer::default();
    timer.start(TimerMode::Repeated, Duration::from_millis(16), {
        let ui = Rc::clone(&ui);
        move || {
            if let Ok(mut ui) = ui.try_borrow_mut() {
                let mut poses = false;
                while let Ok(event) = event_rx.try_recv() {
                    poses |= matches!(event, Event::Pose(_));
                    ui.on_event(event);
                }
                let stale = ui.app.get_tracking()
                    && ui
                        .last_pose
                        .as_ref()
                        .is_none_or(|(at, _, _)| at.elapsed() >= POSE_TIMEOUT);
                if poses || stale {
                    ui.refresh_live();
                }
                ui.flush(false);
            }
        }
    });

    // For screenshots in reviews and docs: with OPENTOBII_GUI_SNAPSHOT set to
    // a path, save the window there as a PPM image after a moment and quit.
    let snapshot = Timer::default();
    if let Some(path) = std::env::var_os("OPENTOBII_GUI_SNAPSHOT") {
        // A taller window shows the cards below the fold:
        // OPENTOBII_GUI_SNAPSHOT_HEIGHT, in pixels.
        if let Some(height) = std::env::var("OPENTOBII_GUI_SNAPSHOT_HEIGHT")
            .ok()
            .and_then(|text| text.parse::<u32>().ok())
        {
            let width = app.window().size().width.max(1040);
            app.window()
                .set_size(slint::PhysicalSize::new(width, height));
        }
        let app = app.clone_strong();
        snapshot.start(
            TimerMode::SingleShot,
            Duration::from_millis(3000),
            move || {
                match app.window().take_snapshot() {
                    Ok(pixels) => {
                        let mut image =
                            format!("P6\n{} {}\n255\n", pixels.width(), pixels.height())
                                .into_bytes();
                        for pixel in pixels.as_slice() {
                            image.extend_from_slice(&[pixel.r, pixel.g, pixel.b]);
                        }
                        if let Err(err) = std::fs::write(&path, image) {
                            eprintln!("opentobii: cannot save the snapshot: {err}");
                        }
                    }
                    Err(err) => eprintln!("opentobii: cannot take a snapshot: {err}"),
                }
                let _ = slint::quit_event_loop();
            },
        );
    }

    let result = app.run().map_err(|err| format!("the window failed: {err}"));
    if let Ok(mut ui) = ui.try_borrow_mut() {
        ui.flush(true);
    }
    // Give the last change a moment to reach the daemon before the process
    // ends and takes the sending thread with it.
    thread::sleep(Duration::from_millis(150));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentobii_core::control::GameStatus;

    fn game(helper: Option<Result<prefix::Installed, String>>) -> GameInfo {
        GameInfo {
            id: "227300".into(),
            name: "Euro Truck Simulator 2".into(),
            source: Source::Steam,
            prefix: "/pfx".into(),
            helper,
        }
    }

    fn found(
        service_key: bool,
        helper: bool,
        helper_current: bool,
        in_use: bool,
    ) -> Option<Result<prefix::Installed, String>> {
        Some(Ok(prefix::Installed {
            service_key,
            helper,
            helper_current,
            in_use,
        }))
    }

    #[test]
    fn the_checklist_lists_the_steps_in_order_with_the_right_buttons() {
        let nothing = checklist(Readiness {
            can_start: true,
            ..Readiness::default()
        });
        assert_eq!(nothing.len(), 5);
        assert!(nothing.iter().all(|row| !row.done));
        assert_eq!(nothing[CHECK_INSTALL].action, "Install");
        assert_eq!(nothing[CHECK_DAEMON].action, "Start");
        assert!(nothing[0].action.is_empty() && nothing[0].detail.contains("tobii-linux"));
        assert!(nothing[4].detail.contains("No installed Steam game"));

        let all = checklist(Readiness {
            driver_installed: true,
            calibrated: true,
            installed: true,
            daemon_running: true,
            can_start: true,
            game_set_up: true,
            any_game: true,
            installing: false,
            starting: false,
        });
        assert!(all.iter().all(|row| row.done));
    }

    #[test]
    fn without_systemd_the_daemon_step_says_how_to_start_it_by_hand() {
        let rows = checklist(Readiness::default());
        assert!(rows[CHECK_DAEMON].action.is_empty());
        assert!(rows[CHECK_DAEMON].detail.contains("opentobii daemon"));
        let busy = checklist(Readiness {
            can_start: true,
            starting: true,
            installing: true,
            ..Readiness::default()
        });
        assert!(busy[CHECK_DAEMON].busy && busy[CHECK_INSTALL].busy);
    }

    #[test]
    fn release_notes_are_cut_to_a_few_lines_for_the_banner() {
        assert_eq!(first_lines("One\n\n  Two  \nThree", 4), "One\nTwo\nThree");
        assert_eq!(first_lines("1\n2\n3\n4\n5", 2), "1\n2\n…");
        assert_eq!(first_lines("", 3), "");
        assert_eq!(
            first_lines(
                "## Fixed\n\n- The `Update` banner\n**Full Changelog**: x",
                4
            ),
            "Fixed\n• The Update banner\nFull Changelog: x"
        );
    }

    #[test]
    fn a_game_row_offers_the_one_sensible_next_step() {
        let row = game_row(&game(found(false, false, false, false)), None, false);
        assert_eq!((row.action.as_str(), row.removable), ("Set up", false));

        let row = game_row(&game(found(true, true, true, false)), None, false);
        assert_eq!(
            (row.state.as_str(), row.action.as_str(), row.removable),
            ("Set up.", "", true)
        );

        let row = game_row(&game(found(true, true, false, false)), None, false);
        assert_eq!((row.action.as_str(), row.removable), ("Update", true));

        let row = game_row(&game(found(true, false, false, false)), None, false);
        assert_eq!(row.action, "Repair");

        let row = game_row(&game(None), None, false);
        assert!(row.state.contains("Run the game once") && row.action.is_empty() && !row.removable);
    }

    #[test]
    fn a_running_game_cannot_be_changed_and_says_why() {
        let row = game_row(&game(found(true, true, false, true)), None, false);
        assert!(row.action.is_empty() && !row.removable);
        assert!(row.state.contains("Close the game"), "{}", row.state);
    }

    #[test]
    fn a_failed_change_is_shown_in_the_row() {
        let row = game_row(
            &game(found(false, false, false, false)),
            Some("disk full"),
            true,
        );
        assert_eq!(row.state, "disk full");
        assert!(row.busy);
    }

    #[test]
    fn the_daemon_card_tells_the_four_states_apart() {
        let managed = service::State {
            systemd: true,
            installed: true,
            enabled: false,
            active: true,
        };
        let idle = service::State {
            active: false,
            ..managed
        };
        assert_eq!(service_summary(managed, true), "Running.");
        assert!(service_summary(idle, true).contains("started by hand"));
        assert_eq!(service_summary(managed, false), "Starting…");
        assert_eq!(service_summary(idle, false), "Not running.");
        assert!(service_summary(service::State::default(), false).contains("opentobii daemon"));
    }

    #[test]
    fn reads_the_calibration_tools_monitor_list() {
        let text = "0: HDMI-A-2 (1920x1080, 598 x 336 mm)\n1: DP-2 (2560x1440, 697 x 392 mm)\n2: DP-4 (2560x1440, 590 x 333 mm)  <- calibrates here\nnot a monitor line\n";
        let monitors = parse_monitors(text);
        assert_eq!(monitors.len(), 3);
        assert_eq!(monitors[0].name, "HDMI-A-2");
        assert_eq!(monitors[2].label, "DP-4 (2560x1440, 590 x 333 mm)");
        assert_eq!(
            monitors.iter().map(|m| m.current).collect::<Vec<_>>(),
            [false, false, true]
        );
        assert!(parse_monitors("").is_empty());
    }

    #[test]
    fn reads_the_calibration_tools_status() {
        let text = "calibration   yours: id 1453771709, 14 points, saved 2026-11-14 (~/.config/tobii/calibration.bin)\ndisplay area  yours: 590.0 x 333.0 mm (26.7\"), saved 2026-11-14 (~/.config/tobii/display-area)\ntracker       plugged in (USB 3-2)\nreminders     on, none shown yet\n";
        assert_eq!(
            parse_status(text),
            "Calibration: yours: id 1453771709, 14 points, saved 2026-11-14\nDisplay area: yours: 590.0 x 333.0 mm (26.7\"), saved 2026-11-14"
        );
        assert_eq!(parse_status("something else entirely"), "");
    }

    #[test]
    fn exit_statuses_become_plain_messages() {
        assert_eq!(calibration_result(Some(0)), "Calibration saved.");
        assert!(calibration_result(Some(2)).contains("next start"));
        assert_eq!(
            calibration_result(Some(1)),
            "Calibration was not completed."
        );
        assert_eq!(calibration_result(None), "Calibration was not completed.");
    }

    #[test]
    fn steadiness_slider_covers_three_hertz_down_to_a_tenth() {
        assert!((cutoff_for(0.0) - 3.0).abs() < 1e-9);
        assert!((cutoff_for(100.0) - 0.1).abs() < 1e-9);
        assert!(cutoff_for(50.0) < 1.0 && cutoff_for(50.0) > 0.3);
        for cutoff in [0.1, 0.4, 0.8, 1.5, 3.0] {
            assert!(
                (cutoff_for(steadiness_for(cutoff)) - cutoff).abs() < 1e-9,
                "{cutoff}"
            );
        }
        assert_eq!(steadiness_for(50.0), 0.0, "out of range clamps");
    }

    #[test]
    fn response_slider_round_trips() {
        assert_eq!(response_for(0.0), 0.0);
        assert!((response_for(100.0) - 0.5).abs() < 1e-12);
        assert!((slider_for_response(response_for(20.0)) - 20.0).abs() < 1e-9);
    }

    #[test]
    fn slider_values_are_snapped_and_clamped() {
        let mut axis = Axis::default();
        assert!(set_field(&mut axis, Group::Rotation, "dead_zone", 2.34));
        assert!((axis.dead_zone - 2.3).abs() < 1e-9);
        assert!(set_field(&mut axis, Group::Rotation, "dead_zone", 99.0));
        assert_eq!(axis.dead_zone, 10.0);
        assert!(set_field(&mut axis, Group::Rotation, "sensitivity", 1.52));
        assert!((axis.sensitivity - 1.5).abs() < 1e-9);
        assert!(set_field(&mut axis, Group::Rotation, "curve", 0.1));
        assert_eq!(axis.curve, 0.5);
        assert!(set_field(&mut axis, Group::Position, "limit", 0.4));
        assert_eq!(axis.limit, 0.0, "the bottom of the slider is no limit");
        assert!(set_field(&mut axis, Group::Position, "limit", 120.4));
        assert_eq!(axis.limit, 120.0);
        assert!(!set_field(&mut axis, Group::Rotation, "colour", 1.0));
        let profile = Profile {
            yaw: axis,
            ..Profile::default()
        };
        assert!(
            profile.validate().is_ok(),
            "whatever the sliders produce is a usable profile"
        );
    }

    #[test]
    fn rows_show_values_in_the_groups_units() {
        let axis = Axis {
            dead_zone: 1.5,
            sensitivity: 2.0,
            curve: 1.25,
            invert: true,
            limit: 0.0,
        };
        let turning = row("Yaw", &axis, Group::Rotation, Some(-3.25), Some(-3.5));
        assert_eq!(turning.dead_zone_text, "1.5°");
        assert_eq!(turning.sensitivity_text, "2.00×");
        assert_eq!(turning.limit_text, "Off");
        assert_eq!(
            (turning.raw_text.as_str(), turning.out_text.as_str()),
            ("-3.2°", "-3.5°")
        );
        let leaning = row(
            "Side",
            &Axis {
                limit: 200.0,
                ..axis
            },
            Group::Position,
            None,
            None,
        );
        assert_eq!(leaning.dead_zone_text, "2 mm");
        assert_eq!(leaning.limit_text, "200 mm");
        assert_eq!(leaning.raw_text, "–");
    }

    #[test]
    fn offers_a_profile_for_a_connected_game_that_has_none() {
        let mut config = Config::default();
        let game = |key: &str| GameStatus {
            pid: 1,
            executable: key.into(),
            game: key.into(),
            own_profile: false,
            head_pose: true,
        };
        let status = Status {
            games: vec![game(""), game("eurotrucks2.exe")],
            ..Status::default()
        };
        assert_eq!(
            game_without_profile(&config, &status).as_deref(),
            Some("eurotrucks2.exe")
        );
        config
            .games
            .insert("eurotrucks2.exe".into(), Profile::default());
        assert_eq!(game_without_profile(&config, &status), None);
        assert_eq!(
            profile_names(&config),
            [DEFAULT_PROFILE_NAME, "eurotrucks2.exe"]
        );
    }
}
