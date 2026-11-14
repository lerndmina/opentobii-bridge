//! The gaze overlay: `opentobii overlay`.
//!
//! Draws a mark on screen where the user is looking, with a short trail
//! behind it, in the manner of Tobii Ghost. It is a client of the daemon's
//! control socket for its settings and the gaze feed, and a Wayland client
//! for the drawing.
//!
//! One transparent surface covers the tracker's monitor on the compositor's
//! overlay layer, above every window including a full-screen game. Its input
//! region is empty: clicks and keys go to whatever is underneath. Only the
//! small area around the mark is redrawn, once per screen refresh while
//! something is moving and not at all while nothing is. This needs the
//! layer-shell protocol, which KDE Plasma, Sway, Hyprland and COSMIC offer.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::{Duration, Instant};

use opentobii_core::control::{self, GazePoint, Line, Request};
use opentobii_core::overlay::{trail_seconds, GazeFilter, Overlay, Style};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel::{
    self, Channel, Event as ChannelEvent, Sender,
};
use smithay_client_toolkit::reexports::calloop::EventLoop;
use smithay_client_toolkit::reexports::calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::shell::wlr_layer::{
    Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
    LayerSurfaceConfigure,
};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    registry_handlers,
};
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_output, wl_shm, wl_surface};
use wayland_client::{Connection, QueueHandle};

const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// No valid gaze for this long and the mark starts to fade.
const LOST_AFTER: f64 = 0.4;
/// How long the mark takes to appear and to fade away, seconds.
const FADE_IN: f64 = 0.12;
const FADE_OUT: f64 = 0.35;
/// How quickly the drawn mark closes on the newest gaze point, seconds.
/// Gaze arrives 33 times a second and the screen refreshes faster, so this
/// is what makes the motion smooth instead of stepped.
const FOLLOW: f64 = 0.028;
/// Spacing in time between the trail's marks, seconds.
const TRAIL_STEP: f64 = 0.028;
/// A step longer than this is treated as a pause, not as motion.
const MAX_STEP: f64 = 0.1;

// --- drawing, tested without a display ---------------------------------------

/// A rectangle of pixels: left and top inside, right and bottom outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Rect {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl Rect {
    fn union(self, other: Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }
}

fn join(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.union(b)),
        (one, None) | (None, one) => one,
    }
}

/// One mark to draw: where, how big and how strongly.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Mark {
    x: f64,
    y: f64,
    radius: f64,
    alpha: f64,
}

fn smoothstep(edge0: f64, edge1: f64, x: f64) -> f64 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn bell(x: f64, width: f64) -> f64 {
    let t = x / width;
    (-t * t).exp()
}

/// How far past its radius a style draws, as a multiple of the radius.
fn reach(style: Style) -> f64 {
    match style {
        Style::Bubble => 1.45,
        Style::Solid => 1.05,
        Style::Heatmap => 1.9,
    }
}

/// What a style puts at distance `d` pixels from the centre of a mark of
/// `radius` pixels: how opaque, and how far towards white the colour is
/// pushed (the hot core of a glow).
fn shade(style: Style, d: f64, radius: f64) -> (f64, f64) {
    match style {
        Style::Bubble => {
            // A thin bright ring, a wider soft glow around it, and a trace
            // of fill inside so the ring reads as a lens, not a wire.
            let ring_at = radius * 0.82;
            let width = (radius * 0.085).max(1.4);
            let core = bell(d - ring_at, width);
            let glow = 0.42 * bell(d - ring_at, width * 3.4);
            let fill = if d < ring_at { 0.07 } else { 0.0 };
            (core.max(glow).max(fill), core * core * 0.75)
        }
        Style::Solid => {
            let alpha = 1.0 - smoothstep(radius * 0.72, radius, d);
            (alpha, 0.3 * (1.0 - d / radius).max(0.0))
        }
        Style::Heatmap => {
            let alpha = 0.85 * bell(d, radius * 0.78);
            (alpha, alpha * alpha * alpha * 0.7)
        }
    }
}

