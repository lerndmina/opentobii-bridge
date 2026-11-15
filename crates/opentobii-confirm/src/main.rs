//! The login helper: `opentobii-confirm`.
//!
//! `pam_exec` runs this as root after `pam_howdy` has matched a face. It
//! asks the opentobii daemon of the person logging in to put the question
//! on their screen, and exits 0 only if they nod. Anything else (a shake,
//! no answer, no daemon, any error) exits non-zero, and PAM goes on to ask
//! for the password.
//!
//! ```text
//! auth  [success=ok default=1]        pam_howdy.so
//! auth  [success=done default=ignore] pam_exec.so quiet /usr/local/libexec/opentobii-confirm
//! ```
//!
//! It is installed owned by root, apart from the rest of opentobii, because
//! a program root runs must not be a file the user can replace. It reads
//! `PAM_USER` and `PAM_SERVICE` and nothing else, gives up root before it
//! opens the socket, and prints nothing on standard output: polkit's helper
//! shares that with its dialog and fails on a line it does not know.

use std::ffi::CString;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use nix::unistd::{self, Uid, User};

/// Longer than the longest wait the daemon's settings allow (60 s), so the
/// daemon's own time limit is the one that normally ends a prompt.
const WAIT: Duration = Duration::from_secs(75);
/// The one line that lets a login through. The daemon's tests pin it.
const APPROVED: &str = r#"{"type":"confirmed","answer":"approved"}"#;
const MAX_LINE: u64 = 4096;

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(reason) => {
            eprintln!("opentobii-confirm: {reason}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<bool, String> {
    let name = std::env::var("PAM_USER")
        .map_err(|_| "PAM_USER is not set: this program is run by pam_exec".to_string())?;
    let user = User::from_name(&name)
        .map_err(|err| format!("looking up {name}: {err}"))?
        .ok_or_else(|| format!("no user called {name}"))?;
    if user.uid.is_root() {
        return Err("root has no desktop to put the question on".into());
    }
    give_up_root(&user)?;
    let uid = user.uid.as_raw();
    let socket = socket_for(uid);
    check_socket(&socket, uid)?;
    let service = std::env::var("PAM_SERVICE")
        .ok()
        .and_then(|raw| service_name(&raw));
    ask(&socket, service.as_deref(), WAIT)
}

/// Become `user` for good. Run by hand as that user, there is nothing to
/// give up; as anyone else but root it is refused.
fn give_up_root(user: &User) -> Result<(), String> {
    let me = unistd::geteuid();
    if me == user.uid && unistd::getuid() == user.uid {
        return Ok(());
    }
    if !me.is_root() {
        return Err(format!("not root and not {}", user.name));
    }
    let name = CString::new(user.name.as_str()).map_err(|_| "user name holds a NUL")?;
    unistd::initgroups(&name, user.gid).map_err(|err| format!("initgroups: {err}"))?;
    unistd::setgid(user.gid).map_err(|err| format!("setgid: {err}"))?;
    unistd::setuid(user.uid).map_err(|err| format!("setuid: {err}"))?;
    if unistd::setuid(Uid::from_raw(0)).is_ok() || unistd::geteuid() != user.uid {
        return Err("could not give up root".into());
    }
    Ok(())
}

/// Where a user's daemon listens: the same place the daemon works out from
/// `XDG_RUNTIME_DIR`, which PAM does not pass on.
fn socket_for(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}/opentobii.sock"))
}

/// The socket must be a socket and must belong to the person logging in.
fn check_socket(path: &Path, uid: u32) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|err| {
        format!(
            "{}: {err} (is the opentobii daemon running?)",
            path.display()
        )
    })?;
    if !meta.file_type().is_socket() {
        return Err(format!("{} is not a socket", path.display()));
    }
    if meta.uid() != uid {
        return Err(format!("{} belongs to someone else", path.display()));
    }
    Ok(())
}

/// A PAM service name fit to show on screen and to put in the request
/// as it stands: letters, digits, `-` and `_`, at most 32 of them.
fn service_name(raw: &str) -> Option<String> {
    let fits = !raw.is_empty()
        && raw.len() <= 32
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    fits.then(|| raw.to_string())
}

