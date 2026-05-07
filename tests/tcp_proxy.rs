use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct ProxyProcess {
    child: Child,
}

impl ProxyProcess {
    fn spawn(local_port: u16, remote_port: u16) -> Self {
        let exe = env!("CARGO_BIN_EXE_kgdb-console-splitting-proxy");
        let child = Command::new(exe)
            .arg("proxy")
            .arg("--local")
            .arg(local_port.to_string())
            .arg("--remote")
            .arg(format!("tcp:127.0.0.1:{remote_port}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn kgdb-console-splitting-proxy");

        ProxyProcess { child }
    }

    fn spawn_udp(local_port: u16, remote_port: u16) -> Self {
        let exe = env!("CARGO_BIN_EXE_kgdb-console-splitting-proxy");
        let child = Command::new(exe)
            .arg("proxy")
            .arg("--local")
            .arg(format!("udp:{local_port}"))
            .arg("--remote")
            .arg(format!("udp:127.0.0.1:{remote_port}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn kgdb-console-splitting-proxy");

        ProxyProcess { child }
    }

    fn connect(&mut self, port: u16) -> TcpStream {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(status) = self.child.try_wait().expect("failed to poll proxy") {
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

fn unused_udp_port() -> u16 {
    UdpSocket::bind(("127.0.0.1", 0))
        .expect("failed to bind ephemeral UDP port")
        .local_addr()
        .expect("failed to read local UDP address")
        .port()
}

#[test]
fn forwards_tcp_payload_to_remote_server() {
    let remote_listener =
        TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind remote listener");
    let remote_port = remote_listener
        .local_addr()
        .expect("failed to read remote address")
        .port();

    let server = thread::spawn(move || {
        let (mut stream, _) = remote_listener.accept().expect("remote accept failed");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("failed to set remote read timeout");
        let mut received = [0u8; 5];
        stream
            .read_exact(&mut received)
            .expect("remote read failed");
        received
    });

    let local_port = unused_local_port();
    let mut proxy = ProxyProcess::spawn(local_port, remote_port);
    let mut client = proxy.connect(local_port);

    client.write_all(b"hello").expect("client write failed");
    client
        .shutdown(Shutdown::Write)
        .expect("client shutdown failed");

    assert_eq!(server.join().expect("server thread panicked"), *b"hello");
}

#[test]
fn forwards_udp_replies_to_original_client() {
    let remote = UdpSocket::bind(("127.0.0.1", 0)).expect("failed to bind remote UDP socket");
    remote
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("failed to set remote UDP read timeout");
    let remote_port = remote
        .local_addr()
        .expect("failed to read remote UDP address")
        .port();
    let local_port = unused_udp_port();
    let _proxy = ProxyProcess::spawn_udp(local_port, remote_port);
    let client = UdpSocket::bind(("127.0.0.1", 0)).expect("failed to bind UDP client");
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("failed to set UDP client read timeout");

    let mut buf = [0u8; 64];
    let deadline = Instant::now() + Duration::from_secs(2);
    let (got, proxy_addr) = loop {
        client
            .send_to(b"hi", ("127.0.0.1", local_port))
            .expect("failed to send UDP client packet");
        match remote.recv_from(&mut buf) {
            Ok(result) => break result,
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(25));
            }
            Err(err) => panic!("remote UDP read failed: {err}"),
        }
    };

    assert_eq!(&buf[..got], b"hi");
    remote
        .send_to(b"ok", proxy_addr)
        .expect("failed to send UDP reply");
    let (got, _) = client
        .recv_from(&mut buf)
        .expect("client did not receive UDP reply");

    assert_eq!(&buf[..got], b"ok");
}
