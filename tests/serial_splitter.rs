use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::pty::{PtyMaster, grantpt, posix_openpt, ptsname_r, unlockpt};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

struct ProxyProcess {
    child: Child,
}

const TELNET_STARTUP: [u8; 15] = [
    0xff, 0xfb, 0x01, 0xff, 0xfb, 0x03, 0xff, 0xfd, 0x03, 0xff, 0xfb, 0x00, 0xff, 0xfd, 0x00,
];

fn serial_test_guard() -> MutexGuard<'static, ()> {
    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn serial_split_command(serial_path: &str, console_port: u16, gdb_port: u16) -> Command {
    let exe = env!("CARGO_BIN_EXE_kgdb-console-splitting-proxy");
    let mut command = Command::new(exe);
    command
        .arg("serial-split")
        .arg("--device")
        .arg(serial_path)
        .arg("--baud")
        .arg("115200")
        .arg("--console-port")
        .arg(console_port.to_string())
        .arg("--gdb-port")
        .arg(gdb_port.to_string());
    command
}

fn connect_proxy(child: &mut Child, port: u16) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(status) = child.try_wait().expect("failed to poll proxy") {
            panic!("kgdb-console-splitting-proxy exited before accepting connections: {status}");
        }

        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(stream) => return stream,
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                thread::sleep(Duration::from_millis(25));
            }
            Err(err) => panic!("proxy did not accept connections on {port}: {err}"),
        }
    }
}

impl ProxyProcess {
    fn spawn(serial_path: String, console_port: u16, gdb_port: u16) -> Self {
        let child = serial_split_command(&serial_path, console_port, gdb_port)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn kgdb-console-splitting-proxy");

        ProxyProcess { child }
    }

    fn connect(&mut self, port: u16) -> TcpStream {
        connect_proxy(&mut self.child, port)
    }
}

impl Drop for ProxyProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn unused_local_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
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

fn new_pty() -> (PtyMaster, String) {
    let master = posix_openpt(OFlag::O_RDWR).expect("posix_openpt failed");
    grantpt(&master).expect("grantpt failed");
    unlockpt(&master).expect("unlockpt failed");
    let slave = ptsname_r(&master).expect("ptsname_r failed");
    set_nonblocking(&master);
    (master, slave)
}

fn set_nonblocking(master: &PtyMaster) {
    let flags = fcntl(master, FcntlArg::F_GETFL).expect("fcntl(F_GETFL) failed");
    let new_flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(master, FcntlArg::F_SETFL(new_flags)).expect("fcntl(F_SETFL) failed");
}

fn read_until(stream: &mut TcpStream, needle: &[u8]) -> Vec<u8> {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("failed to set read timeout");

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut received = Vec::new();
    let mut buf = [0u8; 64];
    while !received
        .windows(needle.len())
        .any(|window| window == needle)
    {
        match stream.read(&mut buf) {
            Ok(got) if got > 0 => received.extend_from_slice(&buf[..got]),
            Ok(_) => panic!("socket closed before receiving {needle:?}; got {received:?}"),
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => {
                panic!("socket read failed before receiving {needle:?}; got {received:?}: {err}")
            }
        }

        assert!(
            Instant::now() < deadline,
            "timed out before receiving {needle:?}; got {received:?}",
        );
    }
    received
}

fn read_pty_until(master: &mut PtyMaster, needle: &[u8]) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut received = Vec::new();
    let mut buf = [0u8; 64];

    while !received
        .windows(needle.len())
        .any(|window| window == needle)
    {
        match master.read(&mut buf) {
            Ok(got) if got > 0 => received.extend_from_slice(&buf[..got]),
            Ok(_) => thread::sleep(Duration::from_millis(10)),
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("pty read failed before receiving {needle:?}: {err}"),
        }

        assert!(
            Instant::now() < deadline,
            "timed out before receiving {needle:?}; got {received:?}",
        );
    }

    received
}

fn wait_for_gdb_forwarding(gdb: &mut TcpStream, master: &mut PtyMaster) {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut next_write = Instant::now();
    let mut received = Vec::new();
    let mut buf = [0u8; 64];

    loop {
        if Instant::now() >= next_write {
            gdb.write_all(b"?")
                .expect("gdb readiness probe write failed");
            next_write = Instant::now() + Duration::from_millis(50);
        }

        match master.read(&mut buf) {
            Ok(got) if got > 0 => {
                received.extend_from_slice(&buf[..got]);
                if received.windows(1).any(|w| w == b"?") {
                    return;
                }
            }
            Ok(_) => thread::sleep(Duration::from_millis(10)),
            Err(err)
                if err.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => panic!("pty read failed while waiting for gdb forwarding: {err}"),
        }

        assert!(
            Instant::now() < deadline,
            "timed out waiting for gdb forwarding; got {received:?}",
        );
    }
}

