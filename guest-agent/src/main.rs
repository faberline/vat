//! vat-guest: the agent inside the `vat machine` Linux VM.
//!
//! - Dialer on vsock port 1024. The host writes one header line naming the
//!   target, then the connection is a raw byte stream:
//!   `tcp <host:port>`, `unix <path>`, `exec <base64 sh command>` (output,
//!   then `__VAT_EXIT__ <code>`), `ping`, `poweroff`.
//! - Uplinks: each `<bind-addr> <port> <service>` line in the state share's
//!   `guest/uplinks` becomes a guest TCP listener whose connections are sent
//!   to the host (CID 2, vsock port 1025) behind `VATPEER <peer> <service>`.
//! - Heartbeat: `status.json` in the state share every two seconds.
//!
//! Connection close semantics: a half-close from the host reaches the guest,
//! but a guest-side half-close does not reach the host through
//! Virtualization.framework's vsock. So the side acting as the server ending
//! its stream closes the whole connection, while the client side's EOF is
//! forwarded as a half-close.

use std::ffi::CString;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DIAL_PORT: u32 = 1024;
const UPLINK_PORT: u32 = 1025;
const HOST_CID: u32 = 2;
const STATE: &str = "/mnt/vat";
const GUEST: &str = "/mnt/vat/guest";
const DOCKER_SOCK: &str = "/var/run/docker.sock";
const K3S_KUBECONFIG: &str = "/etc/rancher/k3s/k3s.yaml";

fn main() {
    match std::env::args().nth(1).as_deref() {
        None | Some("agent") => agent(),
        Some("--version") => println!("vat-guest {}", env!("CARGO_PKG_VERSION")),
        Some(other) => {
            eprintln!("vat-guest: unknown command {other:?}");
            std::process::exit(2);
        }
    }
}

