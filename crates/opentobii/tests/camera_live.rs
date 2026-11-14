//! The face login camera against the real driver and a real `v4l2loopback`
//! device. Ignored by default: it needs both on the machine.
//!
//! ```sh
//! OPENTOBII_CAMERA_DEVICE=/dev/video10 OPENTOBII_CAMERA_OUT=/tmp/frame.png \
//!     cargo test -p opentobii --test camera_live -- --ignored --nocapture
//! ```

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use opentobii_core::control::{Line, Request};

fn camera_status(socket: &Path) -> String {
    let mut stream = UnixStream::connect(socket).expect("the control socket");
    let mut text = serde_json::to_string(&Request::Get).unwrap();
    text.push('\n');
    stream.write_all(text.as_bytes()).unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).unwrap();
    match serde_json::from_str(&line).unwrap() {
        Line::State { status, .. } => status.camera,
        other => panic!("{other:?}"),
    }
}

fn wait_for(socket: &Path, wanted: &str, within: Duration) -> String {
    let since = Instant::now();
    loop {
        let status = camera_status(socket);
        if status.starts_with(wanted) {
            println!("{:6.2}s  {status}", since.elapsed().as_secs_f64());
            return status;
        }
        assert!(
            since.elapsed() < within,
            "still {status:?} after {within:?}, wanted {wanted:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "needs the tracker's driver and a v4l2loopback device"]
fn a_reader_gets_pictures_and_the_feed_stops_after_it() {
    let device = std::env::var("OPENTOBII_CAMERA_DEVICE").expect("OPENTOBII_CAMERA_DEVICE");
    let out = std::env::var("OPENTOBII_CAMERA_OUT").expect("OPENTOBII_CAMERA_OUT");
    let folder = std::env::temp_dir().join(format!("opentobii-camera-{}", std::process::id()));
    std::fs::create_dir_all(&folder).unwrap();
    let socket = folder.join("control.sock");

    let mut config = opentobii::Config {
        ttp_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        relay_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
        control_socket: Some(socket.clone()),
        manage_overlay: false,
        ..opentobii::Config::default()
    };
    config.tuning.camera.enabled = true;
    config.tuning.camera.device = device.clone();
    let _running = opentobii::start(config).expect("the daemon");

    wait_for(&socket, "Ready", Duration::from_secs(5));

    // A reader: take 40 pictures and keep the last.
    let began = Instant::now();
    let mut reader = Command::new("ffmpeg")
        .args(["-loglevel", "error", "-y", "-f", "v4l2", "-i", &device])
        .args(["-frames:v", "40", "-update", "1", &out])
        .spawn()
        .expect("ffmpeg");
    wait_for(&socket, "Feeding", Duration::from_secs(5));
    let status = reader.wait().unwrap();
    println!(
        "reader done after {:.2}s: {status}",
        began.elapsed().as_secs_f64()
    );
    assert!(status.success());
    wait_for(&socket, "Ready", Duration::from_secs(5));
    std::fs::remove_dir_all(&folder).ok();
}