fn strip_ansi(text: &str) -> String {
    let mut stripped = String::new();
    let mut in_escape = false;

    for ch in text.chars() {
        if in_escape {
            if ch.is_ascii_alphabetic() {
                in_escape = false;
            }
            continue;
        }

        if ch == '\x1b' {
            in_escape = true;
        } else {
            stripped.push(ch);
        }
    }

    stripped
}

fn assert_status_box_aligned(stdout: &str) {
    let plain = strip_ansi(stdout);
    let box_lines: Vec<&str> = plain
        .lines()
        .filter(|line| line.starts_with('╭') || line.starts_with('│') || line.starts_with('╰'))
        .collect();
    assert!(!box_lines.is_empty(), "missing status box: {stdout:?}");

    let mut expected_width = None;
    for line in box_lines {
        let width = line.chars().count();
        if line.starts_with('╭') {
            expected_width = Some(width);
            continue;
        }

        match expected_width {
            Some(expected) => assert_eq!(width, expected, "misaligned status row: {line:?}"),
            None => panic!("box row appeared before top border: {line:?}"),
        }

        if line.starts_with('╰') {
            expected_width = None;
        }
    }
}

fn last_status_frame(stdout: &str) -> String {
    strip_ansi(stdout.rsplit("\x1b[J").next().unwrap_or(stdout))
}

fn char_column(line: &str, needle: char) -> Option<usize> {
    line.chars().position(|ch| ch == needle)
}

fn assert_traffic_values_right_aligned(stdout: &str) {
    let frame = last_status_frame(stdout);
    let traffic_lines: Vec<&str> = frame
        .lines()
        .filter(|line| line.starts_with('│') && line.contains('⇣') && line.contains('⇡'))
        .collect();
    assert!(
        traffic_lines.len() >= 5,
        "expected serial, channel, and client traffic rows: {frame:?}",
    );

    let down_col = char_column(traffic_lines[0], '⇣').expect("missing download marker");
    let up_col = char_column(traffic_lines[0], '⇡').expect("missing upload marker");
    for line in traffic_lines {
        assert_eq!(
            char_column(line, '⇣'),
            Some(down_col),
            "download traffic column is not aligned: {line:?}",
        );
        assert_eq!(
            char_column(line, '⇡'),
            Some(up_col),
            "upload traffic column is not aligned: {line:?}",
        );
    }
}

#[test]
fn serial_debug_splitter_forwards_console_and_gdb_traffic() {
    let _guard = serial_test_guard();
    let (mut master, slave_path) = new_pty();
    let console_port = unused_local_port();
    let gdb_port = distinct_unused_local_port(console_port);

    let mut proxy = ProxyProcess::spawn(slave_path, console_port, gdb_port);
    let mut console = proxy.connect(console_port);
    let mut gdb = proxy.connect(gdb_port);
    wait_for_gdb_forwarding(&mut gdb, &mut master);

    master
        .write_all(b"boot log\n$T05#b9")
        .expect("pty write failed");

    let console_data = read_until(&mut console, b"boot log\n");
    assert!(
        console_data
            .windows(b"boot log\n".len())
            .any(|w| w == b"boot log\n"),
        "console data did not include kernel log bytes: {console_data:?}",
    );

    let gdb_data = read_until(&mut gdb, b"$T05#b9");
    assert_eq!(gdb_data, b"$T05#b9");

    gdb.write_all(b"q").expect("gdb client write failed");
    let serial_data = read_pty_until(&mut master, b"q");
    assert!(
        serial_data.windows(1).any(|w| w == b"q"),
        "serial side did not receive gdb client bytes: {serial_data:?}",
    );
}

#[test]
fn serial_debug_splitter_gdb_port_works_without_console_client() {
    let _guard = serial_test_guard();
    let (mut master, slave_path) = new_pty();
    let console_port = unused_local_port();
    let gdb_port = distinct_unused_local_port(console_port);

    let mut proxy = ProxyProcess::spawn(slave_path, console_port, gdb_port);
    let mut gdb = proxy.connect(gdb_port);
    wait_for_gdb_forwarding(&mut gdb, &mut master);

    master.write_all(b"$T05#b9").expect("pty write failed");
    let gdb_data = read_until(&mut gdb, b"$T05#b9");
    assert_eq!(gdb_data, b"$T05#b9");
}