fn agent() {
    let listener = match vsock_listen(DIAL_PORT) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vat-guest: vsock listen {DIAL_PORT}: {e}");
            std::process::exit(1);
        }
    };
    thread::spawn(move || loop {
        match vsock_accept(&listener) {
            Ok(conn) => {
                thread::spawn(move || dial(conn));
            }
            Err(e) => {
                eprintln!("vat-guest: accept: {e}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    });
    start_uplinks();
    heartbeat();
}

// ---------------------------------------------------------------------------
// vsock sockets. A connected vsock fd is carried as a UnixStream: its
// read/write/shutdown/try_clone are plain socket syscalls that work on any
// stream socket.

fn vsock_socket() -> io::Result<OwnedFd> {
    let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn vsock_addr(cid: u32, port: u32) -> libc::sockaddr_vm {
    let mut addr: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    addr.svm_cid = cid;
    addr.svm_port = port;
    addr
}

fn vsock_listen(port: u32) -> io::Result<OwnedFd> {
    let fd = vsock_socket()?;
    let addr = vsock_addr(libc::VMADDR_CID_ANY, port);
    let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    if unsafe {
        libc::bind(
            fd.as_raw_fd(),
            &addr as *const _ as *const libc::sockaddr,
            len,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    if unsafe { libc::listen(fd.as_raw_fd(), 128) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn vsock_accept(listener: &OwnedFd) -> io::Result<UnixStream> {
    let fd = unsafe {
        libc::accept4(
            listener.as_raw_fd(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            libc::SOCK_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { UnixStream::from_raw_fd(fd) })
}

fn vsock_connect(cid: u32, port: u32) -> io::Result<UnixStream> {
    let fd = vsock_socket()?;
    let addr = vsock_addr(cid, port);
    let len = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    if unsafe {
        libc::connect(
            fd.as_raw_fd(),
            &addr as *const _ as *const libc::sockaddr,
            len,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(UnixStream::from(fd))
}

// ---------------------------------------------------------------------------
// Relaying.

enum Conn {
    Tcp(TcpStream),
    Unix(UnixStream),
}

impl Conn {
    fn try_clone(&self) -> io::Result<Conn> {
        Ok(match self {
            Conn::Tcp(s) => Conn::Tcp(s.try_clone()?),
            Conn::Unix(s) => Conn::Unix(s.try_clone()?),
        })
    }

    fn shutdown(&self, how: Shutdown) {
        let _ = match self {
            Conn::Tcp(s) => s.shutdown(how),
            Conn::Unix(s) => s.shutdown(how),
        };
    }
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.read(buf),
            Conn::Unix(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Conn::Tcp(s) => s.write(buf),
            Conn::Unix(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Relay `client` <-> `server`. The client's EOF becomes a half-close toward
/// the server; the server's EOF (or any error) ends the connection.
fn relay(client: Conn, server: Conn) {
    let (Ok(mut client_r), Ok(mut server_w)) = (client.try_clone(), server.try_clone()) else {
        return;
    };
    let upstream = thread::spawn(move || {
        let _ = io::copy(&mut client_r, &mut server_w);
        server_w.shutdown(Shutdown::Write);
    });
    let (mut server_r, mut client_w) = (server, client);
    let _ = io::copy(&mut server_r, &mut client_w);
    client_w.shutdown(Shutdown::Both);
    server_r.shutdown(Shutdown::Both);
    let _ = upstream.join();
}

// ---------------------------------------------------------------------------
// Host -> guest dialer.

/// Read the header line one byte at a time so nothing past it is consumed.
fn read_header(s: &mut UnixStream) -> io::Result<String> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        if s.read(&mut b)? == 0 || b[0] == b'\n' {
            break;
        }
        line.push(b[0]);
        if line.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header too long",
            ));
        }
    }
    Ok(String::from_utf8_lossy(&line).into_owned())
}

fn dial(mut host: UnixStream) {
    let header = match read_header(&mut host) {
        Ok(h) => h,
        Err(_) => return,
    };
    let (kind, arg) = header.split_once(' ').unwrap_or((header.as_str(), ""));
    match kind {
        "tcp" => match TcpStream::connect(arg) {
            Ok(t) => {
                let _ = t.set_nodelay(true);
                relay(Conn::Unix(host), Conn::Tcp(t));
            }
            Err(e) => eprintln!("vat-guest: dial tcp {arg}: {e}"),
        },
        "unix" => match UnixStream::connect(arg) {
            Ok(u) => relay(Conn::Unix(host), Conn::Unix(u)),
            // dockerd is not up yet during boot; readiness probes expect EOF.
            Err(_) => {}
        },
        "exec" => exec(host, arg),
        "ping" => {
            let _ = host.write_all(b"pong\n");
        }
        "poweroff" => {
            let _ = host.write_all(b"ok\n");
            let _ = host.shutdown(Shutdown::Both);
            thread::sleep(Duration::from_millis(200));
            let _ = Command::new("poweroff").status();
        }
        _ => {
            let _ = writeln!(host, "unknown dial kind: {kind}");
        }
    }
}

fn exec(mut host: UnixStream, b64: &str) {
    let Some(cmd) = base64_decode(b64.trim()) else {
        let _ = host.write_all(b"bad exec payload\n__VAT_EXIT__ 2\n");
        return;
    };
    let cmd = String::from_utf8_lossy(&cmd).into_owned();
    let (Ok(out), Ok(err)) = (host.try_clone(), host.try_clone()) else {
        return;
    };
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(&cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(OwnedFd::from(out)))
        .stderr(Stdio::from(OwnedFd::from(err)))
        .status();
    let code = match status {
        Ok(s) => s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0)),
        Err(_) => 127,
    };
    let _ = writeln!(host, "__VAT_EXIT__ {code}");
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let bytes: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        let take = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return None,
        };
        out.extend_from_slice(&n.to_be_bytes()[1..1 + take]);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Guest -> host uplinks.

fn start_uplinks() {
    let Ok(text) = fs::read_to_string(format!("{GUEST}/uplinks")) else {
        return;
    };
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(addr), Some(port), Some(service)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        if addr != "127.0.0.1" {
            // Link-local service addresses (e.g. 169.254.169.254) live on lo.
            let _ = Command::new("ip")
                .args(["addr", "add", &format!("{addr}/32"), "dev", "lo"])
                .stderr(Stdio::null())
                .status();
        }
        let listener = match TcpListener::bind(format!("{addr}:{port}")) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("vat-guest: uplink {service}: bind {addr}:{port}: {e}");
                continue;
            }
        };
        let service = service.to_string();
        thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let service = service.clone();
                thread::spawn(move || uplink(conn, &service));
            }
        });
    }
}

fn uplink(conn: TcpStream, service: &str) {
    let peer = conn
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "unknown".into());
    let mut host = match vsock_connect(HOST_CID, UPLINK_PORT) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("vat-guest: uplink {service}: vsock connect: {e}");
            return;
        }
    };
    if writeln!(host, "VATPEER {peer} {service}").is_err() {
        return;
    }
    let _ = conn.set_nodelay(true);
    relay(Conn::Tcp(conn), Conn::Unix(host));
}