/// Draw one mark over what is already on the canvas. The canvas is
/// premultiplied ARGB8888 in the byte order Wayland's shared-memory buffers
/// use (blue, green, red, alpha). Returns the pixels touched.
fn draw_mark(
    canvas: &mut [u8],
    width: i32,
    height: i32,
    mark: &Mark,
    style: Style,
    rgb: [u8; 3],
    opacity: f64,
) -> Option<Rect> {
    let strength = (mark.alpha * opacity).clamp(0.0, 1.0);
    if strength <= 0.002 || mark.radius < 0.5 || !mark.x.is_finite() || !mark.y.is_finite() {
        return None;
    }
    let extent = mark.radius * reach(style) + 1.0;
    let rect = Rect {
        x0: ((mark.x - extent).floor() as i32).max(0),
        y0: ((mark.y - extent).floor() as i32).max(0),
        x1: ((mark.x + extent).ceil() as i32).min(width),
        y1: ((mark.y + extent).ceil() as i32).min(height),
    };
    if rect.x0 >= rect.x1 || rect.y0 >= rect.y1 {
        return None;
    }
    let [red, green, blue] = rgb.map(f64::from);
    for y in rect.y0..rect.y1 {
        let dy = f64::from(y) + 0.5 - mark.y;
        let row = (y as usize) * (width as usize) * 4;
        for x in rect.x0..rect.x1 {
            let dx = f64::from(x) + 0.5 - mark.x;
            let (alpha, white) = shade(style, (dx * dx + dy * dy).sqrt(), mark.radius);
            let alpha = alpha * strength;
            // Below three steps of 255 a pixel would carry alpha but no
            // colour, which shows as a faint dark box around a soft mark.
            if alpha <= 0.012 {
                continue;
            }
            let at = row + (x as usize) * 4;
            let Some(pixel) = canvas.get_mut(at..at + 4) else {
                continue;
            };
            let keep = 1.0 - alpha;
            let lit = |colour: f64| (colour + (255.0 - colour) * white) * alpha;
            let over =
                |below: u8, above: f64| (above + f64::from(below) * keep).round().min(255.0) as u8;
            pixel[0] = over(pixel[0], lit(blue));
            pixel[1] = over(pixel[1], lit(green));
            pixel[2] = over(pixel[2], lit(red));
            pixel[3] = over(pixel[3], alpha * 255.0);
        }
    }
    Some(rect)
}

fn clear(canvas: &mut [u8], width: i32, rect: Rect) {
    for y in rect.y0..rect.y1 {
        let start = ((y as usize) * (width as usize) + rect.x0 as usize) * 4;
        let end = start + ((rect.x1 - rect.x0) as usize) * 4;
        if let Some(row) = canvas.get_mut(start..end) {
            row.fill(0);
        }
    }
}

/// Where the mark is, where it has been, and how visible it is.
#[derive(Debug, Clone)]
struct Motion {
    filter: GazeFilter,
    /// The newest smoothed gaze point, in pixels.
    target: Option<[f64; 2]>,
    /// Where the mark is drawn: it glides towards the target.
    shown: Option<[f64; 2]>,
    /// 0 hidden, 1 fully shown.
    visibility: f64,
    /// When a valid gaze last arrived, seconds.
    last_valid: Option<f64>,
    /// Where the mark was drawn recently, oldest first: time and place.
    history: VecDeque<(f64, [f64; 2])>,
}

impl Motion {
    fn new(smoothing: f64) -> Self {
        Self {
            filter: GazeFilter::new(smoothing),
            target: None,
            shown: None,
            visibility: 0.0,
            last_valid: None,
            history: VecDeque::new(),
        }
    }

    /// Take in one gaze sample at `now` seconds.
    fn on_gaze(&mut self, gaze: GazePoint, screen: (i32, i32), now: f64) {
        if !gaze.valid {
            return;
        }
        let [x, y] = self.filter.filter(
            gaze.micros,
            [gaze.x.clamp(0.0, 1.0), gaze.y.clamp(0.0, 1.0)],
        );
        self.target = Some([
            f64::from(x) * f64::from(screen.0),
            f64::from(y) * f64::from(screen.1),
        ]);
        if self.last_valid.is_none_or(|at| now - at > LOST_AFTER) {
            // Back after a gap: appear where the eyes are now, not glide in
            // from where they were.
            self.shown = self.target;
            self.history.clear();
        }
        self.last_valid = Some(now);
    }

    fn where_at(&self, time: f64) -> Option<[f64; 2]> {
        let mut after = None;
        for (at, place) in self.history.iter().rev() {
            if *at <= time {
                let Some((later, next)) = after else {
                    return Some(*place);
                };
                let span: f64 = later - at;
                let t = if span > 0.0 { (time - at) / span } else { 0.0 };
                let next: [f64; 2] = next;
                return Some([
                    place[0] + (next[0] - place[0]) * t,
                    place[1] + (next[1] - place[1]) * t,
                ]);
            }
            after = Some((*at, *place));
        }
        None
    }

