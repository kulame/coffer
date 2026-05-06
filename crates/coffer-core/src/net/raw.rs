//! Low-level network interface manipulation using ioctls.
//!
//! Replaces external `ip` calls for operations that need to run inside a
//! process with `CAP_NET_ADMIN`.  Using ioctls directly means the capability
//! stays effective in the *current* process — there is no capability-drop
//! across `fork`+`exec` as happens when shelling out to `/usr/sbin/ip`.

use std::net::Ipv4Addr;

use crate::error::{CofferError, Result};

const IFNAMSIZ: usize = 16;

// Linux ioctl constants (ABI-stable).
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;
const TUNSETPERSIST: libc::c_ulong = 0x4004_54cb;
const IFF_TAP: libc::c_int = 0x0002;
const IFF_NO_PI: libc::c_int = 0x1000;

const SIOCBRADDBR: libc::c_ulong = 0x0000_89a0;
const SIOCBRADDIF: libc::c_ulong = 0x0000_89a2;
const SIOCBRDELIF: libc::c_ulong = 0x0000_89a3;

const SIOCGIFFLAGS: libc::c_ulong = 0x0000_8913;
const SIOCSIFFLAGS: libc::c_ulong = 0x0000_8914;
const SIOCGIFINDEX: libc::c_ulong = 0x0000_8933;

const SIOCSIFADDR: libc::c_ulong = 0x0000_8916;
const SIOCSIFNETMASK: libc::c_ulong = 0x0000_891c;

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

/// Build a zeroed `struct ifreq` (40 bytes on x86_64 Linux) with the
/// interface name copied into `ifr_name`.
fn ifreq_with_name(name: &str) -> [u8; 40] {
    let mut buf = [0u8; 40];
    let bytes = name.as_bytes();
    let len = bytes.len().min(IFNAMSIZ - 1);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf
}

