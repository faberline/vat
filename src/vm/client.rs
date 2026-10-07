// CODEGEN-BEGIN
//! Host-side client for a running machine: dial guest endpoints through the
//! VMM control socket, run commands, and probe the Docker socket.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use super::MachinePaths;

/// Open a raw stream to a guest endpoint. `header` is a dialer line such as
/// `unix /var/run/docker.sock` or `tcp 127.0.0.1:6443`.
pub fn dial(paths: &MachinePaths, header: &str) -> Result<UnixStream> {
    let mut stream = UnixStream::connect(&paths.control_sock).with_context(|| {
        format!(
            "connect {} (is the machine running? try `vat machine start`)",
            paths.control_sock.display()
        )
    })?;
    stream.write_all(header.as_bytes())?;
    stream.write_all(b"\n")?;
    Ok(stream)
}

/// Result of a guest command.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub output: String,
}

/// Run `script` with `sh -c` in the guest, streaming combined output to
/// `sink` as it arrives. Returns the exit code.
pub fn exec_streaming(paths: &MachinePaths, script: &str, sink: &mut dyn Write) -> Result<i32> {
    let stream = dial(paths, &format!("exec {}", base64(script.as_bytes())))?;
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            bail!("guest command ended without an exit status");
        }
        if let Some((output, code)) = split_exit(&line) {
            sink.write_all(output)?;
            sink.flush()?;
            return Ok(code);
        }
        sink.write_all(&line)?;
        sink.flush()?;
    }
}

/// The agent writes `__VAT_EXIT__ <code>\n` straight after the command's
/// output, so it shares a line with output that lacks a trailing newline.
pub(crate) fn split_exit(line: &[u8]) -> Option<(&[u8], i32)> {
    const MARK: &[u8] = b"__VAT_EXIT__ ";
    let at = line.windows(MARK.len()).rposition(|w| w == MARK)?;
    let code = std::str::from_utf8(&line[at + MARK.len()..]).ok()?.trim();
    Some((&line[..at], code.parse().ok()?))
}

/// Run `script` in the guest and capture its combined output.
pub fn exec(paths: &MachinePaths, script: &str) -> Result<ExecOutput> {
    let mut buf = Vec::new();
    let exit_code = exec_streaming(paths, script, &mut buf)?;
    Ok(ExecOutput {
        exit_code,
        output: String::from_utf8_lossy(&buf).into_owned(),
    })
}

/// Whether the guest agent answers.
pub fn ping(paths: &MachinePaths) -> bool {
    let Ok(mut s) = dial(paths, "ping") else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    out.trim() == "pong"
}

/// `GET /_ping` on a Docker Engine unix socket.
pub fn docker_ping(sock: &Path) -> bool {
    docker_get(sock, "/_ping")
        .map(|(status, body)| status == 200 && body.trim() == "OK")
        .unwrap_or(false)
}

/// Minimal HTTP/1.0 GET against a Docker Engine socket; returns status and body.
pub fn docker_get(sock: &Path, path: &str) -> Result<(u16, String)> {
    let mut s = UnixStream::connect(sock)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(s, "GET {path} HTTP/1.0\r\nHost: docker\r\n\r\n")?;
    // Frame by Content-Length rather than EOF: the vsock relay may hold the
    // stream open after dockerd has answered.
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        if let Some(end) = find(&raw, b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..end]).into_owned();
            let len = head.lines().find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse::<usize>().ok())
                    .flatten()
            });
            if let Some(len) = len {
                while raw.len() < end + 4 + len {
                    let n = s.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                }
                raw.truncate((end + 4 + len).min(raw.len()));
                break;
            }
        }
        let n = s.read(&mut buf)?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    Ok((status, body.to_string()))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Standard base64 (with padding), used for the dialer `exec` header.
pub fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{base64, split_exit};

    #[test]
    fn exit_marker_after_unterminated_output() {
        assert_eq!(split_exit(b"__VAT_EXIT__ 0\n"), Some((&b""[..], 0)));
        assert_eq!(split_exit(b"{}__VAT_EXIT__ 3\n"), Some((&b"{}"[..], 3)));
        assert_eq!(split_exit(b"plain output\n"), None);
        assert_eq!(split_exit(b"__VAT_EXIT__ soon\n"), None);
    }

    #[test]
    fn base64_matches_rfc4648_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
// CODEGEN-END
