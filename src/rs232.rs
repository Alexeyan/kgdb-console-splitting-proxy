use nix::sys::termios::{self, BaudRate, SetArg};
use std::os::unix::io::RawFd;

/// Wrap a RawFd into a BorrowedFd for nix API calls.
///
/// # Safety
/// The caller must ensure that `fd` is a valid, open file descriptor
/// for the duration of the returned `BorrowedFd`.
unsafe fn borrow_fd(fd: RawFd) -> std::os::fd::BorrowedFd<'static> {
    unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }
}

/// Map a numeric baud rate to the nix BaudRate enum.
pub fn rate_to_code(rate: u32) -> Option<BaudRate> {
    match rate {
        50 => Some(BaudRate::B50),
        75 => Some(BaudRate::B75),
        110 => Some(BaudRate::B110),
        134 => Some(BaudRate::B134),
        150 => Some(BaudRate::B150),
        200 => Some(BaudRate::B200),
        300 => Some(BaudRate::B300),
        600 => Some(BaudRate::B600),
        1200 => Some(BaudRate::B1200),
        1800 => Some(BaudRate::B1800),
        2400 => Some(BaudRate::B2400),
        4800 => Some(BaudRate::B4800),
        9600 => Some(BaudRate::B9600),
        19200 => Some(BaudRate::B19200),
        38400 => Some(BaudRate::B38400),
        57600 => Some(BaudRate::B57600),
        115200 => Some(BaudRate::B115200),
        230400 => Some(BaudRate::B230400),
        460800 => Some(BaudRate::B460800),
        500000 => Some(BaudRate::B500000),
        576000 => Some(BaudRate::B576000),
        921600 => Some(BaudRate::B921600),
        1000000 => Some(BaudRate::B1000000),
        1152000 => Some(BaudRate::B1152000),
        1500000 => Some(BaudRate::B1500000),
        2000000 => Some(BaudRate::B2000000),
        2500000 => Some(BaudRate::B2500000),
        3000000 => Some(BaudRate::B3000000),
        3500000 => Some(BaudRate::B3500000),
        4000000 => Some(BaudRate::B4000000),
        _ => None,
    }
}

/// Set the baud rate on a serial port file descriptor.
pub fn setbaudrate(fd: RawFd, baud: u32) -> Result<(), String> {
    let baud_code = rate_to_code(baud).ok_or_else(|| format!("Invalid baud rate: {}", baud))?;

    let bfd = unsafe { borrow_fd(fd) };
    let mut tios = termios::tcgetattr(bfd).map_err(|e| format!("Cannot get tty state: {}", e))?;

    termios::cfsetospeed(&mut tios, baud_code).map_err(|e| format!("cfsetospeed failed: {}", e))?;
    termios::cfsetispeed(&mut tios, baud_code).map_err(|e| format!("cfsetispeed failed: {}", e))?;

    termios::tcsetattr(bfd, SetArg::TCSANOW, &tios)
        .map_err(|e| format!("Cannot set tty state: {}", e))?;

    Ok(())
}

/// Set stop bits on a serial port. "2" or "1.5" = 2 stop bits, else 1.
pub fn setstopbits(fd: RawFd, stopbits: &str) -> Result<(), String> {
    let bfd = unsafe { borrow_fd(fd) };
    let mut tios = termios::tcgetattr(bfd).map_err(|e| format!("Cannot get tty state: {}", e))?;

    if stopbits == "2" || stopbits == "1.5" {
        tios.control_flags.insert(termios::ControlFlags::CSTOPB);
    } else {
        tios.control_flags.remove(termios::ControlFlags::CSTOPB);
    }

    termios::tcsetattr(bfd, SetArg::TCSANOW, &tios)
        .map_err(|e| format!("Cannot set tty state: {}", e))?;

    Ok(())
}

/// Set console defaults: raw mode, 8N1, no echo, no canonical.
pub fn setcondefaults(fd: RawFd) -> Result<(), String> {
    let bfd = unsafe { borrow_fd(fd) };
    let mut tios = termios::tcgetattr(bfd).map_err(|e| format!("Cannot get tty state: {}", e))?;

    tios.input_flags = termios::InputFlags::empty();
    tios.output_flags = termios::OutputFlags::empty();
    tios.local_flags = termios::LocalFlags::empty();
    tios.control_flags
        .remove(termios::ControlFlags::CSIZE | termios::ControlFlags::PARENB);
    tios.control_flags
        .insert(termios::ControlFlags::CLOCAL | termios::ControlFlags::CS8);
    tios.control_chars[termios::SpecialCharacterIndices::VMIN as usize] = 0;
    tios.control_chars[termios::SpecialCharacterIndices::VTIME as usize] = 0;

    termios::tcsetattr(bfd, SetArg::TCSANOW, &tios)
        .map_err(|e| format!("Cannot set tty state: {}", e))?;

    Ok(())
}

/// Send a serial break.
pub fn serial_break(fd: RawFd) {
    let bfd = unsafe { borrow_fd(fd) };
    let _ = termios::tcsendbreak(bfd, 0);
}

/// Probe whether the tty still accepts termios operations.
pub fn is_alive(fd: RawFd) -> bool {
    let bfd = unsafe { borrow_fd(fd) };
    termios::tcgetattr(bfd).is_ok()
}