#[test]
fn serial_debug_splitter_gdb_port_survives_console_disconnect() {
    let _guard = serial_test_guard();
    let (mut master, slave_path) = new_pty();
    let console_port = unused_local_port();
    let gdb_port = distinct_unused_local_port(console_port);

    let mut proxy = ProxyProcess::spawn(slave_path, console_port, gdb_port);
    let console = proxy.connect(console_port);
    let mut gdb = proxy.connect(gdb_port);
    wait_for_gdb_forwarding(&mut gdb, &mut master);
    drop(console);
    thread::sleep(Duration::from_millis(150));

    gdb.write_all(b"q").expect("gdb client write failed");
    let serial_data = read_pty_until(&mut master, b"q");
    assert!(
        serial_data.windows(1).any(|w| w == b"q"),
        "serial side did not receive kgdb bytes after console disconnect: {serial_data:?}",
    );
}

#[test]
fn serial_debug_splitter_forwards_fragmented_gdb_packets() {
    let _guard = serial_test_guard();
    let (mut master, slave_path) = new_pty();
    let console_port = unused_local_port();
    let gdb_port = distinct_unused_local_port(console_port);

    let mut proxy = ProxyProcess::spawn(slave_path, console_port, gdb_port);
    let mut gdb = proxy.connect(gdb_port);
    wait_for_gdb_forwarding(&mut gdb, &mut master);

    master.write_all(b"$T").expect("pty write failed");
    thread::sleep(Duration::from_millis(150));
    master.write_all(b"05#b9").expect("pty write failed");

    let gdb_data = read_until(&mut gdb, b"$T05#b9");
    assert_eq!(gdb_data, b"$T05#b9");
}

#[test]
fn serial_debug_splitter_gdb_port_is_raw_tcp() {
    let _guard = serial_test_guard();
    let (mut master, slave_path) = new_pty();
    let console_port = unused_local_port();
    let gdb_port = distinct_unused_local_port(console_port);

    let mut proxy = ProxyProcess::spawn(slave_path, console_port, gdb_port);
    let mut gdb = proxy.connect(gdb_port);

    gdb.write_all(&[0xff, 0xf3, b'X'])
        .expect("gdb client write failed");
    let serial_data = read_pty_until(&mut master, &[0xff, 0xf3, b'X']);
    assert!(
        serial_data
            .windows(3)
            .any(|w| w == [0xff, 0xf3, b'X'].as_slice()),
        "serial side did not receive raw kgdb bytes: {serial_data:?}",
    );
}

#[test]
fn serial_debug_splitter_broadcasts_console_output_to_multiple_clients() {
    let _guard = serial_test_guard();
    let (mut master, slave_path) = new_pty();
    let console_port = unused_local_port();
    let gdb_port = distinct_unused_local_port(console_port);

    let mut proxy = ProxyProcess::spawn(slave_path, console_port, gdb_port);
    let mut console_a = proxy.connect(console_port);
    let mut console_b = proxy.connect(console_port);

    console_a.write_all(b"a").expect("console A write failed");
    let _ = read_pty_until(&mut master, b"a");
    console_b.write_all(b"b").expect("console B write failed");
    let _ = read_pty_until(&mut master, b"b");

    master
        .write_all(b"shared log line\n")
        .expect("pty write failed");

    let console_a_data = read_until(&mut console_a, b"shared log line\n");
    let console_b_data = read_until(&mut console_b, b"shared log line\n");
    assert!(
        console_a_data
            .windows(b"shared log line\n".len())
            .any(|w| w == b"shared log line\n"),
        "first console client missed serial log bytes: {console_a_data:?}",
    );
    assert!(
        console_b_data
            .windows(b"shared log line\n".len())
            .any(|w| w == b"shared log line\n"),
        "second console client missed serial log bytes: {console_b_data:?}",
    );
}

