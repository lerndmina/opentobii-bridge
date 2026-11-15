//! The nod prompt on screen: `opentobii banner`.
//!
//! A small panel at the top of the tracker's monitor, above every window,
//! saying that a face was recognised and that a nod approves and a shake
//! denies, with a bar that runs down as the time does. The daemon starts it
//! for one prompt and writes the outcome to its standard input (`approved`,
//! `denied` or `timeout`); it shows that for a moment and exits. If the
//! input closes with no outcome it exits at once.
//!
//! It takes no clicks or keys. Like the gaze mark it needs the layer-shell
//! protocol.

use std::io::BufRead;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::calloop::channel::{self, Event as ChannelEvent};
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

use crate::overlay::driver_monitor;

const WIDTH: i32 = 520;
const HEIGHT: i32 = 116;
/// Gap between the top of the monitor (or a panel there) and the banner.
const TOP_MARGIN: i32 = 28;
/// How long the outcome stays up.
const SHOW_OUTCOME: Duration = Duration::from_millis(1100);

const PANEL: [u8; 3] = [27, 30, 35];
const WHITE: [u8; 3] = [255, 255, 255];
const BODY: [u8; 3] = [205, 210, 218];
const DIM: [u8; 3] = [140, 147, 158];
const ACCENT: [u8; 3] = [96, 205, 255];
const GREEN: [u8; 3] = [108, 203, 95];
const RED: [u8; 3] = [255, 107, 104];

/// What the banner is saying.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Phase {
    /// Waiting for a nod or a shake; `left` of the time remains, 1 to 0.
    Asking {
        left: f32,
    },
    Approved,
    Denied,
    Timeout,
}

impl Phase {
    fn outcome(word: &str) -> Option<Self> {
        match word.trim() {
            "approved" => Some(Self::Approved),
            "denied" => Some(Self::Denied),
            "timeout" => Some(Self::Timeout),
            _ => None,
        }
    }
}

/// Who is asking, in words, from the PAM service's name.
fn asker(service: Option<&str>) -> String {
    match service {
        Some("polkit-1") => "A system dialog is asking for your approval".to_owned(),
        Some(name) => format!("{name} is asking for your approval"),
        None => String::new(),
    }
}

// --- drawing ------------------------------------------------------------------

/// The regular and the bold face of the desktop's sans-serif font.
pub(crate) struct Fonts {
    regular: FontVec,
    bold: FontVec,
}

fn font(pattern: &str) -> Result<FontVec, String> {
    // fontconfig knows which file the desktop's font is; asking it takes a
    // few milliseconds, where reading every installed font would not.
    let found = Command::new("fc-match")
        .args(["-f", "%{file}\n%{index}", pattern])
        .output()
        .map_err(|err| format!("cannot run fc-match to find a font: {err}"))?;
    let text = String::from_utf8_lossy(&found.stdout);
    let mut lines = text.lines();
    let file = lines
        .next()
        .filter(|file| !file.is_empty())
        .ok_or("no font found")?;
    let index = lines
        .next()
        .and_then(|index| index.parse().ok())
        .unwrap_or(0);
    let data = std::fs::read(file).map_err(|err| format!("{file}: {err}"))?;
    FontVec::try_from_vec_and_index(data, index).map_err(|_| format!("{file} is not a usable font"))
}

impl Fonts {
    pub(crate) fn load() -> Result<Self, String> {
        Ok(Self {
            regular: font("sans-serif")?,
            bold: font("sans-serif:bold")?,
        })
    }
}

/// A picture in the compositor's format: four bytes a pixel, blue, green,
/// red, alpha, colour already multiplied by alpha.
struct Canvas<'a> {
    pixels: &'a mut [u8],
    width: i32,
    height: i32,
}

