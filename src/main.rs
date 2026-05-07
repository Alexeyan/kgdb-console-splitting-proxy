mod port;
mod rs232;

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag};
use nix::sys::socket::sockopt::{Linger, ReuseAddr, SocketError, TcpNoDelay};
use nix::sys::socket::{
    self as nix_sock, AddressFamily, Backlog, MsgFlags, Shutdown, SockFlag, SockType, SockaddrIn,
};
use port::*;
use std::collections::HashMap;
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddrV4, ToSocketAddrs};
use std::os::fd::{BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::io::RawFd;
use std::process;
use std::time::{Duration, Instant};

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// ----- nix-based socket helpers -----

/// Wrap a RawFd as a BorrowedFd<'static> for use with nix APIs that take AsFd.
///
/// # Safety
/// Caller must ensure `fd` remains a valid, open descriptor for the duration
/// of any use of the returned `BorrowedFd`.
fn borrow_fd(fd: RawFd) -> BorrowedFd<'static> {
    debug_assert!(fd >= 0, "borrow_fd called with invalid fd");
    unsafe { BorrowedFd::borrow_raw(fd) }
}

fn c_socket(sock_type: SockType) -> nix::Result<RawFd> {
    nix_sock::socket(AddressFamily::Inet, sock_type, SockFlag::empty(), None)
        .map(IntoRawFd::into_raw_fd)
}

fn c_bind(fd: RawFd, addr: &SockaddrIn) -> nix::Result<()> {
    nix_sock::bind(fd, addr)
}

fn c_connect(fd: RawFd, addr: &SockaddrIn) -> nix::Result<()> {
    nix_sock::connect(fd, addr)
}

fn c_listen(fd: RawFd, backlog: i32) -> nix::Result<()> {
    nix_sock::listen(&borrow_fd(fd), Backlog::new(backlog)?)
}

fn c_accept(fd: RawFd) -> nix::Result<RawFd> {
    nix_sock::accept(fd)
}

fn make_sockaddr(ip: Ipv4Addr, port: u16) -> SockaddrIn {
    SockaddrIn::from(SocketAddrV4::new(ip, port))
}

fn errno_is_interrupted(errno: Errno) -> bool {
    errno == Errno::EINTR
}

fn errno_is_would_block(errno: Errno) -> bool {
    errno == Errno::EAGAIN || errno == Errno::EWOULDBLOCK
}

fn errno_is_connect_pending(errno: Errno) -> bool {
    errno == Errno::EINPROGRESS || errno == Errno::EALREADY
}

fn last_errno() -> Errno {
    Errno::last()
}

fn set_nonblocking(fd: RawFd) {
    let bfd = borrow_fd(fd);
    if let Ok(flags) = nix::fcntl::fcntl(bfd, FcntlArg::F_GETFL) {
        let oflag = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
        let _ = nix::fcntl::fcntl(bfd, FcntlArg::F_SETFL(oflag));
    }
}

fn socket_pending_error(fd: RawFd) -> nix::Result<i32> {
    nix_sock::getsockopt(&borrow_fd(fd), SocketError)
}

fn set_remote_sock_opts(fd: RawFd) {
    let bfd = borrow_fd(fd);
    let linger = libc::linger {
        l_onoff: 0,
        l_linger: 0,
    };
    let _ = nix_sock::setsockopt(&bfd, Linger, &linger);
    let _ = nix_sock::setsockopt(&bfd, TcpNoDelay, &true);
}

fn set_reuseaddr(fd: RawFd) {
    let _ = nix_sock::setsockopt(&borrow_fd(fd), ReuseAddr, &true);
}

fn fd_is_valid(fd: RawFd) -> bool {
    if fd < 0 {
        return false;
    }
    match nix::fcntl::fcntl(borrow_fd(fd), FcntlArg::F_GETFD) {
        Ok(_) => true,
        Err(Errno::EBADF) => false,
        Err(_) => true,
    }
}

