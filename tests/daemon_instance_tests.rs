use serial_test::serial;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tacky_borders::{DaemonInstanceClaim, claim_daemon_instance};

#[test]
#[serial]
fn duplicate_process_exits_silently_before_creating_config_state() {
    let _existing_claim = hold_daemon_instance_claim();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let config_home = std::env::temp_dir().join(format!(
        "tacky-borders-duplicate-launch-{}-{nonce}",
        std::process::id()
    ));
    assert!(!config_home.exists());

    let child = Command::new(env!("CARGO_BIN_EXE_tacky-borders"))
        .env("TACKY_BORDERS_CONFIG_HOME", &config_home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch test daemon process");

    let output = run_bounded(child, "duplicate daemon launch");
    assert!(
        output.status.success(),
        "duplicate launch failed: {output:?}"
    );
    assert!(output.stdout.is_empty(), "unexpected stdout: {output:?}");
    assert!(output.stderr.is_empty(), "unexpected stderr: {output:?}");
    assert!(
        !config_home.exists(),
        "duplicate launch created config state at {}",
        config_home.display()
    );
}

#[test]
#[serial]
fn cli_help_remains_available_while_daemon_mutex_is_held() {
    let _existing_claim = hold_daemon_instance_claim();
    let child = Command::new(env!("CARGO_BIN_EXE_tacky-borders"))
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("launch CLI help process");
    let output = run_bounded(child, "CLI help");

    assert!(output.status.success(), "CLI help failed: {output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("USAGE:"),
        "CLI help output was missing usage text: {output:?}"
    );
    assert!(output.stderr.is_empty(), "unexpected stderr: {output:?}");
}

fn hold_daemon_instance_claim() -> tacky_borders::DaemonInstanceGuard {
    match claim_daemon_instance().unwrap() {
        DaemonInstanceClaim::Acquired(guard) | DaemonInstanceClaim::AlreadyRunning(guard) => guard,
    }
}

fn run_bounded(mut child: std::process::Child, label: &str) -> std::process::Output {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if child.try_wait().expect("poll child process").is_some() {
            return child.wait_with_output().expect("collect child output");
        }
        if Instant::now() >= deadline {
            child.kill().expect("stop timed-out child process");
            let output = child.wait_with_output().expect("collect child output");
            panic!(
                "{label} did not exit within two seconds; stdout={:?}, stderr={:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