/// Put the question and wait for the answer. `Ok(true)` is a nod.
fn ask(socket: &Path, service: Option<&str>, wait: Duration) -> Result<bool, String> {
    let mut stream =
        UnixStream::connect(socket).map_err(|err| format!("{}: {err}", socket.display()))?;
    let request = match service {
        Some(service) => format!("{{\"cmd\":\"confirm\",\"service\":\"{service}\"}}\n"),
        None => "{\"cmd\":\"confirm\"}\n".to_string(),
    };
    stream
        .write_all(request.as_bytes())
        .map_err(|err| format!("sending the request: {err}"))?;
    let deadline = Instant::now() + wait;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        let left = deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
            .ok_or("the daemon did not answer in time")?;
        reader
            .get_ref()
            .set_read_timeout(Some(left))
            .map_err(|err| err.to_string())?;
        line.clear();
        let read = reader
            .by_ref()
            .take(MAX_LINE)
            .read_line(&mut line)
            .map_err(|_| "the daemon did not answer in time".to_string())?;
        if read == 0 {
            return Err("the daemon closed the connection without an answer".into());
        }
        let line = line.trim();
        if line.contains(r#""type":"confirmed""#) {
            return Ok(line == APPROVED);
        }
        if line.contains(r#""type":"error""#) {
            // A daemon from before the nod prompt does not know the request.
            return Err(format!("the daemon refused: {line}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;

    use super::*;

    static NEXT: AtomicU32 = AtomicU32::new(0);

    /// A socket in a fresh directory, and a daemon on it that reads one
    /// request line and does `then` with the connection.
    fn daemon(then: impl FnOnce(String, UnixStream) + Send + 'static) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "opentobii-confirm-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("opentobii.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            then(request, stream);
        });
        path
    }

    fn answering(text: &'static str) -> PathBuf {
        daemon(move |_, mut stream| {
            stream.write_all(text.as_bytes()).unwrap();
        })
    }

    const SHORT: Duration = Duration::from_millis(300);

    #[test]
    fn only_a_nod_is_a_yes() {
        let yes = answering("{\"type\":\"confirmed\",\"answer\":\"approved\"}\n");
        assert_eq!(ask(&yes, None, SHORT), Ok(true));
        for answer in ["denied", "timeout", "unavailable", "approved_ish"] {
            let text: &'static str =
                Box::leak(format!("{{\"type\":\"confirmed\",\"answer\":\"{answer}\"}}\n").into());
            assert_eq!(ask(&answering(text), None, SHORT), Ok(false), "{answer}");
        }
    }

    #[test]
    fn a_line_that_only_looks_like_a_yes_is_a_no() {
        let padded =
            answering("{\"type\":\"confirmed\",\"answer\":\"denied\",\"note\":\"approved\"}\n");
        assert_eq!(ask(&padded, None, SHORT), Ok(false));
    }

    #[test]
    fn other_lines_before_the_answer_are_skipped() {
        let chatty =
            answering("{\"type\":\"status\"}\n{\"type\":\"confirmed\",\"answer\":\"approved\"}\n");
        assert_eq!(ask(&chatty, None, SHORT), Ok(true));
    }

    #[test]
    fn the_request_names_the_service_when_there_is_one() {
        let (tell, told) = std::sync::mpsc::channel();
        let path = daemon(move |request, mut stream| {
            tell.send(request).unwrap();
            stream
                .write_all(b"{\"type\":\"confirmed\",\"answer\":\"denied\"}\n")
                .unwrap();
        });
        assert_eq!(ask(&path, Some("polkit-1"), SHORT), Ok(false));
        assert_eq!(
            told.recv().unwrap(),
            "{\"cmd\":\"confirm\",\"service\":\"polkit-1\"}\n"
        );
    }

    #[test]
    fn an_old_daemon_a_silent_one_and_a_gone_one_are_errors() {
        let old = answering("{\"type\":\"error\",\"reason\":\"not a request\"}\n");
        assert!(ask(&old, None, SHORT).unwrap_err().contains("refused"));

        let silent = daemon(|_, stream| {
            thread::sleep(Duration::from_secs(2));
            drop(stream);
        });
        let started = Instant::now();
        assert!(ask(&silent, None, SHORT).unwrap_err().contains("in time"));
        assert!(started.elapsed() < Duration::from_secs(1));

        let gone = daemon(|_, stream| drop(stream));
        assert!(ask(&gone, None, SHORT).unwrap_err().contains("closed"));

        let nowhere = std::env::temp_dir().join("opentobii-confirm-test-nowhere.sock");
        assert!(ask(&nowhere, None, SHORT).is_err());
    }

    #[test]
    fn the_socket_must_be_a_socket_of_the_right_owner() {
        let mine = unistd::getuid().as_raw();
        let path = answering("");
        assert_eq!(check_socket(&path, mine), Ok(()));
        assert!(check_socket(&path, mine + 1)
            .unwrap_err()
            .contains("someone else"));

        let file = path.with_file_name("plain-file");
        std::fs::write(&file, b"").unwrap();
        assert!(check_socket(&file, mine)
            .unwrap_err()
            .contains("not a socket"));
        assert!(check_socket(&path.with_file_name("missing"), mine).is_err());
        // Let the listener thread finish.
        let _ = UnixStream::connect(&path);
    }

    #[test]
    fn service_names_are_plain_or_dropped() {
        assert_eq!(service_name("polkit-1").as_deref(), Some("polkit-1"));
        assert_eq!(service_name("sudo").as_deref(), Some("sudo"));
        assert_eq!(service_name(""), None);
        assert_eq!(service_name("a\"b"), None);
        assert_eq!(service_name("with space"), None);
        assert_eq!(service_name(&"x".repeat(33)), None);
    }

    #[test]
    fn the_socket_is_in_the_users_runtime_directory() {
        assert_eq!(
            socket_for(1000),
            PathBuf::from("/run/user/1000/opentobii.sock")
        );
    }
}