fn close_fd(fd: RawFd) {
    if fd >= 0 {
        // SAFETY: caller asserts they own this fd; OwnedFd takes ownership and
        // nix::unistd::close consumes it via IntoRawFd.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        let _ = nix::unistd::close(owned);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::fcntl::OFlag;
    use nix::pty::{PtyMaster, grantpt, posix_openpt, ptsname_r, unlockpt};
    use std::io::Write;
    use std::os::unix::fs::symlink;
    use std::sync::{Mutex, MutexGuard, OnceLock};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn serial_test_guard() -> MutexGuard<'static, ()> {
        static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn new_pty() -> (PtyMaster, String) {
        let master = posix_openpt(OFlag::O_RDWR).expect("posix_openpt failed");
        grantpt(&master).expect("grantpt failed");
        unlockpt(&master).expect("unlockpt failed");
        let slave = ptsname_r(&master).expect("ptsname_r failed");
        (master, slave)
    }

    fn unique_temp_dir(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("kgdb-console-splitting-proxy-{name}-{unique}"));
        std::fs::create_dir(&dir).expect("failed to create temporary directory");
        dir
    }

    fn add_serial_port(state: &mut ProxyState, path: &str, baud: Option<u32>) -> PortId {
        let sock = open_serial_device(path, baud).expect("failed to open test serial device");
        let id = state.alloc_port(PortType::Rs232, PortClass::Remote);
        {
            let port = state.ports.get_mut(&id).unwrap();
            port.name = path.to_string();
            port.sock = sock;
            port.serial_baud = baud;
            port.serial_config_path = Some(path.to_string());
            port.serial_check_at = Some(Instant::now() + SERIAL_LIVENESS_INTERVAL);
        }
        state.master_rds.insert(sock);
        state.refresh_nsockhandle();
        id
    }

    #[test]
    fn serial_reconnect_reopens_configured_device() {
        let _guard = serial_test_guard();
        let (_master1, slave1) = new_pty();
        let temp_dir = unique_temp_dir("serial-reconnect");
        let serial_link = temp_dir.join("ttyUSB0");
        symlink(&slave1, &serial_link).expect("failed to create serial symlink");
        let serial_path = serial_link.to_string_lossy().into_owned();
        let mut state = ProxyState::new();
        let serial_id = add_serial_port(&mut state, &serial_path, Some(115200));
        let client_id = state.alloc_port(PortType::Tcp, PortClass::Connection);
        state
            .ports
            .get_mut(&serial_id)
            .unwrap()
            .clients
            .push(client_id);

        mark_serial_disconnected(&mut state, serial_id, "test disconnect");

        let serial = state.ports.get(&serial_id).expect("missing serial port");
        assert_eq!(serial.sock, -1);
        assert!(serial.serial_reconnect_at.is_some());
        assert!(serial.clients.contains(&client_id));
        assert!(state.ports.contains_key(&client_id));

        let (mut master2, slave2) = new_pty();
        std::fs::remove_file(&serial_link).expect("failed to remove old serial symlink");
        let serial_link2 = temp_dir.join("ttyUSB1");
        symlink(&slave2, &serial_link2).expect("failed to create sibling serial symlink");
        state.ports.get_mut(&serial_id).unwrap().serial_reconnect_at = Some(Instant::now());

        retry_due_serial_reconnects(&mut state);

        let serial = state.ports.get(&serial_id).expect("missing serial port");
        assert!(serial.sock >= 0);
        assert!(serial.serial_reconnect_at.is_none());
        assert_eq!(serial.name, serial_link2.to_string_lossy());
        assert!(serial.clients.contains(&client_id));

        master2.write_all(b"resume\n").expect("pty write failed");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut buf = [0u8; 64];
        loop {
            let got = tracked_port_read(&mut state, serial_id, &mut buf, false);
            if got > 0 {
                assert_eq!(&buf[..got as usize], b"resume\n");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "serial reconnect did not pass data"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let sock = state.ports.get(&serial_id).unwrap().sock;
        close_fd(sock);
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn invalid_serial_fd_is_removed_from_select_sets() {
        let _guard = serial_test_guard();
        let (_master, slave) = new_pty();
        let mut state = ProxyState::new();
        let serial_id = add_serial_port(&mut state, &slave, Some(115200));
        let sock = state.ports.get(&serial_id).unwrap().sock;
        close_fd(sock);

        assert_eq!(cleanup_invalid_fds(&mut state), 1);

        let serial = state.ports.get(&serial_id).expect("missing serial port");
        assert_eq!(serial.sock, -1);
        assert!(serial.serial_reconnect_at.is_some());
        assert!(!state.master_rds.contains(sock));
        assert!(!state.master_wds.contains(sock));
    }

    #[test]
    fn disconnected_serial_status_uses_red_path_and_x() {
        let serial_split = SerialSplitConfig {
            device: "/dev/ttyUSB0".to_string(),
            baud: "115200".to_string(),
            console_port: 4440,
            gdb_port: 4441,
        };
        let mut serial = Port::new(0, PortType::Rs232, PortClass::Remote);
        serial.name = "/dev/ttyUSB1".to_string();
        serial.sock = -1;
        serial.rx_bytes = 12;
        serial.tx_bytes = 34;

        let row = serial_status_row(&serial_split, Some(&serial));

        assert!(
            row.left.contains(&format!("{ANSI_RED}✕{ANSI_RESET}")),
            "missing red disconnected marker: {:?}",
            row.left,
        );
        assert!(
            row.left
                .contains(&format!("{ANSI_RED}/dev/ttyUSB1{ANSI_RESET}")),
            "serial path is not red: {:?}",
            row.left,
        );
        assert!(
            row.left.contains("disconnected"),
            "missing disconnected label: {:?}",
            row.left,
        );
        assert!(row.right.is_some(), "missing traffic column");
    }
}

fn open_serial_device(dev_path: &str, baud: Option<u32>) -> Result<RawFd, String> {
    let owned = nix::fcntl::open(
        dev_path,
        OFlag::O_RDWR | OFlag::O_NOCTTY | OFlag::O_NONBLOCK,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|err| {
        let sudo_hint = if matches!(err, Errno::EACCES | Errno::EPERM) {
            " Run kgdb-console-splitting-proxy with sudo."
        } else {
            ""
        };
        format!("Error opening serial device {dev_path}: {err}.{sudo_hint}")
    })?;
    let sock = owned.into_raw_fd();

    let configure_result = (|| {
        if let Some(baud) = baud {
            rs232::setbaudrate(sock, baud)?;
        }
        rs232::setstopbits(sock, "1")?;
        rs232::setcondefaults(sock)?;
        Ok::<(), String>(())
    })();

    if let Err(err) = configure_result {
        close_fd(sock);
        return Err(err);
    }

    set_nonblocking(sock);
    Ok(sock)
}

fn serial_numeric_prefix(path: &str) -> Option<(std::path::PathBuf, String)> {
    let path = std::path::Path::new(path);
    let dir = path.parent()?.to_path_buf();
    let file_name = path.file_name()?.to_str()?;
    let prefix_len = file_name
        .char_indices()
        .rev()
        .find_map(|(idx, ch)| (!ch.is_ascii_digit()).then_some(idx + ch.len_utf8()))?;
    if prefix_len >= file_name.len() {
        return None;
    }
    Some((dir, file_name[..prefix_len].to_string()))
}

fn serial_reconnect_candidates(configured_path: &str) -> Vec<String> {
    let mut candidates = vec![configured_path.to_string()];
    let Some((dir, prefix)) = serial_numeric_prefix(configured_path) else {
        return candidates;
    };

    let Ok(entries) = std::fs::read_dir(dir) else {
        return candidates;
    };

    let mut siblings: Vec<(u32, String)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_name = entry.file_name();
            let file_name = file_name.to_str()?;
            let suffix = file_name.strip_prefix(&prefix)?;
            if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let index = suffix.parse::<u32>().ok()?;
            Some((index, entry.path().to_string_lossy().into_owned()))
        })
        .collect();
    siblings.sort_by_key(|(index, path)| (*index, path.clone()));

    for (_, path) in siblings {
        if !candidates.iter().any(|candidate| candidate == &path) {
            candidates.push(path);
        }
    }

    candidates
}

fn open_serial_candidate(
    configured_path: &str,
    baud: Option<u32>,
) -> Result<(RawFd, String), String> {
    let mut last_err = None;
    for candidate in serial_reconnect_candidates(configured_path) {
        match open_serial_device(&candidate, baud) {
            Ok(fd) => return Ok((fd, candidate)),
            Err(err) => last_err = Some(err),
        }
    }

    Err(last_err.unwrap_or_else(|| format!("No serial candidates for {configured_path}")))
}

// ----- I/O helpers -----

fn port_read(port_type: PortType, fd: RawFd, buf: &mut [u8], oob: bool) -> isize {
    let flags = if oob {
        MsgFlags::MSG_OOB
    } else {
        MsgFlags::empty()
    };
    let result = match port_type {
        PortType::Tcp | PortType::Listen | PortType::FifoCon | PortType::Udp => {
            nix_sock::recv(fd, buf, flags)
        }
        PortType::Rs232 | PortType::StdinOut => nix::unistd::read(borrow_fd(fd), buf),
    };
    match result {
        Ok(n) => n as isize,
        Err(_) => -1,
    }
}

fn port_write(port_type: PortType, fd: RawFd, buf: &[u8], oob: bool) -> isize {
    let flags = if oob {
        MsgFlags::MSG_OOB
    } else {
        MsgFlags::empty()
    };
    match port_type {
        PortType::Tcp | PortType::Listen | PortType::FifoCon | PortType::Udp => {
            match nix_sock::send(fd, buf, flags) {
                Ok(n) => n as isize,
                Err(_) => -1,
            }
        }
        PortType::Rs232 => match nix::unistd::write(borrow_fd(fd), buf) {
            Ok(n) => n as isize,
            Err(_) => -1,
        },
        PortType::StdinOut => match io::stdout().write(buf) {
            Ok(n) => n as isize,
            Err(_) => -1,
        },
    }
}

fn tracked_port_read(state: &mut ProxyState, port_id: PortId, buf: &mut [u8], oob: bool) -> isize {
    let Some(port) = state.ports.get(&port_id) else {
        return -1;
    };
    let got = if port.port_type == PortType::Udp && !oob {
        match nix_sock::recvfrom::<SockaddrIn>(port.sock, buf) {
            Ok((n, src)) => {
                if n > 0 {
                    if let Some(src_addr) = src {
                        if let Some(port) = state.ports.get_mut(&port_id) {
                            port.serv_addr = Some(SocketAddrV4::from(src_addr));
                        }
                    }
                }
                n as isize
            }
            Err(_) => -1,
        }
    } else {
        port_read(port.port_type, port.sock, buf, oob)
    };
    if got > 0 {
        state.record_rx(port_id, got as usize);
    }
    got
}

fn port_write_once(state: &mut ProxyState, port_id: PortId, buf: &[u8], oob: bool) -> isize {
    let Some(port) = state.ports.get(&port_id) else {
        return -1;
    };

    let got = if port.port_type == PortType::Udp && !oob {
        let Some(addr) = port.serv_addr else {
            return -1;
        };
        let sockaddr = SockaddrIn::from(addr);
        match nix_sock::sendto(port.sock, buf, &sockaddr, MsgFlags::empty()) {
            Ok(n) => n as isize,
            Err(_) => -1,
        }
    } else {
        port_write(port.port_type, port.sock, buf, oob)
    };

    if got > 0 {
        state.record_tx(port_id, got as usize);
    }
    got
}

fn queue_pending_write(state: &mut ProxyState, port_id: PortId, buf: &[u8]) -> bool {
    let mut sock = None;
    if let Some(port) = state.ports.get_mut(&port_id) {
        if port.tx_buf.len().saturating_add(buf.len()) > MAX_PENDING_WRITE {
            return false;
        }
        port.tx_buf.extend_from_slice(buf);
        sock = Some(port.sock);
    }
    if let Some(sock) = sock {
        state.master_wds.insert(sock);
    }
    true
}

fn flush_pending_write(state: &mut ProxyState, port_id: PortId) -> bool {
    loop {
        let Some(port) = state.ports.get(&port_id) else {
            return true;
        };
        if port.connecting {
            state.master_wds.insert(port.sock);
            return true;
        }
        if port.tx_buf.is_empty() {
            state.master_wds.remove(port.sock);
            return true;
        }

        let pending_len = port.tx_buf.len().min(IO_BUFSIZE);
        let pending = port.tx_buf[..pending_len].to_vec();
        let got = port_write_once(state, port_id, &pending, false);
        if got > 0 {
            let got = got as usize;
            if let Some(port) = state.ports.get_mut(&port_id) {
                port.tx_buf.drain(..got);
            }
            continue;
        }

        let errno = last_errno();
        if errno_is_interrupted(errno) {
            continue;
        }
        if errno_is_would_block(errno) {
            if let Some(port) = state.ports.get(&port_id) {
                state.master_wds.insert(port.sock);
            }
            return true;
        }
        return false;
    }
}

fn tracked_port_write(state: &mut ProxyState, port_id: PortId, buf: &[u8], oob: bool) -> isize {
    if buf.is_empty() {
        return 0;
    }

    let Some(port) = state.ports.get(&port_id) else {
        return -1;
    };
    let port_type = port.port_type;
    if port.connecting && port_type != PortType::Udp && !oob {
        return if queue_pending_write(state, port_id, buf) {
            buf.len() as isize
        } else {
            -1
        };
    }
    if !port.tx_buf.is_empty() && port_type != PortType::Udp && !oob {
        return if queue_pending_write(state, port_id, buf) {
            buf.len() as isize
        } else {
            -1
        };
    }

    let mut written = 0;
    while written < buf.len() {
        let got = port_write_once(state, port_id, &buf[written..], oob);
        if got > 0 {
            written += got as usize;
            continue;
        }

        let errno = last_errno();
        if got < 0 && errno_is_interrupted(errno) {
            continue;
        }
        if got < 0
            && errno_is_would_block(errno)
            && port_type != PortType::Udp
            && port_type != PortType::StdinOut
            && !oob
        {
            return if queue_pending_write(state, port_id, &buf[written..]) {
                buf.len() as isize
            } else {
                -1
            };
        }
        return -1;
    }

    buf.len() as isize
}

fn port_close(port_type: PortType, fd: RawFd) {
    if matches!(
        port_type,
        PortType::Tcp | PortType::Listen | PortType::FifoCon
    ) {
        let _ = nix_sock::shutdown(fd, Shutdown::Both);
    }
    close_fd(fd);
}

// ----- FdSet wrapper -----

#[derive(Clone)]
struct FdSet {
    inner: nix::sys::select::FdSet<'static>,
}

impl FdSet {
    fn new() -> Self {
        FdSet {
            inner: nix::sys::select::FdSet::new(),
        }
    }

    fn insert(&mut self, fd: RawFd) {
        if fd >= 0 && (fd as usize) < nix::sys::select::FD_SETSIZE {
            self.inner.insert(borrow_fd(fd));
        } else if (fd as usize) >= nix::sys::select::FD_SETSIZE {
            eprintln!(
                "Error: fd {} >= FD_SETSIZE ({})",
                fd,
                nix::sys::select::FD_SETSIZE
            );
            process::exit(1);
        }
    }

    fn remove(&mut self, fd: RawFd) {
        if fd >= 0 && (fd as usize) < nix::sys::select::FD_SETSIZE {
            self.inner.remove(borrow_fd(fd));
        }
    }

    fn contains(&self, fd: RawFd) -> bool {
        if fd >= 0 && (fd as usize) < nix::sys::select::FD_SETSIZE {
            self.inner.contains(borrow_fd(fd))
        } else {
            false
        }
    }
}

// ----- Global proxy state -----

struct ProxyState {
    ports: HashMap<PortId, Port>,
    next_id: PortId,
    master_rds: FdSet,
    master_wds: FdSet,
    nsockhandle: i32,
    local_port_id: PortId,
    remote_port_id: PortId,

    debug: bool,
    logchar: bool,
    break_on_connect: bool,
    gdb_split: bool,
    telnet_negotiation: bool,

    gdb_arr: Vec<u8>,
    gdb_ptr: usize,
    gdb_got_dollar: u32,

    break_str: Vec<u8>,

    listen_fd: RawFd,
    fifo_con_fd: RawFd,
    fifo_con_file: Option<String>,
    fifo_buf: Vec<u8>,
    fifo_idx: usize,
    status_dirty: bool,
    status_line_count: usize,
}

const MAX_GDB_BUF: usize = 1024 * 8;
const MAX_FIFO_BUF: usize = 50;
const MAX_PENDING_WRITE: usize = 1024 * 1024;
const SERIAL_RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
const SERIAL_LIVENESS_INTERVAL: Duration = Duration::from_secs(1);
const DEFAULT_SERIAL_DEVICE: &str = "/dev/ttyUSB0";
const DEFAULT_CONSOLE_PORT: &str = "4440";
const DEFAULT_GDB_PORT: &str = "4441";
const STATUS_INTERVAL: Duration = Duration::from_secs(1);

impl ProxyState {
    fn new() -> Self {
        ProxyState {
            ports: HashMap::new(),
            next_id: 0,
            master_rds: FdSet::new(),
            master_wds: FdSet::new(),
            nsockhandle: 0,
            local_port_id: 0,
            remote_port_id: 0,
            debug: false,
            logchar: false,
            break_on_connect: true,
            gdb_split: true,
            telnet_negotiation: false,
            gdb_arr: vec![0u8; MAX_GDB_BUF],
            gdb_ptr: 0,
            gdb_got_dollar: 0,
            break_str: vec![0xff, 0xf3, b'g'],
            listen_fd: -1,
            fifo_con_fd: -1,
            fifo_con_file: None,
            fifo_buf: vec![0u8; MAX_FIFO_BUF],
            fifo_idx: 0,
            status_dirty: true,
            status_line_count: 0,
        }
    }

    fn alloc_port(&mut self, port_type: PortType, cls: PortClass) -> PortId {
        let id = self.next_id;
        self.next_id += 1;
        self.ports.insert(id, Port::new(id, port_type, cls));
        id
    }

    fn remove_port(&mut self, id: PortId) -> Option<Port> {
        let removed = self.ports.remove(&id);
        if removed.is_some() {
            self.status_dirty = true;
        }
        removed
    }

    fn refresh_nsockhandle(&mut self) {
        let mut max_fd: RawFd = -1;
        for p in self.ports.values() {
            if p.sock > max_fd {
                max_fd = p.sock;
            }
        }
        self.nsockhandle = max_fd + 1;
    }

    fn record_rx(&mut self, id: PortId, bytes: usize) {
        if let Some(port) = self.ports.get_mut(&id) {
            port.rx_bytes = port.rx_bytes.saturating_add(bytes as u64);
            self.status_dirty = true;
        }
    }

    fn record_tx(&mut self, id: PortId, bytes: usize) {
        if let Some(port) = self.ports.get_mut(&id) {
            port.tx_bytes = port.tx_bytes.saturating_add(bytes as u64);
            self.status_dirty = true;
        }
    }
}

// ----- Usage -----

struct CliArgs {
    proxy_args: Vec<String>,
    pidfile: Option<String>,
    do_fork: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CliMode {
    Proxy,
    SerialSplit,
}

struct RuntimeConfig {
    serial_split: Option<SerialSplitConfig>,
}

struct SerialSplitConfig {
    device: String,
    baud: String,
    console_port: u16,
    gdb_port: u16,
}

impl RuntimeConfig {
    fn from_state(state: &ProxyState, proxy_args: &[String]) -> Self {
        let Some(local) = state.ports.get(&state.local_port_id) else {
            return RuntimeConfig { serial_split: None };
        };
        let Some(remote) = state.ports.get(&state.remote_port_id) else {
            return RuntimeConfig { serial_split: None };
        };
        let Some(script_id) = local.script_ref else {
            return RuntimeConfig { serial_split: None };
        };
        let Some(script) = state.ports.get(&script_id) else {
            return RuntimeConfig { serial_split: None };
        };

        if remote.port_type != PortType::Rs232 || !script.break_port {
            return RuntimeConfig { serial_split: None };
        }

        let remote_arg = proxy_args.get(2).map(String::as_str).unwrap_or("");
        let (device, baud) = match remote_arg.split_once(',') {
            Some((device, baud)) => (device.to_string(), baud.to_string()),
            None => (remote_arg.to_string(), "default".to_string()),
        };

        RuntimeConfig {
            serial_split: Some(SerialSplitConfig {
                device,
                baud,
                console_port: local.port_num,
                gdb_port: script.port_num,
            }),
        }
    }
}

fn print_usage() {
    println!("kgdb-console-splitting-proxy version {AGENT_VERSION}");
    println!();
    println!("Usage:");
    println!("  kgdb-console-splitting-proxy proxy [options] --local <endpoint> --remote <endpoint>");
    println!("  kgdb-console-splitting-proxy serial-split [options]");
    println!("  kgdb-console-splitting-proxy [options] <local> <remote-host> <remote>    # legacy form");
    println!();
    println!("Local endpoints for `proxy`:");
    println!("  <port>                  listen on localhost TCP port");
    println!("  <bind-ip>:<port>        listen on a specific local TCP address");
    println!("  udp:<port>              bind a local UDP port");
    println!("  udp:<bind-ip>:<port>    bind a UDP port on a specific local address");
    println!("  stdin                   proxy stdin/stdout instead of a local socket");
    println!("  <local>+<script-port>   add a TCP script/control listener");
    println!("  <local>^<kgdb-port>     split console/log data from GDB remote packets");
    println!();
    println!("Remote endpoints for `proxy`:");
    println!("  tcp:<host>:<port>             connect to a remote TCP port");
    println!("  udp:<host>:<port>             connect to a remote UDP port");
    println!("  udp:<host>:<src-port>:<port>  bind a local UDP source port first");
    println!("  serial:<path>[,<baud>]        open a Unix serial device");
    println!("  tcplisten:<bind-ip>:<port>    accept a TCP connection from the remote side");
    println!("  fifocon:<path>                accept console-selected TCP ports via FIFO");
    println!();
    println!("Options:");
    println!("  -h, --help                    show this help");
    println!("      --version                 show the version");
    println!("  -v, --verbose                 verbose connection logging");
    println!("  -d, --log-chars               log proxied characters");
    println!("  -D, --daemon                  fork into the background");
    println!("  -f, --pid-file <file>         write the proxy pid to a file");
    println!("  -B, --no-break-on-connect     do not send break when a kgdb client connects");
    println!("  -G, --no-gdb-filter           do not filter console output to GDB packets");
    println!("  -s, --break-byte <byte>       send byte instead of the default break sequence");
    println!("      --telnet                  enable Telnet negotiation on serial console clients");
    println!("      --remote-host <host>      use legacy remote syntax with --remote <remote>");
    println!();
    println!("Serial splitter options:");
    println!(
        "      --device <path>           serial device; defaults to /dev/ttyUSB0 if it exists"
    );
    println!("      --baud <rate>             serial baud rate, default: 115200");
    println!("      --console-port <port>     TCP port for console/log output, default: 4440");
    println!("      --gdb-port <port>         TCP port for kgdb/GDB traffic, default: 4441");
    println!();
    println!("Examples:");
    println!("  kgdb-console-splitting-proxy proxy --local 5550 --remote tcp:10.0.0.2:2004");
    println!("  kgdb-console-splitting-proxy proxy --local udp:3331 --remote udp:10.0.0.2:6443");
    println!("  kgdb-console-splitting-proxy proxy --local 47.1.1.3:44444 --remote tcp:10.0.0.3:44444");
    println!(
        "  sudo kgdb-console-splitting-proxy serial-split --device /dev/ttyUSB0 --baud 115200 --console-port 4440 --gdb-port 4441"
    );
    println!("  sudo kgdb-console-splitting-proxy serial-split");
    println!("  ncat localhost 4440     # console/log output");
    println!("  ncat localhost 4441     # kgdb/GDB traffic");
    println!();
    println!("This Rust rewrite currently targets Unix-like hosts and serial paths");
    println!("such as /dev/ttyUSB0; Windows COM ports are not implemented.");
}

fn try_help() -> ! {
    println!("Try 'kgdb-console-splitting-proxy --help' for more information");
    process::exit(1);
}

fn usage() -> ! {
    print_usage();
    process::exit(1);
}

fn help() -> ! {
    print_usage();
    process::exit(0);
}

fn version() -> ! {
    println!("kgdb-console-splitting-proxy {AGENT_VERSION}");
    process::exit(0);
}

fn write_pid_file_or_exit(path: &str, pid: u32) {
    let result = std::fs::File::create(path).and_then(|mut f| write!(f, "{pid}"));
    if let Err(err) = result {
        eprintln!("ERROR: Could not write pid file {path}: {err}");
        process::exit(1);
    }
}

fn parse_port_number(s: &str) -> u16 {
    if let Some(hex) = s.strip_prefix("0x") {
        u16::from_str_radix(hex, 16).unwrap_or_else(|_| {
            eprintln!("Invalid port number: {}", s);
            process::exit(1);
        })
    } else {
        s.parse::<u16>().unwrap_or_else(|_| {
            eprintln!("Invalid port number: {}", s);
            process::exit(1);
        })
    }
}

fn resolve_host(host: &str) -> Result<Ipv4Addr, String> {
    if host == "0" || host == "0.0.0.0" {
        Ok(Ipv4Addr::UNSPECIFIED)
    } else if host == "127.0.0.1" || host == "localhost" {
        Ok(Ipv4Addr::LOCALHOST)
    } else if let Ok(ip) = host.parse::<Ipv4Addr>() {
        Ok(ip)
    } else {
        let addr_str = format!("{}:0", host);
        let resolved = addr_str
            .to_socket_addrs()
            .map_err(|e| format!("Could not lookup hostname: {}: {}", host, e))?
            .find(|a| a.is_ipv4())
            .ok_or_else(|| format!("Could not resolve hostname: {}", host))?;
        match resolved {
            std::net::SocketAddr::V4(a) => Ok(*a.ip()),
            _ => unreachable!(),
        }
    }
}

fn require_arg(args: &[String], i: &mut usize, opt: &str) -> String {
    *i += 1;
    if *i >= args.len() {
        eprintln!("No argument specified for {opt}");
        usage();
    }
    args[*i].clone()
}

fn set_break_byte(state: &mut ProxyState, val_str: &str) {
    let val: u8 = val_str.parse().unwrap_or_else(|_| {
        eprintln!("Invalid break byte: {val_str}");
        usage();
    });
    state.break_str = vec![val];
}

fn split_once_nonempty<'a>(value: &'a str, context: &str) -> (&'a str, &'a str) {
    let Some((left, right)) = value.split_once(':') else {
        eprintln!("{context} must include host and port: {value}");
        usage();
    };
    if left.is_empty() || right.is_empty() {
        eprintln!("{context} must include non-empty host and port: {value}");
        usage();
    }
    (left, right)
}

fn parse_remote_endpoint(remote: &str, remote_host: Option<String>) -> (String, String) {
    if let Some(host) = remote_host {
        return (host, remote.to_string());
    }

    if let Some(rest) = remote
        .strip_prefix("tcp://")
        .or_else(|| remote.strip_prefix("tcp:"))
    {
        let (host, port) = split_once_nonempty(rest, "TCP remote endpoint");
        return (host.to_string(), port.to_string());
    }

    if let Some(rest) = remote
        .strip_prefix("udp://")
        .or_else(|| remote.strip_prefix("udp:"))
    {
        let (host, ports) = split_once_nonempty(rest, "UDP remote endpoint");
        return (host.to_string(), format!("udp:{ports}"));
    }

    if let Some(rest) = remote.strip_prefix("serial:") {
        if rest.is_empty() {
            eprintln!("serial remote endpoint must include a device path");
            usage();
        }
        return ("0".to_string(), rest.to_string());
    }

    if let Some(rest) = remote.strip_prefix("tcplisten:") {
        let (bind_ip, port) = split_once_nonempty(rest, "tcplisten remote endpoint");
        return (bind_ip.to_string(), format!("tcplisten:{port}"));
    }

    if let Some(rest) = remote.strip_prefix("fifocon:") {
        if rest.is_empty() {
            eprintln!("fifocon remote endpoint must include a path");
            usage();
        }
        return ("0".to_string(), format!("fifocon:{rest}"));
    }

    eprintln!("Remote endpoint must start with tcp:, udp:, serial:, tcplisten:, or fifocon:");
    usage();
}

fn default_serial_device() -> Option<String> {
    if std::path::Path::new(DEFAULT_SERIAL_DEVICE).exists() {
        Some(DEFAULT_SERIAL_DEVICE.to_string())
    } else {
        None
    }
}

fn parse_cli(args: &[String], state: &mut ProxyState) -> CliArgs {
    let mut mode: Option<CliMode> = None;
    let mut legacy_args: Vec<String> = Vec::new();
    let mut local: Option<String> = None;
    let mut remote: Option<String> = None;
    let mut remote_host: Option<String> = None;
    let mut device: Option<String> = None;
    let mut baud: Option<String> = None;
    let mut console_port: Option<String> = None;
    let mut gdb_port: Option<String> = None;
    let mut pidfile: Option<String> = None;
    let mut do_fork = false;
    let mut end_of_options = false;

    let mut i = 1;
    while i < args.len() {
        let s = &args[i];

        if !end_of_options && s == "--" {
            end_of_options = true;
            i += 1;
            continue;
        }

        if !end_of_options && !s.starts_with('-') {
            if mode.is_none() && s == "proxy" {
                mode = Some(CliMode::Proxy);
            } else if mode.is_none() && s == "serial-split" {
                mode = Some(CliMode::SerialSplit);
            } else {
                legacy_args.push(s.clone());
            }
            i += 1;
            continue;
        }

        if !end_of_options && s.starts_with("--") {
            let (name, inline_value) = match s.split_once('=') {
                Some((name, value)) => (name, Some(value.to_string())),
                None => (s.as_str(), None),
            };
            let value = |opt: &str, i: &mut usize| {
                inline_value
                    .clone()
                    .unwrap_or_else(|| require_arg(args, i, opt))
            };
            let no_value = |opt: &str| {
                if inline_value.is_some() {
                    eprintln!("Option {opt} does not take a value");
                    usage();
                }
            };

            match name {
                "--help" => {
                    no_value(name);
                    help();
                }
                "--version" => {
                    no_value(name);
                    version();
                }
                "--verbose" => {
                    no_value(name);
                    state.debug = true;
                }
                "--log-chars" => {
                    no_value(name);
                    state.logchar = true;
                }
                "--daemon" => {
                    no_value(name);
                    do_fork = true;
                }
                "--pid-file" => pidfile = Some(value(name, &mut i)),
                "--no-break-on-connect" => {
                    no_value(name);
                    state.break_on_connect = false;
                }
                "--no-gdb-filter" => {
                    no_value(name);
                    state.gdb_split = false;
                }
                "--break-byte" => set_break_byte(state, &value(name, &mut i)),
                "--telnet" | "--telnet-negotiation" => {
                    no_value(name);
                    state.telnet_negotiation = true;
                }
                "--local" => local = Some(value(name, &mut i)),
                "--remote" => remote = Some(value(name, &mut i)),
                "--remote-host" => remote_host = Some(value(name, &mut i)),
                "--device" => {
                    mode = Some(CliMode::SerialSplit);
                    device = Some(value(name, &mut i));
                }
                "--baud" => {
                    mode = Some(CliMode::SerialSplit);
                    baud = Some(value(name, &mut i));
                }
                "--console-port" => {
                    mode = Some(CliMode::SerialSplit);
                    console_port = Some(value(name, &mut i));
                }
                "--gdb-port" | "--kgdb-port" => {
                    mode = Some(CliMode::SerialSplit);
                    gdb_port = Some(value(name, &mut i));
                }
                _ => {
                    eprintln!("Option {name} not recognized");
                    usage();
                }
            }
            i += 1;
            continue;
        }

        if !end_of_options && s.starts_with('-') {
            let opt_chars: Vec<char> = s[1..].chars().collect();
            let mut j = 0;
            while j < opt_chars.len() {
                match opt_chars[j] {
                    'h' => help(),
                    'd' => state.logchar = true,
                    'v' => state.debug = true,
                    'D' => do_fork = true,
                    'f' => {
                        pidfile = Some(require_arg(args, &mut i, "-f"));
                    }
                    'G' => state.gdb_split = false,
                    'B' => state.break_on_connect = false,
                    's' => {
                        let val_str = if j + 1 < opt_chars.len() {
                            let rest: String = opt_chars[j + 1..].iter().collect();
                            j = opt_chars.len();
                            rest
                        } else {
                            require_arg(args, &mut i, "-s")
                        };
                        set_break_byte(state, &val_str);
                    }
                    c => {
                        eprintln!("Option -{c} not recognized");
                        usage();
                    }
                }
                j += 1;
            }
            i += 1;
            continue;
        }

        legacy_args.push(s.clone());
        i += 1;
    }

    if mode == Some(CliMode::SerialSplit) {
        if !legacy_args.is_empty() || local.is_some() || remote.is_some() || remote_host.is_some() {
            eprintln!("serial-split uses --device, --console-port, and --gdb-port");
            usage();
        }
        let device = match device {
            Some(device) => device,
            None => default_serial_device().unwrap_or_else(|| {
                eprintln!(
                    "serial-split requires explicit --device because default device {DEFAULT_SERIAL_DEVICE} does not exist"
                );
                try_help();
            }),
        };
        let console_port = console_port.unwrap_or_else(|| DEFAULT_CONSOLE_PORT.to_string());
        let gdb_port = gdb_port.unwrap_or_else(|| DEFAULT_GDB_PORT.to_string());
        let baud = baud.unwrap_or_else(|| "115200".to_string());
        let console_port = parse_port_number(&console_port).to_string();
        let gdb_port = parse_port_number(&gdb_port).to_string();
        let local_arg = format!("{console_port}^{gdb_port}");
        let remote_arg = format!("{device},{baud}");

        return CliArgs {
            proxy_args: vec![local_arg, "0".to_string(), remote_arg],
            pidfile,
            do_fork,
        };
    }

    if mode == Some(CliMode::Proxy) || local.is_some() || remote.is_some() || remote_host.is_some()
    {
        if !legacy_args.is_empty() {
            eprintln!("proxy mode uses --local and --remote, not positional endpoints");
            try_help();
        }
        let Some(local) = local else {
            eprintln!("proxy mode requires --local");
            try_help();
        };
        let Some(remote) = remote else {
            eprintln!("proxy mode requires --remote");
            try_help();
        };
        let (host, remote_arg) = parse_remote_endpoint(&remote, remote_host);

        return CliArgs {
            proxy_args: vec![local, host, remote_arg],
            pidfile,
            do_fork,
        };
    }

    if legacy_args.len() != 3 {
        usage();
    }

    CliArgs {
        proxy_args: legacy_args,
        pidfile,
        do_fork,
    }
}

fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.1} GiB", bytes_f / GIB)
    } else if bytes_f >= MIB {
        format!("{:.1} MiB", bytes_f / MIB)
    } else if bytes_f >= KIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else {
        format!("{bytes} B")
    }
}

