//! The gaze overlay: `opentobii overlay`.
//!
//! Draws a mark on screen where the user is looking. It is a client of the
//! daemon's control socket for its settings and the gaze feed, and a Wayland
//! client for the drawing.
//!
//! The mark is one small surface on the compositor's overlay layer, above
//! every window including a full-screen game. It is anchored to the top-left
//! corner of the monitor and moved by changing its margins, so following the
//! eyes costs no redrawing. Its input region is empty: clicks and keys go to
//! whatever is underneath. This needs the layer-shell protocol, which KDE
//! Plasma, Sway, Hyprland and COSMIC offer.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::thread;
use std::time::Duration;

use opentobii_core::control::{self, GazePoint, Line, Request};
use opentobii_core::overlay::{GazeFilter, Overlay, Style};
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

/// No valid gaze for this long and the mark is hidden.
const HIDE_AFTER_MICROS: i64 = 400_000;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

// --- drawing, tested without a display ---------------------------------------

/// 0 below `edge0`, 1 above `edge1`, smooth in between.
fn smoothstep(edge0: f64, edge1: f64, x: f64) -> f64 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// How opaque the mark is at `r`, a distance from its centre as a fraction
/// of its radius (0 at the centre, 1 at the edge), before the opacity
/// setting is applied. `pixel` is one pixel in the same units, for the
/// anti-aliased edges.
fn coverage(style: Style, r: f64, pixel: f64) -> f64 {
    let inside = 1.0 - smoothstep(1.0 - pixel, 1.0, r);
    match style {
        Style::Bubble => {
            // A fill that fades towards the edge, and a faint ring on it.
            let fill = (1.0 - r * r).max(0.0).powf(0.8) * 0.75;
            let ring = band(r, 0.93, 0.035, pixel) * 0.9;
            fill.max(ring) * inside
        }
        Style::Ring => band(r, 0.9, 0.06, pixel),
        Style::Dot => inside,
    }
}

/// A ring centred on radius `at`, `half_width` either side.
fn band(r: f64, at: f64, half_width: f64, pixel: f64) -> f64 {
    let outer = 1.0 - smoothstep(at + half_width - pixel, at + half_width, r);
    let inner = smoothstep(at - half_width - pixel, at - half_width, r);
    outer * inner
}

/// The mark as a square of `side` pixels: premultiplied ARGB8888 in the
/// byte order Wayland's shared-memory buffers use (blue, green, red, alpha).
fn render(settings: &Overlay, side: u32) -> Vec<u8> {
    let [red, green, blue] = settings.rgb();
    let radius = f64::from(side) / 2.0;
    let pixel = 1.0 / radius.max(1.0);
    let mut out = Vec::with_capacity((side as usize) * (side as usize) * 4);
    for y in 0..side {
        for x in 0..side {
            let dx = f64::from(x) + 0.5 - radius;
            let dy = f64::from(y) + 0.5 - radius;
            let r = (dx * dx + dy * dy).sqrt() / radius;
            let alpha = (coverage(settings.style, r, pixel) * settings.opacity).clamp(0.0, 1.0);
            let channel = |value: u8| (f64::from(value) * alpha).round() as u8;
            out.extend_from_slice(&[
                channel(blue),
                channel(green),
                channel(red),
                (alpha * 255.0).round() as u8,
            ]);
        }
    }
    out
}

