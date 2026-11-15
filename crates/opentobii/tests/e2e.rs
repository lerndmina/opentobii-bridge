//! End to end on throwaway ports: a fake `tobiid`, a fake game DLL on the
//! TTP side and a fake helper on the services side.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use opentobii::daemon::{monotonic_micros, start, Config, Running};
use opentobii_core::config::Config as Tuning;
use opentobii_core::control::{Answer, Line, Request};
use opentobii_core::overlay::{Overlay, Style};
use opentobii_core::pipeline::{Axis, Profile, Smoothing};
use opentobii_proto::frames::{display_area_corners, keyed_values, Keyed};
use opentobii_proto::sesp;
use opentobii_proto::tobiid::testing::{gaze_raw_body, head_pose_body, presence_body};
use opentobii_proto::tobiid::{self, GazeRaw, HeadPose, RawEye};
use opentobii_proto::ttp::{self, Message};

const AREA: [[f32; 3]; 3] = [
    [-300.0, 340.0, 100.0],
    [300.0, 340.0, 100.0],
    [-300.0, 5.0, -2.0],
];
const QPC: u64 = 5_000_000_000;
const TIMEOUT: Duration = Duration::from_secs(3);

struct Rig {
    running: Running,
    /// The fake driver's end of the daemon's connection.
    driver: UnixStream,
    /// What the daemon sent the driver first.
    greeting: Vec<u8>,
    _dir: TempDir,
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Settings with smoothing off, so a test sees the values it sent.
fn unsmoothed() -> Tuning {
    let mut tuning = Tuning::default();
    tuning.default.smoothing.enabled = false;
    tuning
}

fn rig(tuning: Tuning) -> Rig {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("opentobii-e2e-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("tobiid.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        // SUBSCRIBE (4 + 5 bytes) then the display area REQUEST (4 + 6).
        let mut greeting = vec![0u8; 19];
        stream.read_exact(&mut greeting).unwrap();
        stream
            .write_all(&tobiid::frame(&[tobiid::TAG_SUBSCRIBED, 1]))
            .unwrap();
        let mut reply = vec![tobiid::TAG_REPLY, 1, 0, 0, 0, tobiid::STATUS_OK];
        reply.extend(AREA.iter().flatten().flat_map(|v| v.to_le_bytes()));
        stream.write_all(&tobiid::frame(&reply)).unwrap();
        tx.send((stream, greeting)).unwrap();
    });

    let any = SocketAddr::from(([127, 0, 0, 1], 0));
    let running = start(Config {
        ttp_addr: any,
        relay_addr: any,
        tobiid_socket: socket,
        tuning,
        force_unsmoothed: false,
        settings_path: Some(dir.join("config.toml")),
        control_socket: Some(dir.join("opentobii.sock")),
        monitor_index: None,
        manage_overlay: false,
        confirm_banner: false,
    })
    .unwrap();
    let (driver, greeting) = rx
        .recv_timeout(TIMEOUT)
        .expect("the daemon connects to the driver");
    let deadline = Instant::now() + TIMEOUT;
    while !running.has_display_area() {
        assert!(
            Instant::now() < deadline,
            "the daemon takes the display area"
        );
        thread::sleep(Duration::from_millis(5));
    }
    Rig {
        running,
        driver,
        greeting,
        _dir: TempDir(dir),
    }
}

fn connect(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    stream
}

fn read_ttp(stream: &mut TcpStream) -> Message {
    let mut head = [0u8; ttp::HEADER_LEN];
    stream.read_exact(&mut head).unwrap();
    let length = ttp::Header::decode(&head).unwrap().payload_len as usize;
    let mut all = head.to_vec();
    all.resize(ttp::HEADER_LEN + length, 0);
    stream.read_exact(&mut all[ttp::HEADER_LEN..]).unwrap();
    Message::decode(&all).unwrap()
}

fn command(stream: &mut TcpStream, seq: u32, id: u32, payload: Vec<u8>) -> Message {
    let request = Message {
        marker: ttp::MARKER_COMMAND,
        seq,
        status: 0,
        id,
        error: 0,
        payload,
    };
    stream.write_all(&request.encode()).unwrap();
    read_ttp(stream)
}

fn read_sesp(stream: &mut TcpStream) -> Vec<u8> {
    let mut head = [0u8; sesp::FRAME_HEAD_LEN];
    stream.read_exact(&mut head).unwrap();
    let size = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
    let mut buffer = vec![0u8; size];
    stream.read_exact(&mut buffer).unwrap();
    buffer
}