const ANSI_RESET: &str = "\x1b[0m";
const ANSI_BOLD: &str = "\x1b[1m";
const ANSI_DIM: &str = "\x1b[2m";
const ANSI_BLUE: &str = "\x1b[34m";
const ANSI_CYAN: &str = "\x1b[36m";
const ANSI_GREEN: &str = "\x1b[32m";
const ANSI_MAGENTA: &str = "\x1b[35m";
const ANSI_RED: &str = "\x1b[31m";
const ANSI_YELLOW: &str = "\x1b[33m";
const STATUS_MIN_INNER_WIDTH: usize = 64;
const STATUS_COLUMN_GAP: usize = 2;
const TRAFFIC_VALUE_WIDTH: usize = 6;

struct StatusRow {
    left: String,
    right: Option<String>,
}

fn status_row(left: String) -> StatusRow {
    StatusRow { left, right: None }
}

fn status_row_with_traffic(left: String, rx_bytes: u64, tx_bytes: u64) -> StatusRow {
    StatusRow {
        left,
        right: Some(format_traffic_stats(rx_bytes, tx_bytes)),
    }
}

fn format_client_count(count: usize) -> String {
    let color = if count == 0 { ANSI_DIM } else { ANSI_GREEN };
    let label = if count == 1 { "client" } else { "clients" };
    format!("{color}{count} connected {label}{ANSI_RESET}")
}