    /// Advance to `now` seconds and say what to draw, back of the trail
    /// first. Nothing to draw means nothing is visible.
    fn step(&mut self, now: f64, dt: f64, settings: &Overlay) -> Vec<Mark> {
        let dt = dt.clamp(0.0, MAX_STEP);
        let seen = self.last_valid.is_some_and(|at| now - at <= LOST_AFTER);
        let fade = if seen { dt / FADE_IN } else { -dt / FADE_OUT };
        self.visibility = (self.visibility + fade).clamp(0.0, 1.0);

        if let (Some(target), true) = (self.target, seen) {
            let shown = self.shown.unwrap_or(target);
            let pull = 1.0 - (-dt / FOLLOW).exp();
            self.shown = Some([
                shown[0] + (target[0] - shown[0]) * pull,
                shown[1] + (target[1] - shown[1]) * pull,
            ]);
        }
        let Some(head) = self.shown else {
            return Vec::new();
        };
        let trail = trail_seconds(settings.trail);
        self.history.push_back((now, head));
        while self
            .history
            .front()
            .is_some_and(|(at, _)| now - at > trail + 2.0 * TRAIL_STEP)
        {
            self.history.pop_front();
        }
        if self.visibility <= 0.003 {
            return Vec::new();
        }

        let radius = settings.size / 2.0;
        let mut marks = Vec::new();
        if trail > 0.0 {
            let count = (trail / TRAIL_STEP).floor() as u32;
            let mut previous = head;
            let mut tail = Vec::new();
            for index in 1..=count {
                let age = f64::from(index) * TRAIL_STEP;
                let Some(place) = self.where_at(now - age) else {
                    break;
                };
                // Marks piled on one spot would only thicken the ring, so a
                // still gaze has no trail at all.
                let gap =
                    ((place[0] - previous[0]).powi(2) + (place[1] - previous[1]).powi(2)).sqrt();
                if gap < radius * 0.3 {
                    continue;
                }
                previous = place;
                let spent = age / trail;
                tail.push(Mark {
                    x: place[0],
                    y: place[1],
                    radius: radius * (1.0 - 0.62 * spent),
                    alpha: self.visibility * (1.0 - spent).powf(1.5) * 0.55,
                });
            }
            marks.extend(tail.into_iter().rev());
        }
        marks.push(Mark {
            x: head[0],
            y: head[1],
            radius,
            alpha: self.visibility,
        });
        marks
    }

    /// Nothing is showing and nothing is about to.
    fn at_rest(&self, now: f64) -> bool {
        self.visibility <= 0.003 && self.last_valid.is_none_or(|at| now - at > LOST_AFTER)
    }
}

/// The monitor the driver was last calibrated on, from the file
/// `tobii-calibrate` keeps: comment lines, then the connector name.
fn calibrated_monitor(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
}

fn driver_monitor() -> Option<String> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::path::PathBuf::from(std::env::var_os("HOME")?).join(".config"),
    };
    calibrated_monitor(&std::fs::read_to_string(base.join("tobii/calibration-monitor")).ok()?)
}

// --- the daemon's side -------------------------------------------------------

enum Feed {
    Gaze(GazePoint),
    Settings(Overlay),
    /// The daemon went away.
    Lost,
}

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

/// Connect to the daemon and read the overlay's settings. Returns them and
/// the connection, subscribed to gaze.
fn connect() -> Result<(Overlay, BufReader<UnixStream>), String> {
    let path = control::socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|err| {
        format!(
            "the opentobii daemon is not running ({}: {err})",
            path.display()
        )
    })?;
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|err| err.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|err| err.to_string())?);
    let Line::State { config, .. } = ask(&mut stream, &mut reader, &Request::Get)? else {
        return Err("unexpected answer from the daemon".into());
    };
    ask(
        &mut stream,
        &mut reader,
        &Request::Subscribe {
            poses: false,
            gaze: true,
        },
    )?;
    Ok((config.overlay, reader))
}

fn follow(mut reader: BufReader<UnixStream>, feed: &Sender<Feed>) {
    let mut text = String::new();
    loop {
        text.clear();
        match reader.read_line(&mut text) {
            Ok(n) if n > 0 => {}
            // The daemon sends a status line every second, so a timeout
            // means it is gone or stuck.
            _ => break,
        }
        let sent = match serde_json::from_str::<Line>(&text) {
            Ok(Line::Gaze(gaze)) => feed.send(Feed::Gaze(gaze)),
            Ok(Line::Overlay(settings)) => feed.send(Feed::Settings(settings)),
            _ => Ok(()),
        };
        if sent.is_err() {
            return;
        }
    }
    let _ = feed.send(Feed::Lost);
}

// --- the Wayland side ---------------------------------------------------------

/// One of the two buffers the surface flips between, and what is drawn in
/// it.
struct Page {
    buffer: Buffer,
    drawn: Option<Rect>,
}