/// Open a temporary `AF_INET` / `SOCK_DGRAM` socket for ioctl use.
fn socket_ioctl() -> Result<libc::c_int> {
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return Err(CofferError::Network(format!(
            "socket(AF_INET, SOCK_DGRAM): {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(fd)
}

fn last_io_err() -> std::io::Error {
    std::io::Error::last_os_error()
}

// ------------------------------------------------------------------
// TAP
// ------------------------------------------------------------------

/// Create a persistent TAP device.
pub fn create_tap(name: &str) -> Result<()> {
    let fd = unsafe {
        libc::open(
            b"/dev/net/tun\0".as_ptr() as *const libc::c_char,
            libc::O_RDWR | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(CofferError::Network(format!(
            "Failed to open /dev/net/tun: {}",
            last_io_err()
        )));
    }

    let mut req = ifreq_with_name(name);
    let flags = (IFF_TAP | IFF_NO_PI) as i16;
    req[IFNAMSIZ..IFNAMSIZ + 2].copy_from_slice(&flags.to_ne_bytes());

    let rc = unsafe { libc::ioctl(fd, TUNSETIFF, req.as_ptr()) };
    if rc < 0 {
        let err = last_io_err();
        unsafe { libc::close(fd) };
        if err.raw_os_error() == Some(libc::EEXIST)
            || err.raw_os_error() == Some(libc::EBUSY)
        {
            return Err(CofferError::Network(format!(
                "TAP {} already exists (held by another process). Consider killing stale Firecracker processes.",
                name
            )));
        }
        return Err(CofferError::Network(format!("ioctl(TUNSETIFF): {}", err)));
    }

    let rc = unsafe { libc::ioctl(fd, TUNSETPERSIST, 1) };
    if rc < 0 {
        let err = last_io_err();
        unsafe { libc::close(fd) };
        return Err(CofferError::Network(format!(
            "ioctl(TUNSETPERSIST): {}",
            err
        )));
    }

    unsafe { libc::close(fd) };
    Ok(())
}

/// Delete a persistent TAP device.
pub fn delete_tap(name: &str) -> Result<()> {
    let fd = unsafe {
        libc::open(
            b"/dev/net/tun\0".as_ptr() as *const libc::c_char,
            libc::O_RDWR | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Ok(());
    }

    let mut req = ifreq_with_name(name);
    let flags = (IFF_TAP | IFF_NO_PI) as i16;
    req[IFNAMSIZ..IFNAMSIZ + 2].copy_from_slice(&flags.to_ne_bytes());

    let rc = unsafe { libc::ioctl(fd, TUNSETIFF, req.as_ptr()) };
    if rc < 0 {
        unsafe { libc::close(fd) };
        return Ok(());
    }

    let _ = unsafe { libc::ioctl(fd, TUNSETPERSIST, 0) };
    unsafe { libc::close(fd) };
    Ok(())
}

// ------------------------------------------------------------------
// Link flags (up / down)
// ------------------------------------------------------------------

fn get_flags(fd: libc::c_int, name: &str) -> Result<i16> {
    let mut req = ifreq_with_name(name);
    if unsafe { libc::ioctl(fd, SIOCGIFFLAGS, req.as_mut_ptr()) } < 0 {
        return Err(CofferError::Network(format!(
            "ioctl(SIOCGIFFLAGS): {}",
            last_io_err()
        )));
    }
    Ok(i16::from_ne_bytes([req[IFNAMSIZ], req[IFNAMSIZ + 1]]))
}

fn set_flags(fd: libc::c_int, name: &str, flags: i16) -> Result<()> {
    let mut req = ifreq_with_name(name);
    req[IFNAMSIZ..IFNAMSIZ + 2].copy_from_slice(&flags.to_ne_bytes());
    if unsafe { libc::ioctl(fd, SIOCSIFFLAGS, req.as_mut_ptr()) } < 0 {
        return Err(CofferError::Network(format!(
            "ioctl(SIOCSIFFLAGS): {}",
            last_io_err()
        )));
    }
    Ok(())
}

pub fn set_link_up(name: &str) -> Result<()> {
    let fd = socket_ioctl()?;
    let mut flags = get_flags(fd, name)?;
    flags |= libc::IFF_UP as i16;
    let res = set_flags(fd, name, flags);
    unsafe { libc::close(fd) };
    res
}

pub fn set_link_down(name: &str) -> Result<()> {
    let fd = socket_ioctl()?;
    let mut flags = get_flags(fd, name)?;
    flags &= !(libc::IFF_UP as i16);
    let res = set_flags(fd, name, flags);
    unsafe { libc::close(fd) };
    res
}

// ------------------------------------------------------------------
// Bridge
// ------------------------------------------------------------------

pub fn create_bridge(name: &str) -> Result<()> {
    let fd = socket_ioctl()?;
    let req = ifreq_with_name(name);
    let rc = unsafe { libc::ioctl(fd, SIOCBRADDBR, req.as_ptr()) };
    unsafe { libc::close(fd) };
    if rc < 0 {
        let err = last_io_err();
        if err.raw_os_error() == Some(libc::EEXIST) {
            return Ok(());
        }
        return Err(CofferError::Network(format!("ioctl(SIOCBRADDBR): {}", err)));
    }
    Ok(())
}

fn ifindex(fd: libc::c_int, name: &str) -> Result<i32> {
    let mut req = ifreq_with_name(name);
    if unsafe { libc::ioctl(fd, SIOCGIFINDEX, req.as_mut_ptr()) } < 0 {
        return Err(CofferError::Network(format!(
            "ioctl(SIOCGIFINDEX, {}): {}",
            name,
            last_io_err()
        )));
    }
    Ok(i32::from_ne_bytes([
        req[IFNAMSIZ],
        req[IFNAMSIZ + 1],
        req[IFNAMSIZ + 2],
        req[IFNAMSIZ + 3],
    ]))
}

pub fn add_to_bridge(tap: &str, bridge: &str) -> Result<()> {
    let fd = socket_ioctl()?;
    let idx = ifindex(fd, tap)?;

    let mut br_req = ifreq_with_name(bridge);
    br_req[IFNAMSIZ..IFNAMSIZ + 4].copy_from_slice(&idx.to_ne_bytes());

    let rc = unsafe { libc::ioctl(fd, SIOCBRADDIF, br_req.as_ptr()) };
    unsafe { libc::close(fd) };
    if rc < 0 {
        let err = last_io_err();
        if err.raw_os_error() == Some(libc::EEXIST)
            || err.raw_os_error() == Some(libc::EBUSY)
        {
            return Ok(());
        }
        return Err(CofferError::Network(format!("ioctl(SIOCBRADDIF): {}", err)));
    }
    Ok(())
}

pub fn remove_from_bridge(tap: &str, bridge: &str) -> Result<()> {
    let fd = socket_ioctl()?;
    let idx = ifindex(fd, tap)?;

    let mut br_req = ifreq_with_name(bridge);
    br_req[IFNAMSIZ..IFNAMSIZ + 4].copy_from_slice(&idx.to_ne_bytes());

    let rc = unsafe { libc::ioctl(fd, SIOCBRDELIF, br_req.as_ptr()) };
    unsafe { libc::close(fd) };
    if rc < 0 {
        let err = last_io_err();
        if err.raw_os_error() == Some(libc::ENXIO) {
            return Ok(());
        }
        return Err(CofferError::Network(format!("ioctl(SIOCBRDELIF): {}", err)));
    }
    Ok(())
}

// ------------------------------------------------------------------
// IP address / netmask
// ------------------------------------------------------------------

fn parse_cidr(cidr: &str) -> Option<(Ipv4Addr, Ipv4Addr)> {
    let parts: Vec<&str> = cidr.split('/').collect();
    if parts.len() != 2 {
        return None;
    }
    let ip: Ipv4Addr = parts[0].parse().ok()?;
    let prefix: u32 = parts[1].parse().ok()?;
    if prefix > 32 {
        return None;
    }
    let mask = if prefix == 0 {
        0
    } else {
        !0u32 << (32 - prefix)
    };
    let mask = Ipv4Addr::from(mask);
    Some((ip, mask))
}

fn sockaddr_in_bytes(ip: Ipv4Addr) -> [u8; 16] {
    let sin = libc::sockaddr_in {
        sin_family: libc::AF_INET as u16,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from(ip).to_be(),
        },
        sin_zero: [0; 8],
    };
    let mut buf = [0u8; 16];
    let src = unsafe {
        std::slice::from_raw_parts(
            &sin as *const _ as *const u8,
            std::mem::size_of_val(&sin),
        )
    };
    buf.copy_from_slice(src);
    buf
}

pub fn add_ip_to_interface(name: &str, cidr: &str) -> Result<()> {
    let (ip, mask) = parse_cidr(cidr)
        .ok_or_else(|| CofferError::Network(format!("Invalid CIDR: {}", cidr)))?;

    let fd = socket_ioctl()?;

    // Set address.
    {
        let mut req = ifreq_with_name(name);
        let sin = sockaddr_in_bytes(ip);
        req[IFNAMSIZ..IFNAMSIZ + sin.len()].copy_from_slice(&sin);
        if unsafe { libc::ioctl(fd, SIOCSIFADDR, req.as_ptr()) } < 0 {
            let err = last_io_err();
            unsafe { libc::close(fd) };
            return Err(CofferError::Network(format!("ioctl(SIOCSIFADDR): {}", err)));
        }
    }

    // Set netmask.
    {
        let mut req = ifreq_with_name(name);
        let sin = sockaddr_in_bytes(mask);
        req[IFNAMSIZ..IFNAMSIZ + sin.len()].copy_from_slice(&sin);
        if unsafe { libc::ioctl(fd, SIOCSIFNETMASK, req.as_ptr()) } < 0 {
            let err = last_io_err();
            unsafe { libc::close(fd) };
            return Err(CofferError::Network(format!(
                "ioctl(SIOCSIFNETMASK): {}",
                err
            )));
        }
    }

    unsafe { libc::close(fd) };
    Ok(())
}