fn format_traffic_stats(rx_bytes: u64, tx_bytes: u64) -> String {
    let rx = format_bytes(rx_bytes);
    let tx = format_bytes(tx_bytes);
    format!(
        "{:>width$}{ANSI_BLUE}⇣{ANSI_RESET} {:>width$}{ANSI_YELLOW}⇡{ANSI_RESET}",
        rx,
        tx,
        width = TRAFFIC_VALUE_WIDTH
    )
}

fn visible_width(text: &str) -> usize {
    let mut width = 0;
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
            width += 1;
        }
    }

    width
}

fn status_row_width(row: &StatusRow) -> usize {
    let left_width = visible_width(&row.left);
    match &row.right {
        Some(right) => left_width + STATUS_COLUMN_GAP + visible_width(right),
        None => left_width,
    }
}

fn render_status_row(row: &StatusRow, inner_width: usize) -> String {
    match &row.right {
        Some(right) => {
            let gap = inner_width.saturating_sub(visible_width(&row.left) + visible_width(right));
            format!("{}{}{}", row.left, " ".repeat(gap), right)
        }
        None => row.left.clone(),
    }
}

fn box_status_lines(content_lines: &[StatusRow]) -> Vec<String> {
    let inner_width = content_lines
        .iter()
        .map(status_row_width)
        .max()
        .unwrap_or(STATUS_MIN_INNER_WIDTH)
        .max(STATUS_MIN_INNER_WIDTH);
    let title = format!("kgdb-console-splitting-proxy v{AGENT_VERSION}");
    let title_width = visible_width(&format!("─ {title} "));
    let top_fill = "─".repeat((inner_width + 2).saturating_sub(title_width));
    let mut lines = Vec::with_capacity(content_lines.len() + 2);

    lines.push(format!(
        "{ANSI_CYAN}╭─{ANSI_RESET} {ANSI_BOLD}{title}{ANSI_RESET} {ANSI_CYAN}{top_fill}╮{ANSI_RESET}"
    ));
    for row in content_lines {
        let line = render_status_row(row, inner_width);
        let padding = " ".repeat(inner_width.saturating_sub(visible_width(&line)));
        lines.push(format!(
            "{ANSI_CYAN}│{ANSI_RESET} {line}{padding} {ANSI_CYAN}│{ANSI_RESET}"
        ));
    }
    lines.push(format!(
        "{ANSI_CYAN}╰{}╯{ANSI_RESET}",
        "─".repeat(inner_width + 2)
    ));

    lines
}

fn push_client_status(
    lines: &mut Vec<StatusRow>,
    state: &ProxyState,
    role: &str,
    client_ids: &[PortId],
) {
    if client_ids.is_empty() {
        lines.push(status_row(format!(
            "{ANSI_DIM}{role} clients{ANSI_RESET}  none"
        )));
        return;
    }

    lines.push(status_row(format!(
        "  {ANSI_DIM}{role} clients{ANSI_RESET}"
    )));
    for &id in client_ids {
        if let Some(port) = state.ports.get(&id) {
            lines.push(status_row_with_traffic(
                format!(
                    "    {ANSI_GREEN}◦{ANSI_RESET} id={id:<3} fd={:<3}",
                    port.sock
                ),
                port.rx_bytes,
                port.tx_bytes,
            ));
        }
    }
}

fn client_traffic_totals(state: &ProxyState, client_ids: &[PortId]) -> (u64, u64) {
    client_ids
        .iter()
        .filter_map(|id| state.ports.get(id))
        .fold((0, 0), |(rx, tx), port| {
            (
                rx.saturating_add(port.rx_bytes),
                tx.saturating_add(port.tx_bytes),
            )
        })
}

fn serial_status_row(serial_split: &SerialSplitConfig, serial: Option<&Port>) -> StatusRow {
    match serial {
        Some(serial) if serial.sock >= 0 => status_row_with_traffic(
            format!(
                "{ANSI_MAGENTA}⌁{ANSI_RESET} {ANSI_BOLD}Serial{ANSI_RESET}     {ANSI_YELLOW}{}{ANSI_RESET} @ {} baud  {ANSI_DIM}fd={}{ANSI_RESET}",
                serial.name, serial_split.baud, serial.sock
            ),
            serial.rx_bytes,
            serial.tx_bytes,
        ),
        Some(serial) => status_row_with_traffic(
            format!(
                "{ANSI_RED}✕{ANSI_RESET} {ANSI_BOLD}Serial{ANSI_RESET}     {ANSI_RED}{}{ANSI_RESET} @ {} baud  {ANSI_RED}disconnected{ANSI_RESET}",
                serial.name, serial_split.baud
            ),
            serial.rx_bytes,
            serial.tx_bytes,
        ),
        None => status_row(format!(
            "{ANSI_RED}✕{ANSI_RESET} {ANSI_BOLD}Serial{ANSI_RESET}     {ANSI_RED}{}{ANSI_RESET} @ {} baud  {ANSI_RED}disconnected{ANSI_RESET}",
            serial_split.device, serial_split.baud
        )),
    }
}

fn print_runtime_status(state: &mut ProxyState, config: &RuntimeConfig) {
    let Some(serial_split) = &config.serial_split else {
        state.status_dirty = false;
        return;
    };

    let serial = state.ports.get(&state.remote_port_id);
    let local = state.ports.get(&state.local_port_id);
    let script_id = local.and_then(|port| port.script_ref);
    let script = script_id.and_then(|id| state.ports.get(&id));

    let console_clients: Vec<PortId> = serial
        .map(|serial| serial.clients.clone())
        .unwrap_or_else(|| {
            script
                .and_then(|script| script.lscript)
                .filter(|id| state.ports.contains_key(id))
                .into_iter()
                .collect()
        })
        .into_iter()
        .filter(|id| state.ports.contains_key(id))
        .collect();
    let gdb_clients = script
        .map(|script| script.clients.clone())
        .unwrap_or_default();
    let (console_rx, console_tx) = client_traffic_totals(state, &console_clients);
    let (gdb_rx, gdb_tx) = client_traffic_totals(state, &gdb_clients);

    let mut content_lines = Vec::new();
    content_lines.push(status_row(format!(
        "{ANSI_GREEN}●{ANSI_RESET} {ANSI_BOLD}Mode{ANSI_RESET}       serial-split"
    )));
    content_lines.push(serial_status_row(serial_split, serial));
    if !console_clients.is_empty() {
        content_lines.push(status_row_with_traffic(
            format!(
                "{ANSI_BLUE}▣{ANSI_RESET} {ANSI_BOLD}Console{ANSI_RESET}    tcp://localhost:{}  {}",
                serial_split.console_port,
                format_client_count(console_clients.len())
            ),
            console_rx,
            console_tx,
        ));
        push_client_status(&mut content_lines, state, "console", &console_clients);
    } else {
        content_lines.push(status_row(format!(
            "{ANSI_BLUE}▣{ANSI_RESET} {ANSI_BOLD}Console{ANSI_RESET}    tcp://localhost:{}  {}",
            serial_split.console_port,
            format_client_count(console_clients.len())
        )));
    }
    if !gdb_clients.is_empty() {
        content_lines.push(status_row_with_traffic(
            format!(
                "{ANSI_YELLOW}◆{ANSI_RESET} {ANSI_BOLD}KGDB{ANSI_RESET}       tcp://localhost:{}  {}",
                serial_split.gdb_port,
                format_client_count(gdb_clients.len())
            ),
            gdb_rx,
            gdb_tx,
        ));
        push_client_status(&mut content_lines, state, "kgdb", &gdb_clients);
    } else {
        content_lines.push(status_row(format!(
            "{ANSI_YELLOW}◆{ANSI_RESET} {ANSI_BOLD}KGDB{ANSI_RESET}       tcp://localhost:{}  {}",
            serial_split.gdb_port,
            format_client_count(gdb_clients.len())
        )));
    }
    let lines = box_status_lines(&content_lines);

    if state.status_line_count > 0 {
        print!("\x1b[{}A\x1b[J", state.status_line_count);
    }
    for line in &lines {
        println!("{line}");
    }
    state.status_line_count = lines.len();
    let _ = io::stdout().flush();
    state.status_dirty = false;
}

// ----- Setup functions -----

fn setup_local_port(
    state: &mut ProxyState,
    port_str: &str,
    is_script: bool,
) -> Result<PortId, String> {
    let mut port_s = port_str;
    let mut local_bind_addr: Option<Ipv4Addr> = None;

    let (port_type, sock) = if port_s == "stdin" {
        let fd = libc::STDIN_FILENO;
        set_nonblocking(fd);
        (PortType::StdinOut, fd)
    } else if let Some(rest) = port_s.strip_prefix("udp:") {
        port_s = rest;
        let sock =
            c_socket(SockType::Datagram).map_err(|_| "Error opening UDP socket".to_string())?;
        set_nonblocking(sock);
        (PortType::Udp, sock)
    } else {
        let sock =
            c_socket(SockType::Stream).map_err(|_| "Error opening TCP socket".to_string())?;
        set_remote_sock_opts(sock);
        set_nonblocking(sock);
        (PortType::Tcp, sock)
    };

    // Check for bind address (IP:port)
    if let Some(colon_pos) = port_s.find(':') {
        let addr_str = &port_s[..colon_pos];
        local_bind_addr = Some(
            addr_str
                .parse::<Ipv4Addr>()
                .map_err(|e| format!("Invalid bind address: {}", e))?,
        );
        port_s = &port_s[colon_pos + 1..];
    }

    let cls = if is_script {
        PortClass::Script
    } else {
        PortClass::Local
    };

    let id = state.alloc_port(port_type, cls);
    {
        let p = state.ports.get_mut(&id).unwrap();
        p.name = "localhost".to_string();
        p.is_local = true;
        p.sock = sock;
    }

    if port_type == PortType::Tcp || port_type == PortType::Udp {
        let port_num = parse_port_number(port_s);

        if port_type != PortType::Udp {
            set_reuseaddr(sock);
        }

        let bind_ip = local_bind_addr.unwrap_or(Ipv4Addr::LOCALHOST);
        let addr = make_sockaddr(bind_ip, port_num);

        if c_bind(sock, &addr).is_err() {
            close_fd(sock);
            return Err("Error: on socket bind, address in use".to_string());
        }

        if port_type == PortType::Tcp && c_listen(sock, 1).is_err() {
            close_fd(sock);
            return Err("Error: on listen()".to_string());
        }

        state.ports.get_mut(&id).unwrap().port_num = port_num;
    }

    if state.debug {
        let p = &state.ports[&id];
        println!("Added local port: {} {}", p.name, p.sock);
    }

    Ok(id)
}