fn helper_hello(channel: u32, slot: u32) -> Vec<u8> {
    let monitor = b"DISPLAY\\TEST\\1";
    let mut hello = b"OTB1".to_vec();
    for word in [channel, 392, slot] {
        hello.extend_from_slice(&word.to_le_bytes());
    }
    hello.extend_from_slice(&QPC.to_le_bytes());
    hello.extend_from_slice(&1u32.to_le_bytes());
    hello.push(1);
    hello.extend_from_slice(&(monitor.len() as u16).to_le_bytes());
    hello.extend_from_slice(monitor);
    hello
}

fn sample(host_micros: i64) -> GazeRaw {
    let eye = RawEye {
        gaze_origin_mm: [-30.0, 20.0, 650.0],
        gaze_origin_in_track_box: [0.5, 0.5, 0.5],
        gaze_point_mm: [10.0, 100.0, 50.0],
        gaze_point_on_display: [0.25, 0.75],
        pupil_diameter_mm: 3.5,
        status: 0,
    };
    GazeRaw {
        device_micros: 1,
        left: eye,
        right: eye,
        combined_on_display: [0.125, 0.875],
        combined_validity: 1,
        key_11: Some(4),
        frame_counter: Some(99),
        left_origin_flag: Some(1),
        right_origin_flag: Some(1),
        host_micros: Some(host_micros),
        responsive_on_display: Some([0.125, 0.875]),
        responsive_validity: 1,
        ..GazeRaw::default()
    }
}

#[test]
fn subscribes_and_asks_for_the_display_area() {
    let rig = rig(unsmoothed());
    let streams = tobiid::STREAM_GAZE_RAW
        | tobiid::STREAM_PRESENCE
        | tobiid::STREAM_HEAD_POSE
        | tobiid::STREAM_NOTIFICATIONS;
    let mut expected = tobiid::frame(&tobiid::subscribe(streams));
    expected.extend(tobiid::frame(&tobiid::request(
        1,
        tobiid::KIND_DISPLAY_AREA_GET,
        &[],
    )));
    assert_eq!(rig.greeting, expected);
}

#[test]
fn the_game_is_told_the_drivers_display_area_and_recorded_identity() {
    let rig = rig(unsmoothed());
    let mut game = connect(rig.running.ttp_addr);
    let area = command(&mut game, 1, 1430, Vec::new());
    assert_eq!(
        (area.marker, area.seq, area.error),
        (ttp::MARKER_RESPONSE, 1, 0)
    );
    assert_eq!(
        display_area_corners(&area.payload),
        Some(AREA.map(|c| c.map(f64::from)))
    );

    let identity = command(&mut game, 2, 1420, Vec::new());
    assert_eq!(identity.error, 0);
    assert!(!identity.payload.is_empty());

    let unlock = command(&mut game, 3, 1600, Vec::new());
    assert_eq!((unlock.error, unlock.payload.len()), (0, 29));

    let unknown = command(&mut game, 4, 1460, Vec::new());
    assert_eq!(unknown.error, ttp::ERROR_UNSUPPORTED);
}

#[test]
fn live_gaze_and_presence_reach_a_subscribed_game() {
    let mut rig = rig(unsmoothed());
    let mut game = connect(rig.running.ttp_addr);
    for (seq, stream) in [(1, 0x504u32), (2, 0x500)] {
        let reply = command(
            &mut game,
            seq,
            1220,
            ttp::payload(&[&ttp::u32_item(stream)]),
        );
        assert_eq!(reply.error, 0);
    }
    // The subscription is registered after its reply is written.
    thread::sleep(Duration::from_millis(50));

    let host = monotonic_micros();
    rig.driver
        .write_all(&tobiid::frame(&presence_body(
            host,
            tobiid::PRESENCE_PRESENT,
        )))
        .unwrap();
    rig.driver
        .write_all(&tobiid::frame(&gaze_raw_body(&sample(host))))
        .unwrap();

    let presence = read_ttp(&mut game);
    assert_eq!((presence.marker, presence.id), (ttp::MARKER_STREAM, 0x504));
    assert_eq!(keyed_values(&presence.payload)[&2], Keyed::Int(2));

    let gaze = read_ttp(&mut game);
    assert_eq!((gaze.marker, gaze.id), (ttp::MARKER_STREAM, 0x500));
    let values = keyed_values(&gaze.payload);
    assert_eq!(values[&0x1C], Keyed::Point(vec![0.125, 0.875]));
    assert_eq!(values[&0x14], Keyed::Int(99));
    // No helper has said hello, so the host clock is passed on as it is.
    assert_eq!(values[&0x01], Keyed::Int(host as u64));
    // The eye origin is placed in the display frame of the driver's area.
    let Keyed::Point(origin) = &values[&0x22] else {
        panic!("0x22 is a point")
    };
    assert!((origin[0] - -30.0).abs() < 1e-3, "{origin:?}");
    let deadline = Instant::now() + TIMEOUT;
    while rig
        .running
        .stats()
        .gaze_frames
        .load(std::sync::atomic::Ordering::Relaxed)
        != 1
    {
        assert!(Instant::now() < deadline, "the gaze frame is counted");
        thread::sleep(Duration::from_millis(2));
    }

    // After a stream stop, no more gaze frames.
    assert_eq!(
        command(&mut game, 3, 1230, ttp::payload(&[&ttp::u32_item(0x500)])).error,
        0
    );
    thread::sleep(Duration::from_millis(50));
    rig.driver
        .write_all(&tobiid::frame(&gaze_raw_body(&sample(host))))
        .unwrap();
    game.set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    assert!(
        game.read(&mut [0u8; 1]).is_err(),
        "nothing arrives after the stop"
    );
}