// ---------------------------------------------------------------------------
// Status heartbeat.

fn heartbeat() {
    let mut k8s_ready = false;
    let mut k8s_checked: Option<Instant> = None;
    loop {
        let ip = ipv4_of("eth0").map(|a| a.to_string()).unwrap_or_default();
        let docker_ready = docker_ping();
        if Path::new(K3S_KUBECONFIG).exists() {
            // Probe often until ready, then back off: `k3s kubectl` is not free.
            let interval = if k8s_ready { 30 } else { 2 };
            if k8s_checked.map_or(true, |t| t.elapsed() >= Duration::from_secs(interval)) {
                k8s_ready = k8s_probe();
                k8s_checked = Some(Instant::now());
            }
        } else {
            k8s_ready = false;
        }
        let (mem_total, mem_avail) = meminfo();
        let (disk_used, disk_total) = disk_usage("/");
        let uptime = fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| s.split_whitespace().next().map(str::to_string))
            .unwrap_or_else(|| "0".into());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let json = format!(
            "{{\"phase\":\"running\",\"ip\":\"{ip}\",\"docker_ready\":{docker_ready},\"k8s_ready\":{k8s_ready},\"mem_total_kib\":{mem_total},\"mem_available_kib\":{mem_avail},\"disk_used_kib\":{disk_used},\"disk_total_kib\":{disk_total},\"uptime_s\":{uptime},\"at\":{now},\"agent\":\"{}\"}}\n",
            env!("CARGO_PKG_VERSION")
        );
        let tmp = format!("{STATE}/status.json.tmp");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, format!("{STATE}/status.json"));
        }
        thread::sleep(Duration::from_secs(2));
    }
}

fn ipv4_of(ifname: &str) -> Option<Ipv4Addr> {
    let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut addrs) } != 0 {
        return None;
    }
    let mut found = None;
    let mut cur = addrs;
    while !cur.is_null() {
        let ifa = unsafe { &*cur };
        cur = ifa.ifa_next;
        if ifa.ifa_addr.is_null() {
            continue;
        }
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) };
        if name.to_bytes() != ifname.as_bytes() {
            continue;
        }
        if unsafe { (*ifa.ifa_addr).sa_family } as i32 != libc::AF_INET {
            continue;
        }
        let sin = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
        found = Some(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)));
        break;
    }
    unsafe { libc::freeifaddrs(addrs) };
    found
}

fn docker_ping() -> bool {
    let Ok(mut s) = UnixStream::connect(DOCKER_SOCK) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    if s.write_all(b"GET /_ping HTTP/1.0\r\nHost: docker\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    out.starts_with(b"HTTP/1.0 200") || out.starts_with(b"HTTP/1.1 200")
}

fn k8s_probe() -> bool {
    let Ok(mut child) = Command::new("k3s")
        .args(["kubectl", "get", "--raw=/readyz"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return s.success(),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn meminfo() -> (u64, u64) {
    let text = fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |key: &str| {
        text.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

fn disk_usage(path: &str) -> (u64, u64) {
    let Ok(c) = CString::new(path) else {
        return (0, 0);
    };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return (0, 0);
    }
    let frsize = st.f_frsize as u64;
    let total = st.f_blocks as u64 * frsize / 1024;
    let used = (st.f_blocks as u64 - st.f_bfree as u64) * frsize / 1024;
    (used, total)
}

#[cfg(test)]
mod tests {
    use super::base64_decode;

    #[test]
    fn decodes_padded_and_unpadded() {
        assert_eq!(base64_decode("aGVsbG8=").unwrap(), b"hello");
        assert_eq!(base64_decode("aGk").unwrap(), b"hi");
        assert_eq!(base64_decode("").unwrap(), b"");
        assert!(base64_decode("a").is_none());
    }
}