#[test]
fn serial_debug_splitter_console_is_raw_tcp_by_default() {
    let _guard = serial_test_guard();
    let (_master, slave_path) = new_pty();
    let console_port = unused_local_port();
    let gdb_port = distinct_unused_local_port(console_port);

    let mut proxy = ProxyProcess::spawn(slave_path, console_port, gdb_port);
    let mut console = proxy.connect(console_port);
    console
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("failed to set console read timeout");

    let mut buf = [0u8; TELNET_STARTUP.len()];
    match console.read(&mut buf) {
        Err(err) if matches!(err.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
        Err(err) => panic!("unexpected console read error: {err}"),
        Ok(0) => panic!("console connection closed unexpectedly"),
        Ok(n) => panic!("raw console received startup bytes: {:02x?}", &buf[..n]),
    }
}

#[test]
fn serial_debug_splitter_telnet_flag_enables_console_negotiation() {
    let _guard = serial_test_guard();
    let (_master, slave_path) = new_pty();
    let console_port = unused_local_port();
    let gdb_port = distinct_unused_local_port(console_port);

    let child = serial_split_command(&slave_path, console_port, gdb_port)
        .arg("--telnet")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn kgdb-console-splitting-proxy");
    let mut proxy = ProxyProcess { child };

    let mut console = proxy.connect(console_port);
    console
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("failed to set console read timeout");

    let mut buf = [0u8; TELNET_STARTUP.len()];
    console
        .read_exact(&mut buf)
        .expect("failed to read telnet startup bytes");

    assert_eq!(buf, TELNET_STARTUP);
}

#[test]
fn serial_debug_splitter_telnet_escaped_iac_reaches_serial() {
    let _guard = serial_test_guard();
    let (mut master, slave_path) = new_pty();
    let console_port = unused_local_port();
    let gdb_port = distinct_unused_local_port(console_port);

    let child = serial_split_command(&slave_path, console_port, gdb_port)
        .arg("--telnet")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn kgdb-console-splitting-proxy");
    let mut proxy = ProxyProcess { child };

    let mut console = proxy.connect(console_port);
    let mut startup = [0u8; TELNET_STARTUP.len()];
    console
        .read_exact(&mut startup)
        .expect("failed to read telnet startup bytes");

    console
        .write_all(&[0xff, 0xff, b'X'])
        .expect("console write failed");
    let serial_data = read_pty_until(&mut master, &[0xff, b'X']);

    assert!(
        serial_data
            .windows(2)
            .any(|window| window == [0xff, b'X'].as_slice()),
        "serial side did not receive escaped IAC data: {serial_data:?}",
    );
}

#[test]
fn serial_debug_splitter_prints_status_with_clients_and_traffic() {
    let _guard = serial_test_guard();
    let (mut master, slave_path) = new_pty();
    let console_port = unused_local_port();
    let gdb_port = distinct_unused_local_port(console_port);

    let mut child = serial_split_command(&slave_path, console_port, gdb_port)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn kgdb-console-splitting-proxy");

    let mut console = connect_proxy(&mut child, console_port);
    let mut gdb = connect_proxy(&mut child, gdb_port);
    wait_for_gdb_forwarding(&mut gdb, &mut master);

    master
        .write_all(b"boot log\n$T05#b9")
        .expect("pty write failed");
    let _ = read_until(&mut console, b"boot log\n");
    let _ = read_until(&mut gdb, b"$T05#b9");

    gdb.write_all(b"q").expect("gdb client write failed");
    let _ = read_pty_until(&mut master, b"q");

    thread::sleep(Duration::from_millis(1500));
    let _ = child.kill();
    let output = child
        .wait_with_output()
        .expect("failed to collect kgdb-console-splitting-proxy output");
    let stdout = String::from_utf8_lossy(&output.stdout);

    let title = format!("kgdb-console-splitting-proxy v{}", env!("CARGO_PKG_VERSION"));
    assert!(
        stdout.contains(&title) && stdout.contains("serial-split"),
        "missing mode in status output: {stdout}",
    );
    assert!(
        stdout.contains(&slave_path) && stdout.contains("115200 baud"),
        "missing serial config in status output: {stdout}",
    );
    assert!(
        stdout.contains(&format!("\x1b[33m{slave_path}\x1b[0m")),
        "serial device path is not highlighted in yellow: {stdout:?}",
    );
    assert!(
        stdout.contains("fd=") && stdout.contains("⇣") && stdout.contains("⇡"),
        "missing serial traffic stats in status output: {stdout}",
    );
    assert!(
        stdout.contains(&format!("tcp://localhost:{console_port}"))
            && stdout.contains("1 connected client"),
        "missing console client count in status output: {stdout}",
    );
    assert!(
        stdout.contains(&format!("tcp://localhost:{gdb_port}"))
            && stdout.contains("1 connected client"),
        "missing kgdb client count in status output: {stdout}",
    );
    assert!(
        stdout.contains("console clients")
            && stdout.contains("kgdb clients")
            && stdout.contains("◦"),
        "missing client traffic stats in status output: {stdout}",
    );
    assert!(
        !stdout.contains("traffic\x1b[0m"),
        "aggregate channel traffic should be merged into channel rows: {stdout:?}",
    );
    assert!(
        stdout.contains("0 connected clients") && !stdout.contains("clients\x1b[0m  none"),
        "no-client channel details should stay hidden: {stdout:?}",
    );
    assert!(
        stdout.contains("\x1b[36m")
            && stdout.contains("╭─")
            && stdout.contains("●")
            && stdout.contains("⌁")
            && stdout.contains("▣")
            && stdout.contains("◆")
            && stdout.contains("⇣")
            && stdout.contains("⇡"),
        "status output is missing colors or UTF-8 icons: {stdout:?}",
    );
    assert!(
        stdout.contains("\x1b[") && stdout.contains("A\x1b[J"),
        "status output was not rewritten in place: {stdout:?}",
    );
    assert_status_box_aligned(&stdout);
    assert_traffic_values_right_aligned(&stdout);
}