#[test]
fn live_head_pose_reaches_the_game_on_the_games_clock() {
    let mut rig = rig(unsmoothed());
    let mut commands = connect(rig.running.relay_addr);
    commands
        .write_all(&helper_hello(sesp::CHANNEL_COMMAND, 1))
        .unwrap();
    let mut subscription = connect(rig.running.relay_addr);
    subscription
        .write_all(&helper_hello(sesp::CHANNEL_SUBSCRIPTION, 2))
        .unwrap();

    commands
        .write_all(&sesp::frame(&sesp::build(
            1,
            sesp::T_DISPLAY_INFO,
            sesp::Table::new(),
        )))
        .unwrap();
    let info = read_sesp(&mut commands);
    let info = sesp::parse(&info).unwrap();
    assert_eq!(info.kind, sesp::T_DISPLAY_INFO_RESPONSE);
    assert_eq!(
        info.payload.unwrap().string(1).as_deref(),
        Some("DISPLAY\\TEST\\1")
    );

    commands
        .write_all(&sesp::frame(&sesp::build(
            2,
            sesp::T_HEAD_POSE_START,
            sesp::Table::new(),
        )))
        .unwrap();
    let started = read_sesp(&mut commands);
    assert_eq!(
        sesp::parse(&started).unwrap().kind,
        sesp::T_HEAD_POSE_START_RESPONSE
    );
    thread::sleep(Duration::from_millis(50));

    let host = monotonic_micros();
    let pose = |valid: bool| HeadPose {
        host_micros: host,
        position_valid: true,
        position_mm: [10.0, 20.0, 600.0],
        rotation_valid: [valid; 3],
        rotation_rad: [0.1, -0.2, 0.05],
    };
    rig.driver
        .write_all(&tobiid::frame(&head_pose_body(&pose(false))))
        .unwrap();
    rig.driver
        .write_all(&tobiid::frame(&head_pose_body(&pose(true))))
        .unwrap();

    let message = read_sesp(&mut subscription);
    let parsed = sesp::parse(&message).unwrap();
    assert_eq!((parsed.kind, parsed.sequence), (sesp::T_HEAD_POSE, 1));
    let payload = parsed.payload.unwrap();
    assert_eq!([2, 3, 4].map(|n| payload.f32(n, 0.0)), [10.0, 20.0, 600.0]);
    assert_eq!(payload.f32(6, 0.0), -0.2);
    let micros = (u64::from(payload.u32(0, 0)) << 32) | u64::from(payload.u32(1, 0));
    // The helper said the game's counter read QPC when it connected.
    assert!(micros.abs_diff(QPC) < 2_000_000, "{micros}");
    let stats = rig.running.stats();
    assert_eq!(
        stats
            .poses_dropped
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    // The daemon counts a pose after writing it, so the message can be here
    // a moment before the count is.
    let deadline = Instant::now() + TIMEOUT;
    while stats.poses_sent.load(std::sync::atomic::Ordering::Relaxed) != 1 {
        assert!(Instant::now() < deadline, "the pose is counted");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn smoothing_steadies_a_noisy_pose_before_it_reaches_the_game() {
    let mut rig = rig(Tuning::default());
    let mut commands = connect(rig.running.relay_addr);
    commands
        .write_all(&helper_hello(sesp::CHANNEL_COMMAND, 1))
        .unwrap();
    let mut subscription = connect(rig.running.relay_addr);
    subscription
        .write_all(&helper_hello(sesp::CHANNEL_SUBSCRIPTION, 2))
        .unwrap();
    commands
        .write_all(&sesp::frame(&sesp::build(
            1,
            sesp::T_HEAD_POSE_START,
            sesp::Table::new(),
        )))
        .unwrap();
    read_sesp(&mut commands);
    thread::sleep(Duration::from_millis(50));

    // Yaw alternating half a degree either side of zero, 30 ms apart.
    let host = monotonic_micros();
    let swing = 0.5f32.to_radians();
    let mut seen = Vec::new();
    for i in 0..40i64 {
        let yaw = if i % 2 == 0 { swing } else { -swing };
        let pose = HeadPose {
            host_micros: host + i * 30_200,
            position_valid: true,
            position_mm: [0.0, 0.0, 700.0],
            rotation_valid: [true; 3],
            rotation_rad: [0.0, yaw, 0.0],
        };
        rig.driver
            .write_all(&tobiid::frame(&head_pose_body(&pose)))
            .unwrap();
        let message = read_sesp(&mut subscription);
        seen.push(sesp::parse(&message).unwrap().payload.unwrap().f32(6, 0.0));
    }
    let late_swing = seen[20..].iter().fold(0.0f32, |max, v| max.max(v.abs()));
    assert!(late_swing < swing / 2.0, "swing {late_swing} of {swing}");
}

#[test]
fn garbage_from_a_client_closes_that_connection_only() {
    let rig = rig(unsmoothed());
    let mut bad = connect(rig.running.ttp_addr);
    let mut header = Vec::new();
    for word in [ttp::MARKER_COMMAND, 1, 0, 1000, 0, u32::MAX] {
        header.extend_from_slice(&word.to_be_bytes());
    }
    bad.write_all(&header).unwrap();
    assert_eq!(
        bad.read(&mut [0u8; 1]).unwrap_or(0),
        0,
        "an oversized command is hung up on"
    );

    let mut bad_helper = connect(rig.running.relay_addr);
    bad_helper.write_all(&[0xAA; 28]).unwrap();
    assert_eq!(bad_helper.read(&mut [0u8; 1]).unwrap_or(0), 0);

    let mut good = connect(rig.running.ttp_addr);
    assert_eq!(command(&mut good, 1, 1000, Vec::new()).error, 0);
}

/// A hello from a helper that reports the game's executable.
fn helper_hello_from(channel: u32, slot: u32, executable: &str) -> Vec<u8> {
    let mut hello = helper_hello(channel, slot);
    hello[..4].copy_from_slice(b"OTB2");
    hello.extend_from_slice(&(executable.len() as u16).to_le_bytes());
    hello.extend_from_slice(executable.as_bytes());
    hello
}

#[test]
fn a_game_with_its_own_profile_gets_that_profile() {
    let mut tuning = unsmoothed();
    let yaw = Axis {
        dead_zone: 1.0,
        sensitivity: 2.0,
        ..Axis::default()
    };
    let smoothing = Smoothing {
        enabled: false,
        ..Smoothing::default()
    };
    let profile = Profile {
        yaw,
        smoothing,
        ..Profile::default()
    };
    tuning.games.insert("eurotrucks2.exe".into(), profile);
    let mut rig = rig(tuning);

    let executable = "S:\\steamapps\\common\\ETS2\\bin\\win_x64\\EuroTrucks2.exe";
    let mut commands = connect(rig.running.relay_addr);
    commands
        .write_all(&helper_hello_from(sesp::CHANNEL_COMMAND, 1, executable))
        .unwrap();
    let mut subscription = connect(rig.running.relay_addr);
    subscription
        .write_all(&helper_hello_from(
            sesp::CHANNEL_SUBSCRIPTION,
            2,
            executable,
        ))
        .unwrap();
    let start = sesp::build(1, sesp::T_HEAD_POSE_START, sesp::Table::new());
    commands.write_all(&sesp::frame(&start)).unwrap();
    read_sesp(&mut commands);
    thread::sleep(Duration::from_millis(50));

    let mut yaw_for = |degrees: f32| {
        let pose = HeadPose {
            host_micros: monotonic_micros(),
            position_valid: true,
            position_mm: [0.0, 0.0, 700.0],
            rotation_valid: [true; 3],
            rotation_rad: [0.0, degrees.to_radians(), 0.0],
        };
        rig.driver
            .write_all(&tobiid::frame(&head_pose_body(&pose)))
            .unwrap();
        let message = read_sesp(&mut subscription);
        let payload = sesp::parse(&message).unwrap().payload.unwrap();
        payload.f32(6, 0.0).to_degrees()
    };
    assert_eq!(yaw_for(0.5), 0.0, "inside the game's dead zone");
    assert!(
        (yaw_for(6.0) - 10.0).abs() < 1e-3,
        "5 degrees beyond it, doubled"
    );
    assert!((yaw_for(-6.0) + 10.0).abs() < 1e-3);
}

// --- the control socket -----------------------------------------------------

struct Control {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Control {
    fn connect(rig: &Rig) -> Self {
        let path = rig
            .running
            .control_socket
            .clone()
            .expect("a control socket");
        let writer = UnixStream::connect(path).unwrap();
        writer.set_read_timeout(Some(TIMEOUT)).unwrap();
        let reader = BufReader::new(writer.try_clone().unwrap());
        Self { reader, writer }
    }

    fn send_text(&mut self, text: &str) {
        self.writer.write_all(text.as_bytes()).unwrap();
        self.writer.write_all(b"\n").unwrap();
    }

    fn line(&mut self) -> Line {
        let mut text = String::new();
        self.reader.read_line(&mut text).unwrap();
        serde_json::from_str(&text).unwrap_or_else(|err| panic!("{err}: {text:?}"))
    }

    fn ask(&mut self, request: &Request) -> Line {
        self.send_text(&serde_json::to_string(request).unwrap());
        self.line()
    }
}

/// A fake game that asked for head pose; returns its subscription channel.
fn posing_game(rig: &Rig) -> (TcpStream, TcpStream) {
    let mut commands = connect(rig.running.relay_addr);
    commands
        .write_all(&helper_hello(sesp::CHANNEL_COMMAND, 1))
        .unwrap();
    let mut subscription = connect(rig.running.relay_addr);
    subscription
        .write_all(&helper_hello(sesp::CHANNEL_SUBSCRIPTION, 2))
        .unwrap();
    let start = sesp::build(1, sesp::T_HEAD_POSE_START, sesp::Table::new());
    commands.write_all(&sesp::frame(&start)).unwrap();
    read_sesp(&mut commands);
    thread::sleep(Duration::from_millis(50));
    (commands, subscription)
}

fn send_yaw(rig: &mut Rig, degrees: f32) {
    let pose = HeadPose {
        host_micros: monotonic_micros(),
        position_valid: true,
        position_mm: [0.0, 0.0, 700.0],
        rotation_valid: [true; 3],
        rotation_rad: [0.0, degrees.to_radians(), 0.0],
    };
    rig.driver
        .write_all(&tobiid::frame(&head_pose_body(&pose)))
        .unwrap();
}

fn yaw_seen(subscription: &mut TcpStream) -> f32 {
    let message = read_sesp(subscription);
    let payload = sesp::parse(&message).unwrap().payload.unwrap();
    payload.f32(6, 0.0).to_degrees()
}

#[test]
fn control_reports_settings_and_what_is_connected() {
    let rig = rig(unsmoothed());
    let _game = posing_game(&rig);
    let mut control = Control::connect(&rig);
    let Line::State { config, status } = control.ask(&Request::Get) else {
        panic!("a state line")
    };
    assert_eq!(config, unsmoothed());
    assert!(status.driver_connected);
    assert!(!status.tracking, "no pose has arrived yet");
    assert_eq!(status.display_width_mm, Some(600.0));
    assert_eq!(status.games.len(), 1);
    assert_eq!(
        (status.games[0].pid, status.games[0].head_pose),
        (392, true)
    );
    assert!(!status.games[0].own_profile);
    assert!(status.settings_path.unwrap().ends_with("config.toml"));
}

#[test]
fn a_setting_changed_over_control_applies_on_the_next_pose_and_is_saved() {
    let mut rig = rig(unsmoothed());
    let (_commands, mut subscription) = posing_game(&rig);
    let mut control = Control::connect(&rig);

    send_yaw(&mut rig, 6.0);
    assert!((yaw_seen(&mut subscription) - 6.0).abs() < 1e-3);

    let mut profile = unsmoothed().default;
    profile.yaw = Axis {
        dead_zone: 1.0,
        sensitivity: 2.0,
        ..Axis::default()
    };
    let set = Request::SetProfile {
        game: None,
        profile,
    };
    assert_eq!(control.ask(&set), Line::Ok);

    send_yaw(&mut rig, 6.0);
    assert!(
        (yaw_seen(&mut subscription) - 10.0).abs() < 1e-3,
        "the very next pose"
    );

    let saved = Tuning::load(&rig._dir.0.join("config.toml")).unwrap();
    assert_eq!(saved.default.yaw.sensitivity, 2.0);
    assert_eq!(saved.default.yaw.dead_zone, 1.0);
}

#[test]
fn recentre_makes_the_current_pose_neutral() {
    let mut rig = rig(unsmoothed());
    let (_commands, mut subscription) = posing_game(&rig);
    let mut control = Control::connect(&rig);

    let early = control.ask(&Request::Recentre { game: None });
    assert!(
        matches!(early, Line::Error { ref reason } if reason.contains("no head pose")),
        "{early:?}"
    );

    send_yaw(&mut rig, 12.0);
    assert!((yaw_seen(&mut subscription) - 12.0).abs() < 1e-3);
    let Line::Recentred { neutral } = control.ask(&Request::Recentre { game: None }) else {
        panic!("a recentred line")
    };
    assert!((neutral.rotation[1] - 12.0).abs() < 1e-3);
    assert_eq!(neutral.position, Some([0.0, 0.0, 700.0]));

    send_yaw(&mut rig, 12.0);
    assert!(yaw_seen(&mut subscription).abs() < 1e-3);
    send_yaw(&mut rig, 20.0);
    assert!((yaw_seen(&mut subscription) - 8.0).abs() < 1e-3);
}

#[test]
fn a_game_profile_can_be_added_and_removed_live() {
    let mut tuning = unsmoothed();
    tuning.default.yaw.sensitivity = 3.0;
    let rig = rig(tuning);
    let mut control = Control::connect(&rig);

    let unknown = control.ask(&Request::Recentre {
        game: Some("eurotrucks2.exe".into()),
    });
    assert!(matches!(unknown, Line::Error { .. }), "{unknown:?}");

    let set = Request::SetProfile {
        game: Some("C:\\Games\\EuroTrucks2.EXE".into()),
        profile: unsmoothed().default,
    };
    assert_eq!(control.ask(&set), Line::Ok);
    let Line::State { config, .. } = control.ask(&Request::Get) else {
        panic!("a state line")
    };
    assert!(
        config.has_profile("eurotrucks2.exe"),
        "stored under the file name"
    );
    assert_eq!(config.profile_for("eurotrucks2.exe").yaw.sensitivity, 1.0);

    let delete = Request::DeleteProfile {
        game: "eurotrucks2.exe".into(),
    };
    assert_eq!(control.ask(&delete), Line::Ok);
    assert!(matches!(control.ask(&delete), Line::Error { .. }));
}

#[test]
fn subscribers_get_raw_poses_and_status() {
    let mut rig = rig(Tuning::default());
    let mut control = Control::connect(&rig);
    assert_eq!(
        control.ask(&Request::Subscribe {
            poses: true,
            gaze: false,
        }),
        Line::Ok
    );
    thread::sleep(Duration::from_millis(50));

    send_yaw(&mut rig, 7.0);
    let Line::Pose(pose) = control.line() else {
        panic!("a pose line")
    };
    assert!(pose.valid);
    assert!(
        (pose.rotation_deg[1] - 7.0).abs() < 1e-3,
        "unshaped, unsmoothed"
    );
    assert_eq!(pose.position_mm, [0.0, 0.0, 700.0]);

    let Line::Status(status) = control.line() else {
        panic!("a status line")
    };
    assert!(status.driver_connected && status.tracking);
}

#[test]
fn bad_control_requests_are_answered_not_fatal() {
    let rig = rig(unsmoothed());
    let mut control = Control::connect(&rig);

    control.send_text("this is not json");
    assert!(
        matches!(control.line(), Line::Error { ref reason } if reason.contains("not a request"))
    );

    control.send_text(r#"{"cmd":"set_profile","profile":{"yaw":{"dead_zone":-3.0}}}"#);
    assert!(matches!(control.line(), Line::Error { ref reason } if reason.contains("yaw")));

    control.send_text(r#"{"cmd":"set_profile","profile":{"yaw":{"sensitivty":2.0}}}"#);
    assert!(matches!(control.line(), Line::Error { .. }));

    let Line::State { config, .. } = control.ask(&Request::Get) else {
        panic!("still answering")
    };
    assert_eq!(config, unsmoothed(), "nothing refused was applied");
}

#[test]
fn a_second_daemon_on_the_same_control_socket_does_not_start() {
    let rig = rig(unsmoothed());
    let any = SocketAddr::from(([127, 0, 0, 1], 0));
    let second = start(Config {
        ttp_addr: any,
        relay_addr: any,
        tobiid_socket: rig._dir.0.join("tobiid.sock"),
        tuning: Tuning::default(),
        force_unsmoothed: false,
        settings_path: None,
        control_socket: rig.running.control_socket.clone(),
        monitor_index: None,
        manage_overlay: false,
        confirm_banner: false,
    });
    let err = second.expect_err("the socket is taken");
    assert!(
        err.to_string().contains("another opentobii daemon"),
        "{err}"
    );
}

#[test]
fn a_notification_makes_the_daemon_fetch_the_display_area_again() {
    let mut rig = rig(unsmoothed());
    rig.driver
        .write_all(&tobiid::frame(&[tobiid::TAG_NOTIFICATION, 3]))
        .unwrap();
    // The daemon asks again; answer with a wider screen.
    let mut request = [0u8; 10];
    rig.driver.set_read_timeout(Some(TIMEOUT)).unwrap();
    rig.driver.read_exact(&mut request).unwrap();
    assert_eq!(
        request.to_vec(),
        tobiid::frame(&tobiid::request(1, tobiid::KIND_DISPLAY_AREA_GET, &[]))
    );
    let wider: [[f32; 3]; 3] = [
        [-350.0, 400.0, 100.0],
        [350.0, 400.0, 100.0],
        [-350.0, 5.0, -2.0],
    ];
    let mut reply = vec![tobiid::TAG_REPLY, 1, 0, 0, 0, tobiid::STATUS_OK];
    reply.extend(wider.iter().flatten().flat_map(|v| v.to_le_bytes()));
    rig.driver.write_all(&tobiid::frame(&reply)).unwrap();

    let mut game = connect(rig.running.ttp_addr);
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let area = command(&mut game, 1, 1430, Vec::new());
        if display_area_corners(&area.payload) == Some(wider.map(|c| c.map(f64::from))) {
            break;
        }
        assert!(Instant::now() < deadline, "the game is told the new area");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_gaze_subscriber_gets_gaze_and_not_poses() {
    let mut rig = rig(unsmoothed());
    let mut control = Control::connect(&rig);
    let subscribe = Request::Subscribe {
        poses: false,
        gaze: true,
    };
    assert_eq!(control.ask(&subscribe), Line::Ok);
    thread::sleep(Duration::from_millis(50));

    send_yaw(&mut rig, 7.0);
    let host = monotonic_micros();
    let lost = GazeRaw {
        combined_validity: 0,
        ..sample(host)
    };
    rig.driver
        .write_all(&tobiid::frame(&gaze_raw_body(&sample(host))))
        .unwrap();
    rig.driver
        .write_all(&tobiid::frame(&gaze_raw_body(&lost)))
        .unwrap();

    // The pose sent first is not passed on: the first line is the gaze.
    let Line::Gaze(gaze) = control.line() else {
        panic!("a gaze line")
    };
    assert_eq!(
        (gaze.x, gaze.y, gaze.valid, gaze.micros),
        (0.125, 0.875, true, host)
    );
    let Line::Gaze(gaze) = control.line() else {
        panic!("a second gaze line")
    };
    assert!(!gaze.valid);
}

#[test]
fn overlay_settings_are_checked_saved_and_announced() {
    let rig = rig(unsmoothed());
    let mut watcher = Control::connect(&rig);
    let quiet = Request::Subscribe {
        poses: false,
        gaze: false,
    };
    assert_eq!(watcher.ask(&quiet), Line::Ok);
    let mut control = Control::connect(&rig);

    let bad = Overlay {
        size: 1.0,
        ..Overlay::default()
    };
    let refused = control.ask(&Request::SetOverlay { overlay: bad });
    assert!(
        matches!(refused, Line::Error { ref reason } if reason.contains("size")),
        "{refused:?}"
    );

    let wanted = Overlay {
        enabled: false,
        style: Style::Solid,
        size: 220.0,
        color: "#ff8000".into(),
        ..Overlay::default()
    };
    let set = Request::SetOverlay {
        overlay: wanted.clone(),
    };
    assert_eq!(control.ask(&set), Line::Ok);
    assert_eq!(watcher.line(), Line::Overlay(wanted.clone()));

    let Line::State { config, .. } = control.ask(&Request::Get) else {
        panic!("a state line")
    };
    assert_eq!(config.overlay, wanted);
    let saved = Tuning::load(&rig._dir.0.join("config.toml")).unwrap();
    assert_eq!(saved.overlay, wanted);
}

// --- the nod that approves a face login --------------------------------------

fn send_head(rig: &mut Rig, host_micros: i64, pitch: f32, yaw: f32) {
    let pose = HeadPose {
        host_micros,
        position_valid: true,
        position_mm: [0.0, 0.0, 700.0],
        rotation_valid: [true; 3],
        rotation_rad: [pitch.to_radians(), yaw.to_radians(), 0.0],
    };
    rig.driver
        .write_all(&tobiid::frame(&head_pose_body(&pose)))
        .unwrap();
}

/// The head goes 15 degrees one way and straight back in half a second of
/// tracker time, on pitch (a nod) or yaw (a shake).
fn gesture(rig: &mut Rig, nod: bool) {
    let start = monotonic_micros();
    for step in 0..=16i16 {
        let out = f32::from(8 - (step - 8).abs()) / 8.0;
        let (pitch, yaw) = if nod {
            (-15.0 * out, 0.0)
        } else {
            (0.0, 15.0 * out)
        };
        send_head(rig, start + i64::from(step) * 30_000, pitch, yaw);
        thread::sleep(Duration::from_millis(2));
    }
}

/// Send `confirm` without waiting for the answer, once the daemon has seen
/// a pose and so knows somebody is there.
fn ask_for_a_nod(rig: &mut Rig) -> Control {
    send_head(rig, monotonic_micros(), 0.0, 0.0);
    thread::sleep(Duration::from_millis(50));
    let mut control = Control::connect(rig);
    control.send_text(r#"{"cmd":"confirm","service":"polkit-1"}"#);
    thread::sleep(Duration::from_millis(100));
    control
}

fn confirmed(answer: Answer) -> Line {
    Line::Confirmed { answer }
}

#[test]
fn a_nod_approves_and_a_shake_denies() {
    let mut rig = rig(Tuning::default());
    let mut control = ask_for_a_nod(&mut rig);
    gesture(&mut rig, true);
    assert_eq!(control.line(), confirmed(Answer::Approved));

    // The same connection can ask again: the first prompt is over.
    thread::sleep(Duration::from_millis(400));
    send_head(&mut rig, monotonic_micros(), 0.0, 0.0);
    control.send_text(r#"{"cmd":"confirm"}"#);
    thread::sleep(Duration::from_millis(100));
    gesture(&mut rig, false);
    assert_eq!(control.line(), confirmed(Answer::Denied));
}

#[test]
fn no_answer_times_out_and_a_second_prompt_is_refused_meanwhile() {
    let mut tuning = Tuning::default();
    tuning.confirm.seconds = 3.0;
    let mut rig = rig(tuning);
    let mut first = ask_for_a_nod(&mut rig);
    let asked = Instant::now();

    let mut second = Control::connect(&rig);
    assert_eq!(
        second.ask(&Request::Confirm { service: None }),
        confirmed(Answer::Unavailable)
    );
    assert!(asked.elapsed() < Duration::from_secs(1), "refused at once");

    // Looking down at the keyboard and staying there is not a nod.
    let start = monotonic_micros();
    for step in 0..10i16 {
        send_head(
            &mut rig,
            start + i64::from(step) * 30_000,
            -1.6 * f32::from(step),
            0.0,
        );
    }
    assert_eq!(first.line(), confirmed(Answer::Timeout));
    assert!(asked.elapsed() >= Duration::from_millis(2900));
}

#[test]
fn with_nobody_tracked_the_question_is_not_put() {
    let rig = rig(Tuning::default());
    let mut control = Control::connect(&rig);
    let asked = Instant::now();
    assert_eq!(
        control.ask(&Request::Confirm { service: None }),
        confirmed(Answer::Unavailable)
    );
    assert!(asked.elapsed() < Duration::from_secs(1));
}

#[test]
fn a_cancelled_dialog_ends_its_prompt() {
    let mut rig = rig(Tuning::default());
    let gone = ask_for_a_nod(&mut rig);
    drop(gone);
    thread::sleep(Duration::from_millis(300));

    let mut control = ask_for_a_nod(&mut rig);
    gesture(&mut rig, true);
    assert_eq!(control.line(), confirmed(Answer::Approved));
}