impl Canvas<'_> {
    fn blend(&mut self, x: i32, y: i32, rgb: [u8; 3], alpha: f32) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height || alpha <= 0.0 {
            return;
        }
        let alpha = alpha.min(1.0);
        #[allow(clippy::cast_sign_loss)]
        let at = ((y * self.width + x) * 4) as usize;
        let [r, g, b] = rgb;
        let mix = |under: u8, over: f32| -> u8 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let value = (over * alpha + f32::from(under) * (1.0 - alpha)).round() as u8;
            value
        };
        self.pixels[at] = mix(self.pixels[at], f32::from(b));
        self.pixels[at + 1] = mix(self.pixels[at + 1], f32::from(g));
        self.pixels[at + 2] = mix(self.pixels[at + 2], f32::from(r));
        self.pixels[at + 3] = mix(self.pixels[at + 3], 255.0);
    }

    /// A rectangle with round corners and a soft edge.
    #[allow(clippy::too_many_arguments, clippy::cast_precision_loss)]
    fn rounded(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        radius: f32,
        rgb: [u8; 3],
        alpha: f32,
    ) {
        let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
        let (hw, hh) = ((x1 - x0) / 2.0 - radius, (y1 - y0) / 2.0 - radius);
        #[allow(clippy::cast_possible_truncation)]
        let (left, right, top, bottom) = (
            x0.floor() as i32,
            x1.ceil() as i32,
            y0.floor() as i32,
            y1.ceil() as i32,
        );
        for y in top..bottom {
            for x in left..right {
                let dx = ((x as f32 + 0.5 - cx).abs() - hw).max(0.0);
                let dy = ((y as f32 + 0.5 - cy).abs() - hh).max(0.0);
                let outside = dx.hypot(dy) - radius;
                self.blend(x, y, rgb, alpha * (0.5 - outside).clamp(0.0, 1.0));
            }
        }
    }

    /// One line of text with its left end at `x` and its baseline at `y`.
    fn text(&mut self, font: &FontVec, size: f32, x: f32, y: f32, rgb: [u8; 3], text: &str) {
        let scaled = font.as_scaled(PxScale::from(size));
        let mut pen = x;
        let mut before = None;
        for letter in text.chars() {
            let id = scaled.glyph_id(letter);
            if let Some(before) = before {
                pen += scaled.kern(before, id);
            }
            let glyph = id.with_scale_and_position(size, point(pen, y));
            if let Some(outline) = font.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
                outline.draw(|gx, gy, cover| {
                    self.blend(
                        bounds.min.x as i32 + gx as i32,
                        bounds.min.y as i32 + gy as i32,
                        rgb,
                        cover,
                    );
                });
            }
            pen += scaled.h_advance(id);
            before = Some(id);
        }
    }
}

/// Draw the whole banner into `pixels`, `width` by `height`.
pub(crate) fn paint(
    pixels: &mut [u8],
    width: i32,
    height: i32,
    fonts: &Fonts,
    asker: &str,
    phase: Phase,
) {
    pixels.fill(0);
    let mut canvas = Canvas {
        pixels,
        width,
        height,
    };
    #[allow(clippy::cast_precision_loss)]
    let (w, h) = (width as f32, height as f32);
    // A light rim, then the panel one pixel inside it.
    canvas.rounded(0.0, 0.0, w, h, 14.0, WHITE, 0.16);
    canvas.rounded(1.0, 1.0, w - 1.0, h - 1.0, 13.0, PANEL, 1.0);

    let (title, title_colour, body) = match phase {
        Phase::Asking { .. } => (
            "Face recognised",
            WHITE,
            "Nod to approve. Shake your head to deny.",
        ),
        Phase::Approved => ("Approved", GREEN, "Nod seen."),
        Phase::Denied => (
            "Denied",
            RED,
            "Head shake seen. Enter your password, or cancel.",
        ),
        Phase::Timeout => (
            "No answer",
            DIM,
            "No nod seen. Enter your password, or cancel.",
        ),
    };
    let left = 24.0;
    canvas.text(&fonts.bold, 19.0, left, 37.0, title_colour, title);
    canvas.text(&fonts.regular, 15.0, left, 62.0, BODY, body);
    canvas.text(&fonts.regular, 12.5, left, 84.0, DIM, asker);

    // The time left: a track, and the part of it still to run.
    let (x0, x1, y0, y1) = (left, w - left, h - 16.0, h - 12.0);
    canvas.rounded(x0, y0, x1, y1, 2.0, WHITE, 0.10);
    if let Phase::Asking { left: share } = phase {
        let end = x0 + (x1 - x0) * share.clamp(0.0, 1.0);
        if end - x0 >= 4.0 {
            canvas.rounded(x0, y0, end, y1, 2.0, ACCENT, 1.0);
        }
    }
}