fn setup_remote_port(state: &mut ProxyState, host: &str, port_str: &str) -> Result<PortId, String> {
    let mut port_s = port_str.to_string();
    let port_type;
    let mut sock: RawFd = -1;

    if port_s.starts_with("udp:") {
        port_s = port_s[4..].to_string();
        sock = c_socket(SockType::Datagram).map_err(|_| "Could not allocate socket".to_string())?;
        set_nonblocking(sock);
        port_type = PortType::Udp;

        // Check for source port binding (srcport:destport)
        if let Some(colon_pos) = port_s.find(':') {
            let src_str = &port_s[..colon_pos];
            let src_port = parse_port_number(src_str);
            let addr = make_sockaddr(Ipv4Addr::UNSPECIFIED, src_port);
            if c_bind(sock, &addr).is_err() {
                return Err("Could not bind remote udp socket".to_string());
            }
            if state.debug {
                println!("Binding local port {}", src_port);
            }
            port_s = port_s[colon_pos + 1..].to_string();
        }
    } else if port_s.starts_with("tcplisten:") {
        port_s = port_s[10..].to_string();
        port_type = PortType::Listen;
        sock = c_socket(SockType::Stream).map_err(|_| "Could not allocate socket".to_string())?;
        set_reuseaddr(sock);
        set_nonblocking(sock);
    } else if port_s.starts_with("fifocon:") {
        port_s = port_s[8..].to_string();
        port_type = PortType::FifoCon;

        match nix::unistd::mkfifo(
            port_s.as_str(),
            nix::sys::stat::Mode::from_bits_truncate(0o700),
        ) {
            Ok(()) => {}
            Err(Errno::EEXIST) => {}
            Err(_) => return Err(format!("Error creating {} fifo", port_s)),
        }

        let owned = nix::fcntl::open(
            port_s.as_str(),
            OFlag::O_RDONLY | OFlag::O_NONBLOCK,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(|_| "Error opening fifo".to_string())?;
        sock = owned.into_raw_fd();

        state.fifo_con_file = Some(port_s.clone());

        let id = state.alloc_port(port_type, PortClass::Remote);
        {
            let p = state.ports.get_mut(&id).unwrap();
            p.name = host.to_string();
            p.sock = sock;
        }
        state.master_rds.insert(sock);
        state.refresh_nsockhandle();
        if state.debug {
            println!("Rport socket: {}", sock);
        }
        return Ok(id);
    } else if port_s.starts_with('/') || port_s.starts_with('C') || port_s.starts_with('c') {
        // Serial port
        let (dev_path, baud_info) = if let Some(comma_pos) = port_s.find(',') {
            (&port_s[..comma_pos], Some(&port_s[comma_pos + 1..]))
        } else {
            (port_s.as_str(), None)
        };

        if dev_path.starts_with('/') {
            let baud = match baud_info {
                Some(baud_str) => Some(
                    baud_str
                        .parse()
                        .map_err(|_| format!("Invalid baud rate: {}", baud_str))?,
                ),
                None => None,
            };
            sock = open_serial_device(dev_path, baud)?;

            let id = state.alloc_port(PortType::Rs232, PortClass::Remote);
            {
                let p = state.ports.get_mut(&id).unwrap();
                p.name = dev_path.to_string();
                p.sock = sock;
                p.serial_baud = baud;
                p.serial_config_path = Some(dev_path.to_string());
                p.serial_check_at = Some(Instant::now() + SERIAL_LIVENESS_INTERVAL);
            }
            state.master_rds.insert(sock);
            state.refresh_nsockhandle();
            if state.debug {
                println!("Rport socket: {}", sock);
            }
            return Ok(id);
        } else {
            return Err(format!("Unsupported port path: {}", dev_path));
        }
    } else {
        port_type = PortType::Tcp;
    }

    // For TCP, UDP, Listen: resolve host and set up address
    let port_num = parse_port_number(&port_s);
    let host_addr = resolve_host(host)?;
    let serv_addr = make_sockaddr(host_addr, port_num);

    let id = state.alloc_port(port_type, PortClass::Remote);
    {
        let p = state.ports.get_mut(&id).unwrap();
        p.name = host.to_string();
        p.port_num = port_num;
        p.serv_addr = Some(SocketAddrV4::new(host_addr, port_num));
    }

    match port_type {
        PortType::Udp => {
            if sock < 0 {
                sock = c_socket(SockType::Datagram)
                    .map_err(|_| "Error opening remote socket".to_string())?;
                set_nonblocking(sock);
            }
            let _ = c_connect(sock, &serv_addr);
            let p = state.ports.get_mut(&id).unwrap();
            p.sock = sock;
            state.master_rds.insert(sock);
        }
        PortType::Listen => {
            if sock < 0 {
                return Err("Error opening remote socket".to_string());
            }
            if c_bind(sock, &serv_addr).is_err() {
                return Err("Could not bind remote tcp listen socket".to_string());
            }
            if c_listen(sock, 1).is_err() {
                return Err("Error on listen".to_string());
            }
            let p = state.ports.get_mut(&id).unwrap();
            p.sock = sock;
            state.master_rds.insert(sock);
        }
        PortType::Tcp => {
            let p = state.ports.get_mut(&id).unwrap();
            p.sock = sock; // still -1
        }
        _ => {}
    }

    state.refresh_nsockhandle();
    if state.debug {
        println!("Rport socket: {}", state.ports[&id].sock);
    }

    Ok(id)
}

fn parse_local_port(state: &mut ProxyState, port_str: &str) -> Result<PortId, String> {
    let mut break_port = false;
    let (main_port_str, script_port_str) = if let Some(pos) = port_str.find('+') {
        (&port_str[..pos], Some(&port_str[pos + 1..]))
    } else if let Some(pos) = port_str.find('^') {
        break_port = true;
        (&port_str[..pos], Some(&port_str[pos + 1..]))
    } else {
        (port_str, None)
    };

    let lport_id = setup_local_port(state, main_port_str, false)?;
    state.master_rds.insert(state.ports[&lport_id].sock);

    if let Some(script_str) = script_port_str {
        let script_id = setup_local_port(state, script_str, true)?;

        let lport_type = state.ports[&lport_id].port_type;
        if lport_type == PortType::Udp || lport_type == PortType::StdinOut {
            let sp = state.ports.get_mut(&script_id).unwrap();
            sp.script_in_use = true;
            sp.lscript = Some(lport_id);
        }

        state.master_rds.insert(state.ports[&script_id].sock);
        state.ports.get_mut(&lport_id).unwrap().script_ref = Some(script_id);

        let sp = state.ports.get_mut(&script_id).unwrap();
        sp.lmode = 0;
        sp.rmode = SCRIPT_READ | SCRIPT_WRITE;
        sp.break_port = break_port;
    }

    Ok(lport_id)
}

// ----- Open a new remote connection for an incoming local client -----

fn open_remote_port(state: &mut ProxyState, peer_id: PortId) -> Option<PortId> {
    let remote_id = state.ports[&peer_id].remote?;
    let remote = state.ports.get(&remote_id)?;
    let remote_type = remote.port_type;
    let remote_port_num = remote.port_num;

    match remote_type {
        PortType::Tcp => {
            let remote_addr = state.ports[&remote_id].serv_addr?;
            let sock = c_socket(SockType::Stream).ok()?;
            set_remote_sock_opts(sock);
            set_nonblocking(sock);

            let addr = make_sockaddr(*remote_addr.ip(), remote_addr.port());
            let connecting = match c_connect(sock, &addr) {
                Ok(()) => false,
                Err(err) if errno_is_connect_pending(err) => true,
                Err(_) => {
                    close_fd(sock);
                    return None;
                }
            };

            let id = state.alloc_port(PortType::Tcp, PortClass::Connection);
            {
                let p = state.ports.get_mut(&id).unwrap();
                p.sock = sock;
                p.port_num = remote_port_num;
                p.peer = Some(peer_id);
                p.connecting = connecting;
            }

            if connecting {
                state.master_wds.insert(sock);
            } else {
                state.master_rds.insert(sock);
            }
            state.refresh_nsockhandle();
            state.status_dirty = true;
            Some(id)
        }
        PortType::Udp => {
            state.ports.get_mut(&remote_id).unwrap().peer = Some(peer_id);
            state.status_dirty = true;
            Some(remote_id)
        }
        PortType::Rs232 => {
            let remote = state.ports.get_mut(&remote_id).unwrap();
            if !remote.clients.contains(&peer_id) {
                remote.clients.push(peer_id);
            }
            if remote.peer.is_none() {
                remote.peer = Some(peer_id);
            }
            state.status_dirty = true;
            Some(remote_id)
        }
        _ => None,
    }
}

fn finish_pending_connect(state: &mut ProxyState, port_id: PortId) -> bool {
    let Some(port) = state.ports.get(&port_id) else {
        return true;
    };
    if !port.connecting {
        return true;
    }
    let sock = port.sock;

    match socket_pending_error(sock) {
        Ok(0) => {
            let has_pending = if let Some(port) = state.ports.get_mut(&port_id) {
                port.connecting = false;
                !port.tx_buf.is_empty()
            } else {
                false
            };
            state.master_rds.insert(sock);
            if has_pending {
                state.master_wds.insert(sock);
            } else {
                state.master_wds.remove(sock);
            }
            state.status_dirty = true;
            true
        }
        Ok(err) if errno_is_connect_pending(Errno::from_raw(err)) => {
            state.master_wds.insert(sock);
            true
        }
        Ok(err) => {
            if state.debug {
                eprintln!(
                    "Error completing TCP connect on fd {}: {}",
                    sock,
                    io::Error::from_raw_os_error(err)
                );
            }
            false
        }
        Err(err) => {
            if state.debug {
                eprintln!("Error checking TCP connect on fd {}: {}", sock, err);
            }
            false
        }
    }
}

// ----- IAC telnet processing -----

fn iac_startup(state: &mut ProxyState, port_id: PortId) {
    tracked_port_write(state, port_id, &[0xff, 0xfb, 0x01], false); // IAC WILL ECHO
    tracked_port_write(state, port_id, &[0xff, 0xfb, 0x03], false); // IAC WILL Suppress go ahead
    tracked_port_write(state, port_id, &[0xff, 0xfd, 0x03], false); // IAC DO Suppress go ahead
    tracked_port_write(state, port_id, &[0xff, 0xfb, 0x00], false); // IAC WILL Binary
    tracked_port_write(state, port_id, &[0xff, 0xfd, 0x00], false); // IAC DO Binary
}

fn send_special_break(state: &mut ProxyState, port_id: PortId, break_string: &[u8]) -> bool {
    let pt = state.ports[&port_id].port_type;
    let fd = state.ports[&port_id].sock;

    let mut i = 0;
    while i < break_string.len() {
        if break_string[i] == 0xff
            && i + 1 < break_string.len()
            && break_string[i + 1] == 0xf3
            && pt == PortType::Rs232
        {
            rs232::serial_break(fd);
            i += 2;
        } else {
            let rec = tracked_port_write(state, port_id, &break_string[i..i + 1], false);
            if rec != 1 {
                return false;
            }
            i += 1;
        }
    }
    true
}

fn process_iac_options(state: &mut ProxyState, port_id: PortId, got: usize) -> usize {
    let buf: Vec<u8> = state.ports[&port_id].buf[..got].to_vec();
    let in_iac = state.ports[&port_id].in_iac;
    let cls = state.ports[&port_id].cls;
    let script_ref = state.ports[&port_id].script_ref;

    let mut current_iac = in_iac;
    let mut j = 0usize;
    let mut out_buf = vec![0u8; got];

    for &byte in buf.iter().take(got) {
        if current_iac > 0 {
            if byte == IAC && current_iac == 1 {
                out_buf[j] = byte;
                j += 1;
                current_iac = 0;
            } else if byte == 0xf3 {
                if cls == PortClass::ScriptClient {
                    if let Some(sr) = script_ref {
                        let break_port = state.ports[&sr].break_port;
                        let rscript = state.ports[&sr].rscript;
                        if let Some(rs) = rscript {
                            if break_port {
                                let brk = state.break_str.clone();
                                send_special_break(state, rs, &brk);
                            } else {
                                send_special_break(state, rs, &[0xff, 0xf3]);
                            }
                        }
                    }
                } else {
                    let peer = state.ports[&port_id].peer;
                    if let Some(p) = peer {
                        send_special_break(state, p, &[0xff, 0xf3]);
                    }
                }
                current_iac = 0;
            } else {
                current_iac += 1;
            }
            if current_iac >= 3 {
                current_iac = 0;
            }
        } else if byte == IAC {
            current_iac = 1;
        } else {
            if cls == PortClass::ScriptClient
                && byte == 3
                && let Some(sr) = script_ref
            {
                let break_port = state.ports[&sr].break_port;
                let rscript = state.ports[&sr].rscript;
                if break_port {
                    if let Some(rs) = rscript {
                        let brk = state.break_str.clone();
                        send_special_break(state, rs, &brk);
                    }
                    continue;
                }
            }
            out_buf[j] = byte;
            j += 1;
        }
    }

    state.ports.get_mut(&port_id).unwrap().in_iac = current_iac;
    state.ports.get_mut(&port_id).unwrap().buf[..j].copy_from_slice(&out_buf[..j]);
    j
}

// ----- Write to script clients -----

fn write_script_clients(state: &mut ProxyState, script_port_id: PortId, data: &[u8]) {
    let break_port = state.ports[&script_port_id].break_port;

    let (buf, len) = if break_port && state.gdb_split {
        let mut xmit = false;

        for &b in data {
            if state.gdb_ptr >= MAX_GDB_BUF {
                state.gdb_ptr = 0;
                state.gdb_got_dollar = 0;
            } else if b == b'+' || b == b'-' {
                state.gdb_arr[state.gdb_ptr] = b;
                state.gdb_ptr += 1;
                if state.gdb_got_dollar == 0 {
                    xmit = true;
                }
            } else if b == b'$' {
                state.gdb_got_dollar = 1;
                state.gdb_arr[state.gdb_ptr] = b;
                state.gdb_ptr += 1;
            } else if state.gdb_got_dollar > 0 {
                state.gdb_arr[state.gdb_ptr] = b;
                state.gdb_ptr += 1;
                if state.gdb_got_dollar > 1 {
                    state.gdb_got_dollar += 1;
                }
                if b == b'#' && state.gdb_got_dollar <= 1 {
                    state.gdb_got_dollar += 1;
                }
                if state.gdb_got_dollar >= 4 {
                    state.gdb_got_dollar = 0;
                    xmit = true;
                }
            }
        }

        if !xmit {
            return;
        }

        let len = state.gdb_ptr;
        (state.gdb_arr[..len].to_vec(), len)
    } else {
        (data.to_vec(), data.len())
    };

    let clients: Vec<PortId> = state.ports[&script_port_id].clients.clone();
    let mut to_kill = Vec::new();

    for &client_id in &clients {
        if let Some(client) = state.ports.get(&client_id) {
            let fd = client.sock;
            let got = tracked_port_write(state, client_id, &buf[..len], false);
            if state.logchar {
                print!(">={fd}#{got}= ");
            }
            if got <= 0 {
                if state.debug {
                    println!("ERROR on write of client port {} got {}", fd, got);
                }
                to_kill.push(client_id);
            }
        }
    }

    for cid in to_kill {
        kill_script_client(state, script_port_id, cid);
    }

    if break_port {
        state.gdb_ptr = 0;
    }
}

// ----- Kill helpers -----

fn kill_script_client(state: &mut ProxyState, script_port_id: PortId, client_id: PortId) {
    if let Some(sp) = state.ports.get_mut(&script_port_id) {
        sp.clients.retain(|&c| c != client_id);
    }
    killport(state, client_id);
}

fn detach_serial_client(state: &mut ProxyState, client_id: PortId) {
    for port in state.ports.values_mut() {
        if port.port_type == PortType::Rs232 {
            port.clients.retain(|&id| id != client_id);
            if port.peer == Some(client_id) {
                port.peer = port.clients.first().copied();
            }
        }
    }
}

fn mark_serial_disconnected(state: &mut ProxyState, port_id: PortId, reason: &str) {
    let Some(port) = state.ports.get(&port_id) else {
        return;
    };
    if port.cls != PortClass::Remote || port.port_type != PortType::Rs232 {
        return;
    }

    let sock = port.sock;
    let script_ref = port.script_ref;

    if state.debug {
        eprintln!(
            "Serial port {} disconnected: {}; retrying every {}s",
            port.name,
            reason,
            SERIAL_RECONNECT_INTERVAL.as_secs()
        );
    }

    if sock >= 0 {
        state.master_rds.remove(sock);
        state.master_wds.remove(sock);
        close_fd(sock);
    }

    if let Some(port) = state.ports.get_mut(&port_id) {
        port.sock = -1;
        port.peer = None;
        port.tx_buf.clear();
        port.connecting = false;
        port.serial_reconnect_at = Some(Instant::now() + SERIAL_RECONNECT_INTERVAL);
        port.serial_check_at = None;
    }

    if let Some(sr) = script_ref
        && let Some(sp) = state.ports.get_mut(&sr)
    {
        if sp.rscript == Some(port_id) {
            sp.rscript = None;
        }
        sp.script_in_use = false;
    }

    state.refresh_nsockhandle();
    state.status_dirty = true;
}

fn retry_serial_reconnect(state: &mut ProxyState, port_id: PortId) {
    let Some(port) = state.ports.get(&port_id) else {
        return;
    };
    if port.cls != PortClass::Remote || port.port_type != PortType::Rs232 || port.sock >= 0 {
        return;
    }

    let configured_path = port
        .serial_config_path
        .clone()
        .unwrap_or_else(|| port.name.clone());
    let baud = port.serial_baud;
    match open_serial_candidate(&configured_path, baud) {
        Ok((sock, active_path)) => {
            let script_ref = state.ports.get(&port_id).and_then(|port| port.script_ref);
            if let Some(port) = state.ports.get_mut(&port_id) {
                port.name = active_path.clone();
                port.sock = sock;
                port.peer = None;
                port.serial_reconnect_at = None;
                port.serial_check_at = Some(Instant::now() + SERIAL_LIVENESS_INTERVAL);
                port.tx_buf.clear();
                port.connecting = false;
                if script_ref.is_some() {
                    port.mode = SCRIPT_READ | SCRIPT_WRITE;
                }
            }
            if let Some(sr) = script_ref
                && let Some(sp) = state.ports.get_mut(&sr)
            {
                sp.rscript = Some(port_id);
                sp.script_in_use = true;
            }
            state.master_rds.insert(sock);
            state.refresh_nsockhandle();
            state.status_dirty = true;
            if state.debug {
                eprintln!(
                    "Serial port {configured_path} reconnected as {active_path} on fd {sock}"
                );
            }
        }
        Err(err) => {
            if let Some(port) = state.ports.get_mut(&port_id) {
                port.serial_reconnect_at = Some(Instant::now() + SERIAL_RECONNECT_INTERVAL);
            }
            if state.debug {
                eprintln!("Serial reconnect failed for {configured_path}: {err}");
            }
        }
    }
}

fn retry_due_serial_reconnects(state: &mut ProxyState) {
    let now = Instant::now();
    let due: Vec<PortId> = state
        .ports
        .iter()
        .filter_map(|(&id, port)| {
            if port.cls == PortClass::Remote
                && port.port_type == PortType::Rs232
                && port.sock < 0
                && port
                    .serial_reconnect_at
                    .is_some_and(|retry_at| retry_at <= now)
            {
                Some(id)
            } else {
                None
            }
        })
        .collect();

    for id in due {
        retry_serial_reconnect(state, id);
    }
}

fn check_live_serial_devices(state: &mut ProxyState) {
    let now = Instant::now();
    let due: Vec<PortId> = state
        .ports
        .iter()
        .filter_map(|(&id, port)| {
            if port.cls == PortClass::Remote
                && port.port_type == PortType::Rs232
                && port.sock >= 0
                && port.serial_check_at.is_some_and(|check_at| check_at <= now)
            {
                Some(id)
            } else {
                None
            }
        })
        .collect();

    for id in due {
        let Some(port) = state.ports.get(&id) else {
            continue;
        };
        let sock = port.sock;
        if rs232::is_alive(sock) {
            if let Some(port) = state.ports.get_mut(&id) {
                port.serial_check_at = Some(Instant::now() + SERIAL_LIVENESS_INTERVAL);
            }
        } else {
            mark_serial_disconnected(state, id, "device is no longer responding");
        }
    }
}

fn cleanup_invalid_fds(state: &mut ProxyState) -> usize {
    let invalid: Vec<PortId> = state
        .ports
        .iter()
        .filter_map(|(&id, port)| {
            if port.sock >= 0 && !fd_is_valid(port.sock) {
                Some(id)
            } else {
                None
            }
        })
        .collect();
    let cleaned = invalid.len();

    for id in invalid {
        let Some(port) = state.ports.get(&id) else {
            continue;
        };
        let sock = port.sock;
        let cls = port.cls;
        let pt = port.port_type;

        if cls == PortClass::Remote && pt == PortType::Rs232 {
            mark_serial_disconnected(state, id, "file descriptor became invalid");
        } else if matches!(cls, PortClass::Connection | PortClass::ScriptClient) {
            killport(state, id);
        } else {
            if state.debug {
                eprintln!("Removing invalid fd {sock} from {:?} {:?}", cls, pt);
            }
            state.master_rds.remove(sock);
            state.master_wds.remove(sock);
            close_fd(sock);
            if let Some(port) = state.ports.get_mut(&id) {
                port.sock = -1;
                port.peer = None;
                port.tx_buf.clear();
                port.connecting = false;
            }
            state.status_dirty = true;
        }
    }

    if cleaned > 0 {
        state.refresh_nsockhandle();
    }
    cleaned
}

fn killport(state: &mut ProxyState, port_id: PortId) {
    let port = match state.ports.get(&port_id) {
        Some(p) => p,
        None => return,
    };

    if state.debug {
        let peer_sock = port
            .peer
            .and_then(|pid| state.ports.get(&pid))
            .map(|p| p.sock)
            .unwrap_or(-1);
        println!(
            "Killing cls: {:?} port: {} peer {}",
            port.cls, port.sock, peer_sock
        );
    }

    let cls = port.cls;
    let pt = port.port_type;
    let sock = port.sock;
    let script_ref = port.script_ref;
    let peer_id = port.peer;
    let serial_clients = if cls == PortClass::Remote && pt == PortType::Rs232 {
        port.clients.clone()
    } else {
        Vec::new()
    };
    let remaining_serial_clients: Vec<PortId> = peer_id
        .and_then(|pid| state.ports.get(&pid))
        .filter(|peer| peer.port_type == PortType::Rs232)
        .map(|peer| {
            peer.clients
                .iter()
                .copied()
                .filter(|&id| id != port_id && state.ports.contains_key(&id))
                .collect()
        })
        .unwrap_or_default();

    match cls {
        PortClass::Remote if pt == PortType::Rs232 => {
            mark_serial_disconnected(state, port_id, "device read/write failed");
            return;
        }
        PortClass::Local | PortClass::Script => {
            state.status_dirty = true;
            return;
        }
        PortClass::Remote
            if matches!(
                pt,
                PortType::Udp | PortType::Listen | PortType::StdinOut | PortType::FifoCon
            ) =>
        {
            state.ports.get_mut(&port_id).unwrap().peer = None;
            if pt == PortType::Listen && state.listen_fd >= 0 {
                state.master_rds.remove(sock);
                state.master_wds.remove(sock);
                close_fd(sock);
                state.master_rds.insert(state.listen_fd);
                state.ports.get_mut(&port_id).unwrap().sock = state.listen_fd;
                state.listen_fd = -1;
                state.refresh_nsockhandle();
                state.status_dirty = true;
            }
            if pt == PortType::FifoCon && state.fifo_con_fd >= 0 {
                state.master_rds.remove(sock);
                state.master_wds.remove(sock);
                close_fd(sock);
                state.master_rds.insert(state.fifo_con_fd);
                state.ports.get_mut(&port_id).unwrap().sock = state.fifo_con_fd;
                state.fifo_con_fd = -1;
                state.refresh_nsockhandle();
                state.status_dirty = true;
            }
            state.status_dirty = true;
            return;
        }
        PortClass::Remote | PortClass::ScriptClient | PortClass::Connection => {
            if let Some(sr) = script_ref {
                let lscript = state.ports.get(&sr).and_then(|sp| sp.lscript);
                if lscript == Some(port_id) {
                    let rscript = state.ports.get(&sr).and_then(|sp| sp.rscript);
                    let rs_type = rscript.and_then(|rs| state.ports.get(&rs).map(|p| p.port_type));
                    if let Some(replacement) = remaining_serial_clients.first().copied() {
                        if let Some(sp) = state.ports.get_mut(&sr) {
                            sp.lscript = Some(replacement);
                        }
                    } else if let Some(sp) = state.ports.get_mut(&sr) {
                        sp.lscript = None;
                        if !matches!(
                            rs_type,
                            Some(PortType::Udp)
                                | Some(PortType::Listen)
                                | Some(PortType::StdinOut)
                                | Some(PortType::FifoCon)
                                | Some(PortType::Rs232)
                        ) {
                            sp.script_in_use = false;
                        }
                    }
                }
                if cls == PortClass::Remote
                    && let Some(sp) = state.ports.get_mut(&sr)
                    && sp.rscript == Some(port_id)
                {
                    sp.rscript = None;
                    sp.script_in_use = false;
                }
            }
        }
    }

    state.master_rds.remove(sock);
    state.master_wds.remove(sock);
    port_close(pt, sock);

    detach_serial_client(state, port_id);
    state.ports.remove(&port_id);
    state.status_dirty = true;

    for pid in serial_clients {
        if state.ports.contains_key(&pid) {
            killport(state, pid);
        }
    }

    if let Some(pid) = peer_id
        && let Some(pp) = state.ports.get(&pid)
        && pp.sock != -1
        && !(pp.cls == PortClass::Remote
            && matches!(
                pp.port_type,
                PortType::Udp
                    | PortType::Listen
                    | PortType::StdinOut
                    | PortType::FifoCon
                    | PortType::Rs232
            ))
    {
        killport(state, pid);
    }

    state.refresh_nsockhandle();
}

// ----- Message handlers -----

fn handle_remote_port_accept(state: &mut ProxyState, port_id: PortId) -> bool {
    let sock = state.ports[&port_id].sock;
    let Ok(fd) = c_accept(sock) else {
        return false;
    };

    if state.listen_fd < 0 {
        set_nonblocking(fd);
        state.listen_fd = sock;
        state.ports.get_mut(&port_id).unwrap().sock = fd;
        state.master_rds.remove(state.listen_fd);
        state.master_rds.insert(fd);
        state.refresh_nsockhandle();
        state.status_dirty = true;
    }
    false
}

fn handle_remote_port_fifo_con_read(state: &mut ProxyState, port_id: PortId) -> bool {
    let sock = state.ports[&port_id].sock;
    let mut ibuf = [0u8; 1];

    let cc = nix::unistd::read(borrow_fd(sock), &mut ibuf).unwrap_or(0);
    if cc > 0 {
        if ibuf[0] == b'\n' {
            state.fifo_buf[state.fifo_idx] = 0;
            let port_num_str: String = state.fifo_buf[..state.fifo_idx]
                .iter()
                .take_while(|&&b| b != 0)
                .map(|&b| b as char)
                .collect();
            let port_num: u16 = port_num_str.parse().unwrap_or(0);
            state.fifo_idx = 0;

            if let Ok(new_sock) = c_socket(SockType::Stream) {
                set_remote_sock_opts(new_sock);
                let addr = make_sockaddr(Ipv4Addr::LOCALHOST, port_num);
                if c_connect(new_sock, &addr).is_ok() {
                    set_nonblocking(new_sock);
                    if state.fifo_con_fd < 0 {
                        state.fifo_con_fd = sock;
                        state.ports.get_mut(&port_id).unwrap().sock = new_sock;
                        state.master_rds.remove(state.fifo_con_fd);
                        state.master_rds.insert(new_sock);
                        state.refresh_nsockhandle();
                        state.status_dirty = true;
                    }
                } else {
                    close_fd(new_sock);
                    eprintln!("Error connecting to local port {}", port_num);
                }
            }
        }
        if ibuf[0] != b'\r' && ibuf[0] != b'\n' {
            state.fifo_buf[state.fifo_idx] = ibuf[0];
            state.fifo_idx += 1;
        }
        if state.fifo_idx >= MAX_FIFO_BUF {
            state.fifo_idx = 0;
        }
    } else {
        close_fd(sock);
        state.master_rds.remove(sock);
        if let Some(ref path) = state.fifo_con_file.clone() {
            match nix::fcntl::open(
                path.as_str(),
                OFlag::O_RDONLY | OFlag::O_NONBLOCK,
                nix::sys::stat::Mode::empty(),
            ) {
                Ok(owned) => {
                    let new_fd = owned.into_raw_fd();
                    state.ports.get_mut(&port_id).unwrap().sock = new_fd;
                    state.master_rds.insert(new_fd);
                    state.refresh_nsockhandle();
                    state.status_dirty = true;
                }
                Err(_) => {
                    eprintln!("Error opening fifo");
                    process::exit(1);
                }
            }
        }
    }
    false
}

fn handle_script_port_read(state: &mut ProxyState, port_id: PortId) -> bool {
    let pt = state.ports[&port_id].port_type;
    let sock = state.ports[&port_id].sock;

    if pt != PortType::Tcp {
        println!("Error: Only TCP ports are supported for scripting");
        return true;
    }

    let nsock = match c_accept(sock) {
        Ok(fd) => fd,
        Err(_) => {
            println!("error on socket accept()");
            return true;
        }
    };

    if state.debug {
        println!("Opened from remote {}", nsock);
    }
    set_remote_sock_opts(nsock);
    set_nonblocking(nsock);

    let client_id = state.alloc_port(PortType::Tcp, PortClass::ScriptClient);
    let break_port = state.ports[&port_id].break_port;
    {
        let p = state.ports.get_mut(&client_id).unwrap();
        p.sock = nsock;
        p.script_ref = Some(port_id);
        if break_port {
            p.mode = NO_TELNET_OPTION_NEGOTIATION;
        }
    }

    state
        .ports
        .get_mut(&port_id)
        .unwrap()
        .clients
        .push(client_id);

    state.master_rds.insert(nsock);
    state.refresh_nsockhandle();
    state.status_dirty = true;

    if state.debug {
        println!("Added script client: {}", nsock);
    }

    let mode = state.ports[&port_id].mode;
    if (mode & NO_TELNET_OPTION_NEGOTIATION) == 0 && !break_port {
        iac_startup(state, client_id);
    }

    if state.break_on_connect {
        let rscript = state.ports[&port_id].rscript;
        if let Some(rs) = rscript
            && break_port
        {
            let brk = state.break_str.clone();
            send_special_break(state, rs, &brk);
        }
    }

    false
}

fn handle_script_client_read(state: &mut ProxyState, port_id: PortId) -> bool {
    let fd = state.ports[&port_id].sock;
    let script_ref = state.ports[&port_id].script_ref;

    let mut buf = vec![0u8; IO_BUFSIZE];
    let got = tracked_port_read(state, port_id, &mut buf, false);

    if got <= 0 {
        if let Some(sr) = script_ref {
            kill_script_client(state, sr, port_id);
        }
        return true;
    }
    let got = got as usize;

    if state.debug {
        println!("Read from script: {} got: {}", fd, got);
    }
    if state.logchar {
        print!("<{}=", fd);
        for &byte in buf.iter().take(got) {
            print!("{}", byte as char);
        }
        print!("= ");
    }

    state.ports.get_mut(&port_id).unwrap().buf[..got].copy_from_slice(&buf[..got]);

    let mode = state.ports[&port_id].mode;
    let got = if (mode & NO_TELNET_OPTION_NEGOTIATION) == 0 {
        let new_got = process_iac_options(state, port_id, got);
        if new_got == 0 {
            if state.logchar {
                println!();
            }
            return false;
        }
        new_got
    } else {
        got
    };

    let script_ref = state.ports[&port_id].script_ref;
    if let Some(sr) = script_ref {
        if !state.ports[&sr].script_in_use {
            if state.logchar {
                println!();
            }
            return false;
        }

        let rscript = state.ports[&sr].rscript;
        if let Some(rs) = rscript {
            let rs_mode = state.ports[&rs].mode;
            if rs_mode & SCRIPT_WRITE != 0 {
                let rs_fd = state.ports[&rs].sock;
                let data: Vec<u8> = state.ports[&port_id].buf[..got].to_vec();
                let wgot = tracked_port_write(state, rs, &data, false);
                if state.logchar {
                    print!(">={rs_fd}#{wgot}= ");
                }
                if wgot <= 0 {
                    killport(state, rs);
                    if state.logchar {
                        println!();
                    }
                    return true;
                }
            }
        }

        let lscript = state.ports[&sr].lscript;
        if let Some(ls) = lscript {
            let ls_mode = state.ports[&ls].mode;
            if ls_mode & SCRIPT_WRITE != 0 {
                let ls_fd = state.ports[&ls].sock;
                let data: Vec<u8> = state.ports[&port_id].buf[..got].to_vec();
                let wgot = tracked_port_write(state, ls, &data, false);
                if state.logchar {
                    print!(">={ls_fd}#{wgot}= ");
                }
                if wgot <= 0 {
                    killport(state, ls);
                    if state.logchar {
                        println!();
                    }
                    return true;
                }
            }
        }
    }

    if state.logchar {
        println!();
    }
    false
}

fn handle_local_port_read(state: &mut ProxyState, port_id: PortId) -> bool {
    let pt = state.ports[&port_id].port_type;

    if pt == PortType::Tcp {
        let sock = state.ports[&port_id].sock;
        let nsock = match c_accept(sock) {
            Ok(fd) => fd,
            Err(_) => {
                println!("error on socket accept()");
                return true;
            }
        };

        if state.debug {
            println!("Opened from remote {}", nsock);
        }
        set_remote_sock_opts(nsock);
        set_nonblocking(nsock);

        let remote_ref = state.ports[&port_id].remote;
        let iport_id = state.alloc_port(PortType::Tcp, PortClass::Connection);
        {
            let p = state.ports.get_mut(&iport_id).unwrap();
            p.sock = nsock;
            p.is_local = true;
            p.remote = remote_ref;
        }

        match open_remote_port(state, iport_id) {
            Some(peer_id) => {
                state.ports.get_mut(&iport_id).unwrap().peer = Some(peer_id);
                state.master_rds.insert(nsock);
                state.refresh_nsockhandle();
                state.status_dirty = true;

                let remote_id = state.ports[&port_id].remote;
                if let Some(rid) = remote_id
                    && state.telnet_negotiation
                    && state.ports[&rid].port_type == PortType::Rs232
                {
                    iac_startup(state, iport_id);
                }

                let script_ref = state.ports[&port_id].script_ref;
                if let Some(sr) = script_ref {
                    let remote_is_rs232 = state.ports[&peer_id].port_type == PortType::Rs232;
                    let remote_connected = state.ports[&peer_id].sock >= 0;
                    if remote_is_rs232 {
                        state.ports.get_mut(&iport_id).unwrap().script_ref = Some(sr);
                        let lmode = state.ports[&sr].lmode;
                        state.ports.get_mut(&iport_id).unwrap().mode = lmode;

                        let sp = state.ports.get_mut(&sr).unwrap();
                        if sp.lscript.is_none() {
                            sp.lscript = Some(iport_id);
                        }
                        if remote_connected {
                            state.ports.get_mut(&peer_id).unwrap().script_ref = Some(sr);
                            let rmode = state.ports[&sr].rmode;
                            state.ports.get_mut(&peer_id).unwrap().mode = rmode;

                            let sp = state.ports.get_mut(&sr).unwrap();
                            sp.rscript = Some(peer_id);
                            sp.script_in_use = true;
                        }
                    } else if !state.ports[&sr].script_in_use {
                        state.ports.get_mut(&iport_id).unwrap().script_ref = Some(sr);
                        let lmode = state.ports[&sr].lmode;
                        state.ports.get_mut(&iport_id).unwrap().mode = lmode;

                        state.ports.get_mut(&peer_id).unwrap().script_ref = Some(sr);
                        let rmode = state.ports[&sr].rmode;
                        state.ports.get_mut(&peer_id).unwrap().mode = rmode;

                        let sp = state.ports.get_mut(&sr).unwrap();
                        if sp.lscript.is_none() {
                            sp.lscript = Some(iport_id);
                        }
                        sp.rscript = Some(peer_id);
                        sp.script_in_use = true;
                    }
                }
            }
            None => {
                if state.debug {
                    println!("Error opening remote socket");
                }
                state.remove_port(iport_id);
                let _ = nix_sock::shutdown(nsock, Shutdown::Both);
                close_fd(nsock);
            }
        }
    } else if pt == PortType::Udp || pt == PortType::StdinOut {
        match open_remote_port(state, port_id) {
            Some(peer_id) => {
                state.ports.get_mut(&port_id).unwrap().peer = Some(peer_id);

                let mut buf = vec![0u8; IO_BUFSIZE];
                let got = tracked_port_read(state, port_id, &mut buf, false);
                let fd = state.ports[&port_id].sock;

                if state.debug {
                    let peer_sock = state.ports[&peer_id].sock;
                    println!(
                        "Read from child1: {} got: {} write to {}",
                        fd, got, peer_sock
                    );
                }

                if got <= 0 && pt == PortType::StdinOut {
                    println!("Terminating because STDIN read return <= 0");
                    process::exit(0);
                }

                if got > 0 {
                    if state.logchar {
                        print!("<{}=", fd);
                        for &byte in buf.iter().take(got as usize) {
                            print!("{}", byte as char);
                        }
                        println!("=");
                    }
                    let peer_fd = state.ports[&peer_id].sock;
                    let wgot = tracked_port_write(state, peer_id, &buf[..got as usize], false);
                    if wgot <= 0 {
                        let peer_name = state.ports[&peer_id].name.clone();
                        println!("Error writing to remote: {} on {}", peer_name, peer_fd);
                    }
                }
            }
            None => {
                println!("Warning remote socket could not be opened");
                let mut buf = vec![0u8; IO_BUFSIZE];
                tracked_port_read(state, port_id, &mut buf, false);
            }
        }
    }
    false
}

fn handle_remote_port_read(state: &mut ProxyState, port_id: PortId) -> bool {
    let fd = state.ports[&port_id].sock;

    let mut buf = vec![0u8; IO_BUFSIZE];
    let rgot = tracked_port_read(state, port_id, &mut buf, false);

    if state.logchar {
        print!("<{}=", fd);
        for &byte in buf.iter().take(rgot.max(0) as usize) {
            print!("{}", byte as char);
        }
        print!("= ");
    }

    if rgot <= 0 {
        killport(state, port_id);
        if state.logchar {
            println!();
        }
        return true;
    }
    let rgot = rgot as usize;

    state.ports.get_mut(&port_id).unwrap().buf[..rgot].copy_from_slice(&buf[..rgot]);

    let telnet_neg = state.telnet_negotiation;
    let rgot = if telnet_neg {
        let new_rgot = process_iac_options(state, port_id, rgot);
        if new_rgot == 0 {
            if state.logchar {
                println!();
            }
            return false;
        }
        new_rgot
    } else {
        rgot
    };

    let port_type = state.ports[&port_id].port_type;
    let cls = state.ports[&port_id].cls;
    let data: Vec<u8> = state.ports[&port_id].buf[..rgot].to_vec();
    if port_type == PortType::Rs232 && cls == PortClass::Remote {
        let clients = state.ports[&port_id].clients.clone();
        let mut to_kill = Vec::new();

        for pid in clients {
            let peer_sock = state.ports.get(&pid).map(|p| p.sock).unwrap_or(-1);
            if peer_sock < 0 {
                to_kill.push(pid);
                continue;
            }

            let wgot = tracked_port_write(state, pid, &data, false);
            if state.logchar {
                print!(">={fd}#{wgot}= ");
            }
            if state.debug {
                println!(
                    "Read from child2: {} got: {} write to {}",
                    fd, rgot, peer_sock
                );
            }
            if wgot <= 0 {
                to_kill.push(pid);
            }
        }

        for pid in to_kill {
            killport(state, pid);
        }
    } else if let Some(pid) = state.ports[&port_id].peer {
        let peer_sock = state.ports.get(&pid).map(|p| p.sock).unwrap_or(-1);
        if peer_sock >= 0 {
            let wgot = tracked_port_write(state, pid, &data, false);

            if state.logchar {
                print!(">={fd}#{wgot}= ");
            }
            if state.debug {
                println!(
                    "Read from child2: {} got: {} write to {}",
                    fd, rgot, peer_sock
                );
            }
            if wgot <= 0 {
                killport(state, port_id);
                if state.logchar {
                    println!();
                }
                return true;
            }
        } else if state.debug {
            println!("Read from child3: {} got: {} to /dev/null", fd, rgot);
        }
    }

    let script_ref = state.ports[&port_id].script_ref;
    let mode = state.ports[&port_id].mode;
    if let Some(sr) = script_ref
        && mode & SCRIPT_READ != 0
    {
        let data: Vec<u8> = state.ports[&port_id].buf[..rgot].to_vec();
        write_script_clients(state, sr, &data);
    }

    if state.logchar {
        println!();
    }
    false
}

// ----- Dispatch a read event -----

fn dispatch_read(state: &mut ProxyState, port_id: PortId) -> bool {
    let port = match state.ports.get(&port_id) {
        Some(p) => p,
        None => return false,
    };

    let cls = port.cls;
    let pt = port.port_type;

    match cls {
        PortClass::Local => handle_local_port_read(state, port_id),
        PortClass::Script => handle_script_port_read(state, port_id),
        PortClass::ScriptClient => handle_script_client_read(state, port_id),
        PortClass::Connection => handle_remote_port_read(state, port_id),
        PortClass::Remote => {
            if pt == PortType::Listen && state.listen_fd < 0 {
                handle_remote_port_accept(state, port_id)
            } else if pt == PortType::FifoCon && state.fifo_con_fd < 0 {
                handle_remote_port_fifo_con_read(state, port_id)
            } else {
                handle_remote_port_read(state, port_id)
            }
        }
    }
}

// ----- Main -----

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Ignore SIGPIPE
    // SAFETY: SIG_IGN is the trivial handler — it does not invoke arbitrary code.
    unsafe {
        let _ = nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGPIPE,
            nix::sys::signal::SigHandler::SigIgn,
        );
    }

    let mut state = ProxyState::new();
    let cli = parse_cli(&args, &mut state);
    let proxy_args = cli.proxy_args;
    let pidfile = cli.pidfile;
    let do_fork = cli.do_fork;

    // Setup local port
    let lport_id = parse_local_port(&mut state, &proxy_args[0]).unwrap_or_else(|e| {
        println!("Open of local port failed: {}", e);
        process::exit(1);
    });
    state.local_port_id = lport_id;

    // Setup remote port
    let rport_id =
        setup_remote_port(&mut state, &proxy_args[1], &proxy_args[2]).unwrap_or_else(|e| {
            println!("Open of remote port failed: {}", e);
            process::exit(1);
        });
    state.remote_port_id = rport_id;

    // Connect local to remote
    state.ports.get_mut(&lport_id).unwrap().remote = Some(rport_id);

    // Persistent remotes can accept script clients before a console client exists.
    let rtype = state.ports[&rport_id].port_type;
    let script_ref = state.ports[&lport_id].script_ref;
    if matches!(
        rtype,
        PortType::Udp | PortType::Listen | PortType::FifoCon | PortType::Rs232
    ) && let Some(sr) = script_ref
    {
        state.ports.get_mut(&sr).unwrap().script_in_use = true;
        state.ports.get_mut(&sr).unwrap().rscript = Some(rport_id);
        state.ports.get_mut(&rport_id).unwrap().mode = SCRIPT_READ | SCRIPT_WRITE;
        state.ports.get_mut(&rport_id).unwrap().script_ref = Some(sr);
    }
    let runtime_config = RuntimeConfig::from_state(&state, &proxy_args);

    // PID file and fork handling
    use nix::unistd::ForkResult;
    if let Some(ref pf) = pidfile {
        if do_fork {
            // SAFETY: this binary is single-threaded at fork time and runs no
            // async-signal-unsafe code in the child between fork and exec.
            match unsafe { nix::unistd::fork() } {
                Err(_) => {
                    eprintln!("Fork failed");
                    process::exit(1);
                }
                Ok(ForkResult::Child) => {}
                Ok(ForkResult::Parent { child }) => {
                    write_pid_file_or_exit(pf, child.as_raw() as u32);
                    process::exit(0);
                }
            }
        } else {
            write_pid_file_or_exit(pf, process::id());
        }
    } else if do_fork {
        // SAFETY: see above — single-threaded fork, child does no async-signal-unsafe work.
        match unsafe { nix::unistd::fork() } {
            Err(_) => {
                eprintln!("Fork failed");
                process::exit(1);
            }
            Ok(ForkResult::Child) => {}
            Ok(ForkResult::Parent { .. }) => process::exit(0),
        }
    }

    state.refresh_nsockhandle();
    print_runtime_status(&mut state, &runtime_config);
    let mut last_status = Instant::now();

    // ----- Main event loop -----
    loop {
        check_live_serial_devices(&mut state);
        retry_due_serial_reconnects(&mut state);

        let mut rds = state.master_rds.clone();
        let mut wds = state.master_wds.clone();
        let mut eds = state.master_rds.clone();
        let mut timeout = nix::sys::time::TimeVal::new(
            STATUS_INTERVAL.as_secs() as libc::time_t,
            0 as libc::suseconds_t,
        );

        let nfds = state.nsockhandle;
        if nfds <= 0 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            continue;
        }

        let select_ret = nix::sys::select::select(
            Some(nfds),
            Some(&mut rds.inner),
            Some(&mut wds.inner),
            Some(&mut eds.inner),
            Some(&mut timeout),
        );

        let ready = match select_ret {
            Ok(n) => n,
            Err(errno) => {
                if !errno_is_interrupted(errno) {
                    let cleaned = cleanup_invalid_fds(&mut state);
                    if state.debug {
                        println!("Select error: {errno:?}, cleaned: {cleaned}");
                    }
                    if cleaned == 0 {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
                continue;
            }
        };

        if ready == 0 {
            if state.status_dirty && last_status.elapsed() >= STATUS_INTERVAL {
                print_runtime_status(&mut state, &runtime_config);
                last_status = Instant::now();
            }
            continue;
        }

        // Process read-ready ports
        let port_ids: Vec<PortId> = state.ports.keys().copied().collect();
        'read_loop: for &pid in &port_ids {
            let sock = match state.ports.get(&pid) {
                Some(p) => p.sock,
                None => continue,
            };
            if sock < 0 {
                continue;
            }
            if rds.contains(sock) && dispatch_read(&mut state, pid) {
                break 'read_loop;
            }
        }

        // Process ports with queued outbound data.
        let port_ids: Vec<PortId> = state.ports.keys().copied().collect();
        for &pid in &port_ids {
            let sock = match state.ports.get(&pid) {
                Some(p) => p.sock,
                None => continue,
            };
            if sock < 0 {
                continue;
            }
            if wds.contains(sock) {
                let connecting = state.ports.get(&pid).map(|p| p.connecting).unwrap_or(false);
                if connecting && !finish_pending_connect(&mut state, pid) {
                    killport(&mut state, pid);
                    break;
                }
                let has_pending = state
                    .ports
                    .get(&pid)
                    .map(|p| !p.tx_buf.is_empty())
                    .unwrap_or(false);
                if has_pending && !flush_pending_write(&mut state, pid) {
                    killport(&mut state, pid);
                    break;
                }
            }
        }

        // Process OOB (exception) data
        let port_ids: Vec<PortId> = state.ports.keys().copied().collect();
        for &pid in &port_ids {
            let (sock, peer) = match state.ports.get(&pid) {
                Some(p) => (p.sock, p.peer),
                None => continue,
            };
            if sock < 0 {
                continue;
            }
            if eds.contains(sock) {
                let mut buf = [0u8; 1];
                let got = tracked_port_read(&mut state, pid, &mut buf, true);
                if got <= 0 {
                    killport(&mut state, pid);
                    break;
                }
                if state.debug {
                    println!("OOB child: {} got: {}", sock, got);
                }
                if let Some(peer_id) = peer
                    && state.ports.contains_key(&peer_id)
                {
                    let wgot = tracked_port_write(&mut state, peer_id, &buf[..got as usize], true);
                    if wgot <= 0 {
                        killport(&mut state, pid);
                        break;
                    }
                }
            }
        }

        if state.status_dirty && last_status.elapsed() >= STATUS_INTERVAL {
            print_runtime_status(&mut state, &runtime_config);
            last_status = Instant::now();
        }
    }
}