struct App {
    registry: RegistryState,
    outputs: OutputState,
    compositor: CompositorState,
    shm: Shm,
    layer_shell: LayerShell,
    pool: SlotPool,
    queue: QueueHandle<Self>,
    started: Instant,

    settings: Overlay,
    motion: Motion,
    layer: Option<LayerSurface>,
    pages: Vec<Page>,
    /// The surface's size in pixels, once the compositor has said.
    size: Option<(i32, i32)>,
    /// What the buffer on screen has drawn in it.
    on_screen: Option<Rect>,
    /// A buffer has been committed, so the surface exists on screen.
    mapped: bool,
    /// A frame callback is on its way: the next draw is already scheduled.
    waiting: bool,
    last_step: f64,
    done: Option<Result<(), String>>,
}

impl App {
    fn now(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    /// The output to draw on: the one named in the settings, else the one
    /// the driver was calibrated on, else the first.
    fn pick_output(&self) -> Option<wl_output::WlOutput> {
        let wanted = self.settings.monitor.clone().or_else(driver_monitor);
        let mut first = None;
        for output in self.outputs.outputs() {
            let Some(info) = self.outputs.info(&output) else {
                continue;
            };
            if wanted.is_some() && info.name == wanted {
                return Some(output);
            }
            first.get_or_insert(output);
        }
        first
    }

    /// Make (or remake) the surface.
    fn open(&mut self) -> Result<(), String> {
        self.layer = None;
        self.pages.clear();
        self.size = None;
        self.on_screen = None;
        self.mapped = false;
        self.waiting = false;
        let output = self.pick_output().ok_or("no monitor to draw on")?;

        let surface = self.compositor.create_surface(&self.queue);
        // An empty input region: every click and key goes to what is below.
        let nowhere = Region::new(&self.compositor).map_err(|err| err.to_string())?;
        surface.set_input_region(Some(nowhere.wl_region()));

        let layer = self.layer_shell.create_layer_surface(
            &self.queue,
            surface,
            Layer::Overlay,
            Some("opentobii-gaze"),
            Some(&output),
        );
        // Anchored to all four edges with no size of its own: the compositor
        // makes it the size of the monitor.
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, 0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // -1: do not make room for panels; cover the whole monitor.
        layer.set_exclusive_zone(-1);
        // The first commit has no buffer; the compositor answers with a
        // configure, and drawing starts there.
        layer.commit();
        self.layer = Some(layer);
        Ok(())
    }

    fn resize(&mut self, width: i32, height: i32) {
        if self.size == Some((width, height)) && self.pages.len() == 2 {
            return;
        }
        self.pages.clear();
        self.on_screen = None;
        self.size = Some((width, height));
        for _ in 0..2 {
            let Ok((buffer, canvas)) =
                self.pool
                    .create_buffer(width, height, width * 4, wl_shm::Format::Argb8888)
            else {
                self.done = Some(Err("cannot allocate the overlay's buffers".into()));
                return;
            };
            canvas.fill(0);
            self.pages.push(Page {
                buffer,
                drawn: None,
            });
        }
    }

    /// Draw the next frame, if there is anything to change.
    fn draw(&mut self) {
        self.waiting = false;
        let (Some(layer), Some((width, height))) = (self.layer.clone(), self.size) else {
            return;
        };
        let now = self.now();
        let marks = self.motion.step(now, now - self.last_step, &self.settings);
        self.last_step = now;
        let moving = !self.motion.at_rest(now);
        if marks.is_empty() && self.on_screen.is_none() && self.mapped {
            // Nothing showing and nothing to show: go quiet until gaze comes.
            return;
        }

        let surface = layer.wl_surface();
        let Self {
            pages,
            pool,
            settings,
            ..
        } = self;
        let Some(index) =
            (0..pages.len()).find(|index| pages[*index].buffer.canvas(pool).is_some())
        else {
            // Both buffers are still with the compositor: try next refresh.
            surface.frame(&self.queue, surface.clone());
            layer.commit();
            self.waiting = true;
            return;
        };
        let page = &mut pages[index];
        let Some(canvas) = page.buffer.canvas(pool) else {
            return;
        };
        if let Some(old) = page.drawn.take() {
            clear(canvas, width, old);
        }
        let rgb = settings.rgb();
        let mut fresh = None;
        for mark in &marks {
            fresh = join(
                fresh,
                draw_mark(
                    canvas,
                    width,
                    height,
                    mark,
                    settings.style,
                    rgb,
                    settings.opacity,
                ),
            );
        }
        // What changes on screen: what was there, and what is there now.
        let changed = join(self.on_screen, fresh);
        page.drawn = fresh;
        if page.buffer.attach_to(surface).is_err() {
            return;
        }
        match changed {
            Some(rect) => {
                surface.damage_buffer(rect.x0, rect.y0, rect.x1 - rect.x0, rect.y1 - rect.y0)
            }
            None => surface.damage_buffer(0, 0, 1, 1),
        }
        if moving || fresh.is_some() {
            surface.frame(&self.queue, surface.clone());
            self.waiting = true;
        }
        layer.commit();
        self.on_screen = fresh;
        self.mapped = true;
    }

    fn on_gaze(&mut self, gaze: GazePoint) {
        let Some(size) = self.size else {
            return;
        };
        let now = self.now();
        self.motion.on_gaze(gaze, size, now);
        if !self.waiting {
            // Idle until now: do not treat the idle time as motion.
            self.last_step = now;
            self.draw();
        }
    }

    fn on_settings(&mut self, settings: Overlay) {
        if !settings.enabled {
            self.done = Some(Ok(()));
            return;
        }
        let reopen = settings.monitor != self.settings.monitor;
        if settings.smoothing != self.settings.smoothing {
            self.motion.filter = GazeFilter::new(settings.smoothing);
        }
        self.settings = settings;
        if reopen {
            if let Err(reason) = self.open() {
                self.done = Some(Err(reason));
            }
        } else if !self.waiting {
            self.last_step = self.now();
            self.draw();
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.draw();
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.outputs
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        // The monitor may have been ours: start again on what is left.
        if self.open().is_err() {
            self.done = Some(Err("the monitor went away".into()));
        }
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        // The compositor took the surface away, as it does when its monitor
        // is unplugged. Try once more on whatever is there now.
        if self.open().is_err() {
            self.done = Some(Err("the compositor closed the overlay".into()));
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (width, height) = configure.new_size;
        let (Ok(width), Ok(height)) = (i32::try_from(width), i32::try_from(height)) else {
            return;
        };
        if width == 0 || height == 0 {
            return;
        }
        self.resize(width, height);
        self.last_step = self.now();
        // An empty frame puts the surface on screen; the mark appears with
        // the first valid gaze.
        self.draw();
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_shm!(App);
delegate_layer!(App);
delegate_registry!(App);

/// Draw the gaze mark until the daemon says to stop or goes away.
pub fn run() -> Result<(), String> {
    let (settings, reader) = connect()?;
    if !settings.enabled {
        return Err(
            "the overlay is switched off in the settings; turn it on in the tuning window".into(),
        );
    }

    let connection = Connection::connect_to_env()
        .map_err(|err| format!("no Wayland display to draw on: {err}"))?;
    let (globals, mut events) =
        registry_queue_init::<App>(&connection).map_err(|err| err.to_string())?;
    let queue = events.handle();
    let compositor = CompositorState::bind(&globals, &queue).map_err(|err| err.to_string())?;
    let layer_shell = LayerShell::bind(&globals, &queue).map_err(|_| {
        "this desktop does not offer the layer-shell protocol the overlay needs \
         (KDE Plasma, Sway, Hyprland and COSMIC do)"
            .to_owned()
    })?;
    let shm = Shm::bind(&globals, &queue).map_err(|err| err.to_string())?;
    let pool = SlotPool::new(1920 * 1080 * 4 * 2, &shm).map_err(|err| err.to_string())?;

    let mut app = App {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &queue),
        compositor,
        shm,
        layer_shell,
        pool,
        queue,
        started: Instant::now(),
        motion: Motion::new(settings.smoothing),
        settings,
        layer: None,
        pages: Vec::new(),
        size: None,
        on_screen: None,
        mapped: false,
        waiting: false,
        last_step: 0.0,
        done: None,
    };
    // Twice: once to learn which monitors exist, once for their details.
    events.roundtrip(&mut app).map_err(|err| err.to_string())?;
    events.roundtrip(&mut app).map_err(|err| err.to_string())?;
    app.open()?;

    let mut event_loop: EventLoop<App> = EventLoop::try_new().map_err(|err| err.to_string())?;
    let handle = event_loop.handle();
    WaylandSource::new(connection, events)
        .insert(handle.clone())
        .map_err(|err| err.to_string())?;
    let (feed_tx, feed_rx): (Sender<Feed>, Channel<Feed>) = channel::channel();
    handle
        .insert_source(feed_rx, |event, (), app: &mut App| match event {
            ChannelEvent::Msg(Feed::Gaze(gaze)) => app.on_gaze(gaze),
            ChannelEvent::Msg(Feed::Settings(settings)) => app.on_settings(settings),
            ChannelEvent::Msg(Feed::Lost) | ChannelEvent::Closed => {
                app.done
                    .get_or_insert(Err("the opentobii daemon went away".into()));
            }
        })
        .map_err(|err| err.to_string())?;
    thread::spawn(move || follow(reader, &feed_tx));

    loop {
        event_loop
            .dispatch(None, &mut app)
            .map_err(|err| err.to_string())?;
        if let Some(result) = app.done.take() {
            return result;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: i32 = 200;
    const H: i32 = 200;

    fn canvas() -> Vec<u8> {
        vec![0; (W * H * 4) as usize]
    }

    fn alpha(canvas: &[u8], x: i32, y: i32) -> u8 {
        canvas[((y * W + x) * 4 + 3) as usize]
    }

    fn centred(radius: f64) -> Mark {
        Mark {
            x: 100.0,
            y: 100.0,
            radius,
            alpha: 1.0,
        }
    }

    fn valid(x: f32, y: f32, micros: i64) -> GazePoint {
        GazePoint {
            micros,
            x,
            y,
            valid: true,
        }
    }

    #[test]
    fn a_bubble_is_a_bright_ring_with_a_soft_glow_and_an_almost_clear_middle() {
        let mut image = canvas();
        let rect = draw_mark(
            &mut image,
            W,
            H,
            &centred(40.0),
            Style::Bubble,
            [0x60, 0xcd, 0xff],
            1.0,
        )
        .unwrap();
        // The ring sits at 0.82 of the radius: 33 pixels out.
        assert!(alpha(&image, 133, 100) > 230, "the ring is bright");
        assert!(alpha(&image, 100, 100) < 30, "the middle is nearly clear");
        let glow = alpha(&image, 142, 100);
        assert!(glow > 20 && glow < 140, "a glow outside the ring: {glow}");
        assert_eq!(alpha(&image, 170, 100), 0, "nothing far outside");
        assert!(rect.x0 <= 45 && rect.x1 >= 155 && rect.x0 > 20);
    }

    #[test]
    fn solid_is_filled_and_heatmap_fades_from_the_middle() {
        let mut solid = canvas();
        draw_mark(
            &mut solid,
            W,
            H,
            &centred(40.0),
            Style::Solid,
            [255, 128, 0],
            1.0,
        );
        assert_eq!(alpha(&solid, 100, 100), 255);
        assert_eq!(alpha(&solid, 120, 100), 255);
        assert_eq!(alpha(&solid, 145, 100), 0);

        let mut heat = canvas();
        draw_mark(
            &mut heat,
            W,
            H,
            &centred(40.0),
            Style::Heatmap,
            [255, 128, 0],
            1.0,
        );
        let (middle, half, edge) = (
            alpha(&heat, 100, 100),
            alpha(&heat, 120, 100),
            alpha(&heat, 140, 100),
        );
        assert!(
            middle > half && half > edge && edge > 0,
            "{middle} {half} {edge}"
        );
    }

    #[test]
    fn pixels_stay_premultiplied_and_opacity_scales_the_mark() {
        let mut image = canvas();
        draw_mark(
            &mut image,
            W,
            H,
            &centred(40.0),
            Style::Solid,
            [255, 128, 0],
            0.5,
        );
        let middle = ((100 * W + 100) * 4) as usize;
        assert_eq!(image[middle + 3], 128);
        for pixel in image.as_chunks::<4>().0 {
            assert!(pixel[0] <= pixel[3] && pixel[1] <= pixel[3] && pixel[2] <= pixel[3]);
        }
        // Drawing again over the top adds, and still never exceeds alpha.
        draw_mark(
            &mut image,
            W,
            H,
            &centred(40.0),
            Style::Bubble,
            [96, 205, 255],
            1.0,
        );
        for pixel in image.as_chunks::<4>().0 {
            assert!(pixel[0] <= pixel[3] && pixel[1] <= pixel[3] && pixel[2] <= pixel[3]);
        }
    }

    #[test]
    fn marks_off_the_edge_are_clipped_and_bad_ones_are_skipped() {
        let mut image = canvas();
        let corner = Mark {
            x: 0.0,
            y: 0.0,
            radius: 40.0,
            alpha: 1.0,
        };
        let rect = draw_mark(
            &mut image,
            W,
            H,
            &corner,
            Style::Bubble,
            [255, 255, 255],
            1.0,
        )
        .unwrap();
        assert_eq!((rect.x0, rect.y0), (0, 0));
        let away = Mark {
            x: -500.0,
            y: 100.0,
            radius: 40.0,
            alpha: 1.0,
        };
        assert_eq!(
            draw_mark(&mut image, W, H, &away, Style::Bubble, [255, 255, 255], 1.0),
            None
        );
        let nan = Mark {
            x: f64::NAN,
            y: 100.0,
            radius: 40.0,
            alpha: 1.0,
        };
        assert_eq!(
            draw_mark(&mut image, W, H, &nan, Style::Bubble, [255, 255, 255], 1.0),
            None
        );
        let faint = Mark {
            alpha: 0.0,
            ..centred(40.0)
        };
        assert_eq!(
            draw_mark(
                &mut image,
                W,
                H,
                &faint,
                Style::Bubble,
                [255, 255, 255],
                1.0
            ),
            None
        );
    }

    #[test]
    fn clearing_a_rectangle_leaves_the_rest() {
        let mut image = canvas();
        let rect = draw_mark(
            &mut image,
            W,
            H,
            &centred(20.0),
            Style::Solid,
            [255, 255, 255],
            1.0,
        )
        .unwrap();
        image[3] = 9;
        clear(&mut image, W, rect);
        assert_eq!(alpha(&image, 100, 100), 0);
        assert_eq!(image[3], 9, "outside the rectangle is untouched");
    }

    #[test]
    fn the_mark_fades_in_at_the_gaze_point_and_out_when_gaze_is_lost() {
        let settings = Overlay {
            trail: 0.0,
            smoothing: 0.0,
            ..Overlay::default()
        };
        let mut motion = Motion::new(settings.smoothing);
        assert!(
            motion.step(0.0, 0.016, &settings).is_empty(),
            "nothing before any gaze"
        );
        assert!(motion.at_rest(0.0));

        motion.on_gaze(valid(0.5, 0.25, 0), (2000, 1000), 0.0);
        let first = motion.step(0.016, 0.016, &settings);
        assert_eq!(first.len(), 1);
        assert!(
            (first[0].x - 1000.0).abs() < 1.0 && (first[0].y - 250.0).abs() < 1.0,
            "{first:?}"
        );
        assert!(
            first[0].alpha > 0.0 && first[0].alpha < 0.5,
            "still fading in"
        );
        assert_eq!(first[0].radius, settings.size / 2.0);

        let mut now = 0.016;
        for _ in 0..12 {
            now += 0.016;
            motion.on_gaze(valid(0.5, 0.25, (now * 1e6) as i64), (2000, 1000), now);
            motion.step(now, 0.016, &settings);
        }
        assert_eq!(motion.visibility, 1.0);

        // No more gaze: after the grace period it fades to nothing.
        let mut marks = Vec::new();
        for _ in 0..80 {
            now += 0.016;
            marks = motion.step(now, 0.016, &settings);
        }
        assert!(marks.is_empty());
        assert!(motion.at_rest(now));
    }

    #[test]
    fn a_moving_gaze_leaves_a_shrinking_fading_trail_and_a_still_one_leaves_none() {
        let settings = Overlay {
            trail: 100.0,
            smoothing: 0.0,
            ..Overlay::default()
        };
        let mut motion = Motion::new(settings.smoothing);
        let mut now = 0.0;
        let mut marks = Vec::new();
        // Sweep left to right across a 2000 pixel screen in one second.
        for frame in 0..60 {
            now = f64::from(frame) * 0.016;
            motion.on_gaze(
                valid(now as f32, 0.5, (now * 1e6) as i64),
                (2000, 1000),
                now,
            );
            marks = motion.step(now, 0.016, &settings);
        }
        assert!(marks.len() > 5, "{} marks", marks.len());
        let head = marks.last().unwrap();
        assert_eq!(head.alpha, 1.0);
        for pair in marks.windows(2) {
            assert!(pair[0].x < pair[1].x, "older marks are further back");
            assert!(pair[0].radius <= pair[1].radius && pair[0].alpha <= pair[1].alpha);
        }
        assert!(marks[0].radius < head.radius * 0.6);

        // Hold still: the trail catches up and disappears.
        for _ in 0..60 {
            now += 0.016;
            motion.on_gaze(valid(0.95, 0.5, (now * 1e6) as i64), (2000, 1000), now);
            marks = motion.step(now, 0.016, &settings);
        }
        assert_eq!(marks.len(), 1, "only the mark itself");
    }

    #[test]
    fn the_mark_glides_between_gaze_samples_instead_of_jumping() {
        let settings = Overlay {
            trail: 0.0,
            smoothing: 0.0,
            ..Overlay::default()
        };
        let mut motion = Motion::new(settings.smoothing);
        motion.on_gaze(valid(0.2, 0.5, 0), (1000, 1000), 0.0);
        motion.step(0.007, 0.007, &settings);
        // One new sample, then several screen refreshes before the next.
        motion.on_gaze(valid(0.3, 0.5, 30_000), (1000, 1000), 0.03);
        let mut last = 200.0;
        let mut now = 0.03;
        for _ in 0..4 {
            now += 0.007;
            let x = motion.step(now, 0.007, &settings)[0].x;
            assert!(x > last && x < 300.0, "{x} after {last}");
            last = x;
        }
        assert!(last > 235.0, "well on the way within a sample: {last}");
    }

    #[test]
    fn after_a_gap_the_mark_appears_at_the_new_place() {
        let settings = Overlay {
            trail: 100.0,
            smoothing: 0.0,
            ..Overlay::default()
        };
        let mut motion = Motion::new(settings.smoothing);
        motion.on_gaze(valid(0.1, 0.1, 0), (1000, 1000), 0.0);
        motion.step(0.016, 0.016, &settings);
        // Two seconds later the eyes are somewhere else entirely.
        motion.on_gaze(valid(0.9, 0.9, 2_000_000), (1000, 1000), 2.0);
        let marks = motion.step(2.016, 0.016, &settings);
        assert_eq!(marks.len(), 1, "no streak across the screen");
        assert!((marks[0].x - 900.0).abs() < 1.0);
    }

    /// Writes a picture of every style, still and in motion, over a dark
    /// and a light background, for judging the look by eye:
    /// `OPENTOBII_PREVIEW=/tmp/x.ppm cargo test -p opentobii preview -- --ignored`
    #[test]
    #[ignore = "writes an image for a person to look at"]
    fn preview() {
        let Some(path) = std::env::var_os("OPENTOBII_PREVIEW") else {
            return;
        };
        let (width, height) = (1200i32, 780i32);
        let mut image = vec![0u8; (width * height * 4) as usize];
        for (row, style) in Style::ALL.into_iter().enumerate() {
            let y = 130.0 + 260.0 * row as f64;
            let settings = Overlay {
                style,
                trail: 80.0,
                smoothing: 0.0,
                ..Overlay::default()
            };
            let rgb = settings.rgb();
            // Still, at three sizes.
            for (x, size) in [(110.0, 48.0), (250.0, 72.0), (430.0, 120.0)] {
                let mark = Mark {
                    x,
                    y,
                    radius: size / 2.0,
                    alpha: 1.0,
                };
                draw_mark(
                    &mut image,
                    width,
                    height,
                    &mark,
                    style,
                    rgb,
                    settings.opacity,
                );
            }
            // In motion: a curved sweep.
            let mut motion = Motion::new(0.0);
            let mut marks = Vec::new();
            for frame in 0..40 {
                let now = f64::from(frame) * 0.007;
                let t = now / 0.28;
                let gaze = valid(
                    (0.55 + 0.38 * t) as f32,
                    ((y + 70.0 * (t * 3.0).sin()) / f64::from(height)) as f32,
                    0,
                );
                motion.on_gaze(
                    GazePoint {
                        micros: (now * 1e6) as i64,
                        ..gaze
                    },
                    (width, height),
                    now,
                );
                marks = motion.step(now, 0.007, &settings);
            }
            for mark in &marks {
                let mark = Mark {
                    alpha: mark.alpha.max(0.0),
                    ..*mark
                };
                draw_mark(
                    &mut image,
                    width,
                    height,
                    &mark,
                    style,
                    rgb,
                    settings.opacity,
                );
            }
        }
        // Composite over a background: dark on the left two thirds of each
        // row's height, a busy light gradient at the far right.
        let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
        for (index, pixel) in image.as_chunks::<4>().0.iter().enumerate() {
            let x = (index as i32) % width;
            let y = (index as i32) / width;
            let light = x > 900;
            let base = if light {
                [200 + (y % 40) as u8, 190, 170 + (x % 50) as u8]
            } else {
                [24, 28 + (y / 40 % 2 * 10) as u8, 36]
            };
            let keep = 255 - u32::from(pixel[3]);
            let mix = |premultiplied: u8, below: u8| {
                (u32::from(premultiplied) + u32::from(below) * keep / 255).min(255) as u8
            };
            ppm.extend_from_slice(&[
                mix(pixel[2], base[0]),
                mix(pixel[1], base[1]),
                mix(pixel[0], base[2]),
            ]);
        }
        std::fs::write(path, ppm).unwrap();
    }

    #[test]
    fn reads_the_drivers_calibration_monitor_file() {
        assert_eq!(
            calibrated_monitor("# The monitor tobii-calibrate last calibrated on.\nDP-4\n")
                .as_deref(),
            Some("DP-4")
        );
        assert_eq!(calibrated_monitor("# only a comment\n\n"), None);
    }
}