// --- the Wayland side -----------------------------------------------------------

struct App {
    registry: RegistryState,
    outputs: OutputState,
    compositor: CompositorState,
    shm: Shm,
    layer_shell: LayerShell,
    pool: SlotPool,
    queue: QueueHandle<Self>,

    fonts: Fonts,
    asker: String,
    started: Instant,
    seconds: f32,
    /// How the prompt ended, and when the banner learned it.
    outcome: Option<(Phase, Instant)>,
    layer: Option<LayerSurface>,
    pages: Vec<Buffer>,
    ready: bool,
    /// A frame callback is on its way.
    waiting: bool,
    done: Option<Result<(), String>>,
}

impl App {
    fn phase(&self) -> Phase {
        match self.outcome {
            Some((phase, _)) => phase,
            None => Phase::Asking {
                left: 1.0 - self.started.elapsed().as_secs_f32() / self.seconds,
            },
        }
    }

    fn pick_output(&self) -> Option<wl_output::WlOutput> {
        let wanted = driver_monitor();
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

    fn open(&mut self) -> Result<(), String> {
        self.layer = None;
        self.pages.clear();
        self.ready = false;
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
            Some("opentobii-confirm"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP);
        #[allow(clippy::cast_sign_loss)]
        layer.set_size(WIDTH as u32, HEIGHT as u32);
        layer.set_margin(TOP_MARGIN, 0, 0, 0);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.layer = Some(layer);
        Ok(())
    }

    fn draw(&mut self) {
        self.waiting = false;
        if !self.ready {
            return;
        }
        let Some(layer) = self.layer.clone() else {
            return;
        };
        let surface = layer.wl_surface();
        let phase = self.phase();
        let Self {
            pages,
            pool,
            fonts,
            asker,
            ..
        } = self;
        if let Some(page) = pages.iter_mut().find(|page| page.canvas(pool).is_some()) {
            if let Some(canvas) = page.canvas(pool) {
                paint(canvas, WIDTH, HEIGHT, fonts, asker, phase);
            }
            if page.attach_to(surface).is_err() {
                return;
            }
            surface.damage_buffer(0, 0, WIDTH, HEIGHT);
        }
        // Whether drawn or both buffers were busy: come back next refresh,
        // the bar is always moving.
        surface.frame(&self.queue, surface.clone());
        self.waiting = true;
        layer.commit();
    }

    /// Things that happen with time, looked at a few times a second.
    fn tick(&mut self) {
        match self.outcome {
            Some((_, since)) if since.elapsed() >= SHOW_OUTCOME => {
                self.done.get_or_insert(Ok(()));
            }
            // The daemon ends the prompt at its time limit; this is for a
            // daemon that has stopped answering.
            None if self.started.elapsed().as_secs_f32() > self.seconds + 3.0 => {
                self.done.get_or_insert(Ok(()));
            }
            _ => {}
        }
        if !self.waiting {
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
        if self.open().is_err() {
            self.done = Some(Err("the monitor went away".into()));
        }
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.done = Some(Err("the compositor closed the banner".into()));
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        _: LayerSurfaceConfigure,
        _: u32,
    ) {
        if self.pages.is_empty() {
            for _ in 0..2 {
                match self
                    .pool
                    .create_buffer(WIDTH, HEIGHT, WIDTH * 4, wl_shm::Format::Argb8888)
                {
                    Ok((buffer, _)) => self.pages.push(buffer),
                    Err(_) => {
                        self.done = Some(Err("cannot allocate the banner's buffers".into()));
                        return;
                    }
                }
            }
        }
        self.ready = true;
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

/// `--seconds N` and `--service NAME`.
fn options(mut args: impl Iterator<Item = String>) -> Result<(f32, Option<String>), String> {
    let mut seconds = 15.0;
    let mut service = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => {
                let value = args.next().ok_or("--seconds needs a number")?;
                seconds = value
                    .parse()
                    .ok()
                    .filter(|seconds: &f32| seconds.is_finite() && *seconds > 0.0)
                    .ok_or_else(|| format!("--seconds: `{value}` is not a time"))?;
            }
            "--service" => service = Some(args.next().ok_or("--service needs a name")?),
            other => return Err(format!("banner: unknown option `{other}`")),
        }
    }
    Ok((seconds, service))
}

/// Show the banner until told the outcome on standard input.
pub fn run(args: impl Iterator<Item = String>) -> Result<(), String> {
    let (seconds, service) = options(args)?;
    let fonts = Fonts::load()?;

    let connection = Connection::connect_to_env()
        .map_err(|err| format!("no Wayland display to draw on: {err}"))?;
    let (globals, mut events) =
        registry_queue_init::<App>(&connection).map_err(|err| err.to_string())?;
    let queue = events.handle();
    let compositor = CompositorState::bind(&globals, &queue).map_err(|err| err.to_string())?;
    let layer_shell = LayerShell::bind(&globals, &queue).map_err(|_| {
        "this desktop does not offer the layer-shell protocol the banner needs \
         (KDE Plasma, Sway, Hyprland and COSMIC do)"
            .to_owned()
    })?;
    let shm = Shm::bind(&globals, &queue).map_err(|err| err.to_string())?;
    #[allow(clippy::cast_sign_loss)]
    let pool =
        SlotPool::new((WIDTH * HEIGHT * 4 * 2) as usize, &shm).map_err(|err| err.to_string())?;

    let mut app = App {
        registry: RegistryState::new(&globals),
        outputs: OutputState::new(&globals, &queue),
        compositor,
        shm,
        layer_shell,
        pool,
        queue,
        fonts,
        asker: asker(service.as_deref()),
        started: Instant::now(),
        seconds,
        outcome: None,
        layer: None,
        pages: Vec::new(),
        ready: false,
        waiting: false,
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
    let (told_tx, told_rx) = channel::channel::<Phase>();
    handle
        .insert_source(told_rx, |event, (), app: &mut App| match event {
            ChannelEvent::Msg(phase) => {
                app.outcome.get_or_insert((phase, Instant::now()));
            }
            // The daemon closed the pipe. With an outcome showing, that
            // stays up for its moment; with none, there is nothing to show.
            ChannelEvent::Closed => {
                if app.outcome.is_none() {
                    app.done.get_or_insert(Ok(()));
                }
            }
        })
        .map_err(|err| err.to_string())?;
    thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if let Some(phase) = Phase::outcome(&line) {
                if told_tx.send(phase).is_err() {
                    return;
                }
            }
        }
    });