/// Where the surface's top-left corner goes so the mark is centred on a
/// gaze point given as fractions of a screen `width` by `height`.
fn corner(gaze: [f32; 2], screen: (i32, i32), side: u32) -> (i32, i32) {
    let half = f64::from(side) / 2.0;
    let place = |fraction: f32, extent: i32| {
        let centre = f64::from(fraction.clamp(0.0, 1.0)) * f64::from(extent);
        (centre - half).round() as i32
    };
    (place(gaze[0], screen.0), place(gaze[1], screen.1))
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

struct App {
    registry: RegistryState,
    outputs: OutputState,
    compositor: CompositorState,
    shm: Shm,
    layer_shell: LayerShell,
    pool: SlotPool,
    queue: QueueHandle<Self>,

    settings: Overlay,
    filter: GazeFilter,
    layer: Option<LayerSurface>,
    /// The buffer on screen, kept alive while the compositor shows it.
    shown: Option<Buffer>,
    /// The surface has had its first configure and can take a buffer.
    configured: bool,
    visible: bool,
    last_valid_micros: Option<i64>,
    screen: (i32, i32),
    side: u32,
    done: Option<Result<(), String>>,
}

impl App {
    /// The output to draw on: the one named in the settings, else the one
    /// the driver was calibrated on, else the first.
    fn pick_output(&self) -> Option<(wl_output::WlOutput, (i32, i32))> {
        let wanted = self.settings.monitor.clone().or_else(driver_monitor);
        let mut first = None;
        for output in self.outputs.outputs() {
            let Some(info) = self.outputs.info(&output) else {
                continue;
            };
            let size = info.logical_size.or_else(|| {
                let mode = info.modes.iter().find(|mode| mode.current)?;
                let scale = info.scale_factor.max(1);
                Some((mode.dimensions.0 / scale, mode.dimensions.1 / scale))
            });
            let Some(size) = size else {
                continue;
            };
            if wanted.is_some() && info.name == wanted {
                return Some((output, size));
            }
            first.get_or_insert((output, size));
        }
        first
    }

    /// Make (or remake) the surface for the current settings.
    fn open(&mut self) -> Result<(), String> {
        self.layer = None;
        self.shown = None;
        self.configured = false;
        self.visible = false;
        let (output, screen) = self.pick_output().ok_or("no monitor to draw on")?;
        self.screen = screen;
        self.side = self.settings.size.round().clamp(1.0, 4096.0) as u32;

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
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        // -1: do not make room for panels, so margins count from the corner
        // of the monitor itself.
        layer.set_exclusive_zone(-1);
        layer.set_size(self.side, self.side);
        layer.set_margin(0, 0, 0, 0);
        // The first commit has no buffer; the compositor answers with a
        // configure, and drawing starts there.
        layer.commit();
        self.layer = Some(layer);
        Ok(())
    }

    /// Put a buffer on the surface: the mark, or nothing at all.
    fn present(&mut self, visible: bool) {
        let Some(layer) = &self.layer else {
            return;
        };
        if !self.configured {
            return;
        }
        let side = self.side as i32;
        let Ok((buffer, canvas)) =
            self.pool
                .create_buffer(side, side, side * 4, wl_shm::Format::Argb8888)
        else {
            return;
        };
        if visible {
            canvas.copy_from_slice(&render(&self.settings, self.side));
        } else {
            canvas.fill(0);
        }
        let surface = layer.wl_surface();
        surface.damage_buffer(0, 0, side, side);
        if buffer.attach_to(surface).is_ok() {
            layer.commit();
            // The previous buffer is released once this one is shown.
            self.shown = Some(buffer);
            self.visible = visible;
        }
    }

    fn on_gaze(&mut self, gaze: GazePoint) {
        if gaze.valid {
            self.last_valid_micros = Some(gaze.micros);
            let point = self.filter.filter(gaze.micros, [gaze.x, gaze.y]);
            if let Some(layer) = &self.layer {
                let (left, top) = corner(point, self.screen, self.side);
                layer.set_margin(top, 0, 0, left);
                if self.visible {
                    layer.commit();
                }
            }
            if !self.visible {
                self.present(true);
            }
        } else if self.visible {
            let stale = self
                .last_valid_micros
                .is_none_or(|at| gaze.micros - at > HIDE_AFTER_MICROS);
            if stale {
                self.filter.reset();
                self.present(false);
            }
        }
    }

    fn on_settings(&mut self, settings: Overlay) {
        if !settings.enabled {
            self.done = Some(Ok(()));
            return;
        }
        let reopen =
            settings.monitor != self.settings.monitor || settings.size != self.settings.size;
        let redraw = settings.style != self.settings.style
            || settings.color != self.settings.color
            || settings.opacity != self.settings.opacity;
        if settings.smoothing != self.settings.smoothing {
            self.filter = GazeFilter::new(settings.smoothing);
        }
        self.settings = settings;
        if reopen {
            if let Err(reason) = self.open() {
                self.done = Some(Err(reason));
            }
        } else if redraw && self.visible {
            self.present(true);
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
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
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
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        self.configured = true;
        // Start hidden: the mark appears with the first valid gaze.
        self.present(false);
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
    let pool = SlotPool::new(256 * 256 * 4, &shm).map_err(|err| err.to_string())?;

    let mut app = App {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &queue),
        compositor,
        shm,
        layer_shell,
        pool,
        queue,
        filter: GazeFilter::new(settings.smoothing),
        settings,
        layer: None,
        shown: None,
        configured: false,
        visible: false,
        last_valid_micros: None,
        screen: (0, 0),
        side: 0,
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

    fn alpha_at(image: &[u8], side: u32, x: u32, y: u32) -> u8 {
        image[((y * side + x) * 4 + 3) as usize]
    }

    fn settings(style: Style) -> Overlay {
        Overlay {
            style,
            opacity: 1.0,
            color: "#ff8000".into(),
            ..Overlay::default()
        }
    }

    #[test]
    fn every_style_is_clear_at_the_corners_and_the_right_size() {
        for style in Style::ALL {
            let image = render(&settings(style), 100);
            assert_eq!(image.len(), 100 * 100 * 4);
            for (x, y) in [(0, 0), (99, 0), (0, 99), (99, 99)] {
                assert_eq!(alpha_at(&image, 100, x, y), 0, "{style:?} at {x},{y}");
            }
        }
    }

    #[test]
    fn a_bubble_is_fullest_in_the_middle_and_a_ring_is_hollow() {
        let bubble = render(&settings(Style::Bubble), 100);
        assert!(alpha_at(&bubble, 100, 50, 50) > 150);
        assert!(alpha_at(&bubble, 100, 50, 50) > alpha_at(&bubble, 100, 85, 50));
        assert!(
            alpha_at(&bubble, 100, 96, 50) > alpha_at(&bubble, 100, 90, 50),
            "the ring at the edge"
        );

        let ring = render(&settings(Style::Ring), 100);
        assert_eq!(alpha_at(&ring, 100, 50, 50), 0);
        assert!(alpha_at(&ring, 100, 95, 50) > 200);

        let dot = render(&settings(Style::Dot), 100);
        assert_eq!(alpha_at(&dot, 100, 50, 50), 255);
        assert_eq!(alpha_at(&dot, 100, 90, 50), 255);
    }

    #[test]
    fn pixels_are_premultiplied_in_blue_green_red_alpha_order() {
        let half = Overlay {
            opacity: 0.5,
            ..settings(Style::Dot)
        };
        let image = render(&half, 40);
        let centre = ((20 * 40 + 20) * 4) as usize;
        // #ff8000 at half opacity.
        assert_eq!(&image[centre..centre + 4], &[0, 64, 128, 128]);
        for pixel in image.as_chunks::<4>().0 {
            assert!(pixel[0] <= pixel[3] && pixel[1] <= pixel[3] && pixel[2] <= pixel[3]);
        }
    }

    #[test]
    fn opacity_scales_the_whole_mark() {
        let solid = render(&settings(Style::Bubble), 60);
        let faint = render(
            &Overlay {
                opacity: 0.25,
                ..settings(Style::Bubble)
            },
            60,
        );
        let at = alpha_at(&solid, 60, 30, 30);
        assert!((i32::from(alpha_at(&faint, 60, 30, 30)) - i32::from(at) / 4).abs() <= 1);
    }

    #[test]
    fn the_mark_is_centred_on_the_gaze_point() {
        assert_eq!(corner([0.5, 0.5], (2560, 1440), 160), (1200, 640));
        assert_eq!(
            corner([0.0, 0.0], (2560, 1440), 160),
            (-80, -80),
            "half off the corner"
        );
        assert_eq!(corner([1.0, 1.0], (2560, 1440), 160), (2480, 1360));
        assert_eq!(
            corner([-3.0, 9.0], (2560, 1440), 160),
            (-80, 1360),
            "clamped to the screen"
        );
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
