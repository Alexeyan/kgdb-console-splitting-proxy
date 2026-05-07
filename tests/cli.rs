use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn unused_local_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("failed to bind ephemeral port")
        .local_addr()
        .expect("failed to read local address")
        .port()
}

fn distinct_unused_local_port(other: u16) -> u16 {
    loop {
        let port = unused_local_port();
        if port != other {
            return port;
        }
    }
}

#[test]
fn long_boolean_flag_rejects_inline_value() {
    let output = Command::new(env!("CARGO_BIN_EXE_kgdb-console-splitting-proxy"))
        .arg("--verbose=false")
        .arg("--version")
        .output()
        .expect("failed to run kgdb-console-splitting-proxy");

    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Option --verbose does not take a value"),
        "unexpected stderr: {stderr}",
    );
}

#[test]
fn invalid_pid_file_path_fails_loudly() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    let missing_dir = std::env::temp_dir().join(format!("kgdb-console-splitting-proxy-missing-{unique}"));
    let pid_path = missing_dir.join("proxy.pid");
    let local_port = unused_local_port();
    let remote_port = distinct_unused_local_port(local_port);

    let output = Command::new(env!("CARGO_BIN_EXE_kgdb-console-splitting-proxy"))
        .arg("proxy")
        .arg("--local")
        .arg(local_port.to_string())
        .arg("--remote")
        .arg(format!("tcp:127.0.0.1:{remote_port}"))
        .arg("--pid-file")
        .arg(&pid_path)
        .output()
        .expect("failed to run kgdb-console-splitting-proxy");

    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ERROR: Could not write pid file"),
        "unexpected stderr: {stderr}",
    );
}

#[test]
fn serial_split_without_device_reports_missing_default_when_ttyusb0_absent() {
    if std::path::Path::new("/dev/ttyUSB0").exists() {
        return;
    }

    let output = Command::new(env!("CARGO_BIN_EXE_kgdb-console-splitting-proxy"))
        .arg("serial-split")
        .output()
        .expect("failed to run kgdb-console-splitting-proxy");

    assert!(!output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "serial-split requires explicit --device because default device /dev/ttyUSB0 does not exist"
        ),
        "unexpected stderr: {stderr}",
    );
}

#[test]
#[cfg(unix)]
fn serial_split_permission_denied_suggests_sudo() {
    use std::os::unix::fs::PermissionsExt;

    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time before epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("kgdb-console-splitting-proxy-unreadable-{unique}"));
    std::fs::write(&path, b"").expect("failed to create fake device path");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
        .expect("failed to make fake device unreadable");
    let console_port = unused_local_port();
    let gdb_port = distinct_unused_local_port(console_port);

    let output = Command::new(env!("CARGO_BIN_EXE_kgdb-console-splitting-proxy"))
        .arg("serial-split")
        .arg("--device")
        .arg(&path)
        .arg("--console-port")
        .arg(console_port.to_string())
        .arg("--gdb-port")
        .arg(gdb_port.to_string())
        .output()
        .expect("failed to run kgdb-console-splitting-proxy");

    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    let _ = std::fs::remove_file(&path);

    assert!(!output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Run kgdb-console-splitting-proxy with sudo."),
        "unexpected stdout: {stdout}",
    );
}
