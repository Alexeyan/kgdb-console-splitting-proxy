use std::net::SocketAddrV4;
use std::os::unix::io::RawFd;
use std::time::Instant;

pub const IO_BUFSIZE: usize = 8192;
pub const IAC: u8 = 255;

/// Port transport type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortType {
    Tcp,
    Udp,
    Rs232,
    StdinOut,
    Listen,
    FifoCon,
}

/// Port classification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortClass {
    Local,
    Remote,
    Script,
    ScriptClient,
    Connection,
}

/// Operation mode flags for script ports
pub const SCRIPT_READ: u32 = 0x4;
pub const SCRIPT_WRITE: u32 = 0x2;
pub const NO_TELNET_OPTION_NEGOTIATION: u32 = 0x10;

/// Unique identifier for a port in the port list
pub type PortId = usize;

/// A port in the proxy system. Uses indices (PortId) instead of raw pointers
/// for referencing other ports.
pub struct Port {
    pub port_type: PortType,
    pub cls: PortClass,
    pub sock: RawFd,
    pub is_local: bool,
    pub peer: Option<PortId>,
    pub remote: Option<PortId>,
    pub in_iac: u32,

    // Script-specific
    pub script_ref: Option<PortId>,
    pub mode: u32,
    pub lmode: u32,
    pub rmode: u32,
    pub script_in_use: bool,
    pub lscript: Option<PortId>,
    pub rscript: Option<PortId>,
    pub clients: Vec<PortId>,
    pub break_port: bool,

    // Connection info
    pub port_num: u16,
    pub name: String,
    pub buf: Vec<u8>,
    pub serv_addr: Option<SocketAddrV4>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub tx_buf: Vec<u8>,
    pub connecting: bool,
    pub serial_baud: Option<u32>,
    pub serial_config_path: Option<String>,
    pub serial_reconnect_at: Option<Instant>,
    pub serial_check_at: Option<Instant>,
}

impl Port {
    pub fn new(_id: PortId, port_type: PortType, cls: PortClass) -> Self {
        Port {
            port_type,
            cls,
            sock: -1,
            is_local: false,
            peer: None,
            remote: None,
            in_iac: 0,
            script_ref: None,
            mode: 0,
            lmode: 0,
            rmode: 0,
            script_in_use: false,
            lscript: None,
            rscript: None,
            clients: Vec::new(),
            break_port: false,
            port_num: 0,
            name: String::new(),
            buf: vec![0u8; IO_BUFSIZE],
            serv_addr: None,
            rx_bytes: 0,
            tx_bytes: 0,
            tx_buf: Vec::new(),
            connecting: false,
            serial_baud: None,
            serial_config_path: None,
            serial_reconnect_at: None,
            serial_check_at: None,
        }
    }
}