    loop {
        event_loop
            .dispatch(Some(Duration::from_millis(100)), &mut app)
            .map_err(|err| err.to_string())?;
        app.tick();
        if let Some(result) = app.done.take() {
            return result;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_are_three_words_and_nothing_else() {
        assert_eq!(Phase::outcome("approved\n"), Some(Phase::Approved));
        assert_eq!(Phase::outcome("denied"), Some(Phase::Denied));
        assert_eq!(Phase::outcome("timeout"), Some(Phase::Timeout));
        assert_eq!(Phase::outcome("unavailable"), None);
        assert_eq!(Phase::outcome(""), None);
    }

    #[test]
    fn the_asker_is_named_in_words() {
        assert_eq!(
            asker(Some("polkit-1")),
            "A system dialog is asking for your approval"
        );
        assert_eq!(asker(Some("sudo")), "sudo is asking for your approval");
        assert_eq!(asker(None), "");
    }

    #[test]
    fn options_are_read_and_bad_ones_refused() {
        let words = |text: &str| {
            text.split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
                .into_iter()
        };
        assert_eq!(options(words("")), Ok((15.0, None)));
        assert_eq!(
            options(words("--seconds 8 --service sudo")),
            Ok((8.0, Some("sudo".into())))
        );
        assert!(options(words("--seconds 0")).is_err());
        assert!(options(words("--seconds soon")).is_err());
        assert!(options(words("--loud")).is_err());
    }

    fn alpha(pixels: &[u8], x: i32, y: i32) -> u8 {
        #[allow(clippy::cast_sign_loss)]
        pixels[((y * WIDTH + x) * 4 + 3) as usize]
    }

    /// Needs a font on the machine, as the banner itself does.
    #[test]
    fn the_panel_is_opaque_inside_clear_at_the_corners_and_the_bar_runs_down() {
        let Ok(fonts) = Fonts::load() else {
            eprintln!("no font found; skipping");
            return;
        };
        #[allow(clippy::cast_sign_loss)]
        let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
        let bar = |pixels: &[u8]| -> usize {
            (24..WIDTH - 24)
                .filter(|&x| {
                    #[allow(clippy::cast_sign_loss)]
                    let at = (((HEIGHT - 14) * WIDTH + x) * 4) as usize;
                    // The accent colour: far more blue than red.
                    pixels[at] > 200 && pixels[at + 2] < 140
                })
                .count()
        };
        paint(
            &mut pixels,
            WIDTH,
            HEIGHT,
            &fonts,
            "sudo",
            Phase::Asking { left: 1.0 },
        );
        assert_eq!(alpha(&pixels, 0, 0), 0);
        assert_eq!(alpha(&pixels, WIDTH - 1, HEIGHT - 1), 0);
        assert_eq!(alpha(&pixels, WIDTH / 2, HEIGHT / 2), 255);
        let full = bar(&pixels);
        assert!(full > 400, "{full}");
        // Some of the title's pixels are white.
        let lit = (20..220)
            .flat_map(|x| (20..40).map(move |y| (x, y)))
            .filter(|&(x, y)| {
                #[allow(clippy::cast_sign_loss)]
                let at = ((y * WIDTH + x) * 4) as usize;
                pixels[at] > 230 && pixels[at + 1] > 230 && pixels[at + 2] > 230
            })
            .count();
        assert!(lit > 100, "{lit}");

        paint(
            &mut pixels,
            WIDTH,
            HEIGHT,
            &fonts,
            "sudo",
            Phase::Asking { left: 0.5 },
        );
        let half = bar(&pixels);
        assert!(half.abs_diff(full / 2) <= 3, "{half} of {full}");
        paint(&mut pixels, WIDTH, HEIGHT, &fonts, "sudo", Phase::Approved);
        assert_eq!(bar(&pixels), 0);
    }

    #[test]
    #[ignore = "writes an image for a person to look at"]
    fn preview() {
        let Some(path) = std::env::var_os("OPENTOBII_PREVIEW") else {
            return;
        };
        let fonts = Fonts::load().unwrap();
        let phases = [
            Phase::Asking { left: 0.7 },
            Phase::Approved,
            Phase::Denied,
            Phase::Timeout,
        ];
        let gap = 20;
        let (width, height) = (WIDTH + 2 * gap, (HEIGHT + gap) * 4 + gap);
        // A mid-grey desk to judge the edges against.
        let mut image = vec![90u8; (width * height * 3) as usize];
        #[allow(clippy::cast_sign_loss)]
        let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
        for (row, phase) in phases.into_iter().enumerate() {
            let asked = asker(Some(if row % 2 == 0 { "polkit-1" } else { "sudo" }));
            paint(&mut pixels, WIDTH, HEIGHT, &fonts, &asked, phase);
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let top = gap + (HEIGHT + gap) * row as i32;
            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    #[allow(clippy::cast_sign_loss)]
                    let from = ((y * WIDTH + x) * 4) as usize;
                    #[allow(clippy::cast_sign_loss)]
                    let to = (((top + y) * width + gap + x) * 3) as usize;
                    let a = u32::from(pixels[from + 3]);
                    for channel in 0..3 {
                        let over = u32::from(pixels[from + 2 - channel]);
                        let under = u32::from(image[to + channel]);
                        #[allow(clippy::cast_possible_truncation)]
                        let mixed = (over + under * (255 - a) / 255).min(255) as u8;
                        image[to + channel] = mixed;
                    }
                }
            }
        }
        let mut file = format!("P6\n{width} {height}\n255\n").into_bytes();
        file.extend_from_slice(&image);
        std::fs::write(path, file).unwrap();
    }
}
