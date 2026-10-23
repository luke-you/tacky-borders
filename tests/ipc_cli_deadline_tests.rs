use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tacky_borders::iocp::UnixListener;
use tacky_borders::ipc::CLI_COMMAND_TIMEOUT;
use windows::Win32::Networking::WinSock::{FIONBIO, SOCKET, ioctlsocket};

#[test]
fn cli_process_exits_with_timeout_when_server_accepts_but_never_replies() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let config_home = std::env::temp_dir().join(format!("tb-cli-{}-{nonce}", std::process::id()));
    std::fs::create_dir(&config_home).unwrap();
    let socket = config_home.join("tacky-borders.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let mut nonblocking = 1;
    assert_eq!(
        unsafe { ioctlsocket(SOCKET(listener.token()), FIONBIO, &mut nonblocking) },
        0
    );
    let (release, released) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let peer_deadline = Instant::now() + CLI_COMMAND_TIMEOUT + Duration::from_secs(10);
        let stream = loop {
            match listener.accept() {
                Ok(stream) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if released.try_recv().is_ok() || Instant::now() >= peer_deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("accept CLI connection: {error}"),
            }
        };
        let mut request = String::new();
        let mut reader = BufReader::new(&stream);
        loop {
            match reader.read_line(&mut request) {
                Ok(_) if request.ends_with('\n') => break,
                Ok(0) => return Some(request),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("read CLI command: {error}"),
            }
            if released.try_recv().is_ok() || Instant::now() >= peer_deadline {
                return Some(request);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = released.recv_timeout(CLI_COMMAND_TIMEOUT + Duration::from_secs(10));
        Some(request)
    });
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_tacky-borders"))
        .arg("get-state")
        .env("TACKY_BORDERS_CONFIG_HOME", &config_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch CLI process");
    let deadline = started + CLI_COMMAND_TIMEOUT + Duration::from_secs(5);
    let timed_out = loop {
        if child.try_wait().expect("poll CLI process").is_some() {
            break false;
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill timed-out CLI child");
            break true;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = child.wait_with_output().unwrap();
    let _ = release.send(());
    let request = peer.join().unwrap();
    std::fs::remove_file(socket).unwrap();
    std::fs::remove_dir(config_home).unwrap();
    assert!(!timed_out, "CLI exceeded external deadline: {output:?}");
    let request = request.expect("CLI did not connect to test peer");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&request).unwrap()["cmd"],
        "get_state"
    );
    assert!(!output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("timed out waiting for IPC response"),
        "{output:?}"
    );
    assert!(started.elapsed() >= CLI_COMMAND_TIMEOUT);
}
