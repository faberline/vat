// CODEGEN-BEGIN
//! Async plumbing between host sockets and guest vsock streams: the Docker
//! Engine socket, the control socket, guest->host uplinks, and published
//! container ports. Independent of the hypervisor: it only needs a way to
//! open a raw stream to the guest dialer.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::task::JoinHandle;

use super::Uplink;

pub type DialFuture = Pin<Box<dyn Future<Output = Result<UnixStream>> + Send>>;

/// Opens a fresh raw stream to the guest dialer (vsock port 1024).
pub type Dialer = Arc<dyn Fn() -> DialFuture + Send + Sync>;

/// Open a guest stream and send its dial header.
pub async fn dial(dialer: &Dialer, header: &str) -> Result<UnixStream> {
    let mut s = dialer().await.context("open vsock stream to the guest")?;
    s.write_all(format!("{header}\n").as_bytes()).await?;
    Ok(s)
}

async fn splice<A, B>(mut a: A, mut b: B)
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let _ = tokio::io::copy_bidirectional(&mut a, &mut b).await;
}

fn bind_unix(path: &PathBuf) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(path);
    UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))
}

/// Host Docker Engine socket: every connection is relayed to the guest's
/// `/var/run/docker.sock`, so the full Engine API (hijacked attach/exec
/// streams, BuildKit sessions, events) works unmodified.
///
/// When `shutdown` flips to true every relayed connection is dropped: dockerd
/// otherwise waits on open streams (e.g. `docker events`) before exiting.
pub async fn serve_docker(
    path: PathBuf,
    dialer: Dialer,
    shutdown: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let listener = bind_unix(&path)?;
    loop {
        let (client, _) = listener.accept().await?;
        let dialer = dialer.clone();
        let mut shutdown = shutdown.clone();
        tokio::spawn(async move {
            // Before dockerd is up the dial fails; the client just sees EOF,
            // which is what readiness probes expect, so stay quiet.
            if let Ok(guest) = dial(&dialer, "unix /var/run/docker.sock").await {
                tokio::select! {
                    _ = splice(client, guest) => {}
                    _ = shutdown.wait_for(|stop| *stop) => {}
                }
            }
        });
    }
}

/// Control socket: the client's first line is a dial header, forwarded as-is.
pub async fn serve_control(path: PathBuf, dialer: Dialer) -> Result<()> {
    let listener = bind_unix(&path)?;
    loop {
        let (client, _) = listener.accept().await?;
        let dialer = dialer.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(client);
            let mut header = String::new();
            if reader.read_line(&mut header).await.unwrap_or(0) == 0 {
                return;
            }
            let header = header.trim_end().to_string();
            // Anything buffered past the header belongs to the payload.
            let buffered = reader.buffer().to_vec();
            let client = reader.into_inner();
            match dial(&dialer, &header).await {
                Ok(mut guest) => {
                    if !buffered.is_empty() && guest.write_all(&buffered).await.is_err() {
                        return;
                    }
                    splice(client, guest).await
                }
                Err(err) => eprintln!("control: {err:#}"),
            }
        });
    }
}

/// A service served inside the VMM: gets the guest stream (header consumed,
/// payload still buffered) and the caller's address.
pub type Builtin = Arc<dyn Fn(BufReader<UnixStream>, String) + Send + Sync>;

/// Handle one guest->host uplink stream: `VATPEER <peer> <service>\n`, then bytes.
pub async fn handle_uplink(
    stream: UnixStream,
    uplinks: Arc<Vec<Uplink>>,
    builtins: Arc<HashMap<String, Builtin>>,
) -> Result<()> {
    let mut reader = BufReader::new(stream);
    let mut header = String::new();
    reader.read_line(&mut header).await?;
    let mut parts = header.split_whitespace();
    if parts.next() != Some("VATPEER") {
        bail!("bad uplink header: {header:?}");
    }
    let peer = parts.next().unwrap_or("0.0.0.0").to_string();
    let service = parts.next().unwrap_or_default();
    let Some(up) = uplinks.iter().find(|u| u.service == service) else {
        bail!("unknown uplink service {service:?}");
    };
    if let Some(name) = up.target.strip_prefix("builtin:") {
        let Some(serve) = builtins.get(name) else {
            bail!("uplink {service}: built-in {name:?} is not running");
        };
        serve(reader, peer);
        return Ok(());
    }
    let buffered = reader.buffer().to_vec();
    let guest = reader.into_inner();
    let mut prefix = Vec::new();
    if up.proxy_protocol {
        let family = if peer.contains(':') { "TCP6" } else { "TCP4" };
        prefix.extend_from_slice(
            format!("PROXY {family} {peer} {} 0 {}\r\n", up.bind, up.port).as_bytes(),
        );
    }
    prefix.extend_from_slice(&buffered);
    if let Some(addr) = up.target.strip_prefix("tcp:") {
        let mut host = TcpStream::connect(addr)
            .await
            .with_context(|| format!("uplink {service}: connect {addr}"))?;
        host.write_all(&prefix).await?;
        splice(guest, host).await;
    } else if let Some(path) = up.target.strip_prefix("unix:") {
        let mut host = UnixStream::connect(path)
            .await
            .with_context(|| format!("uplink {service}: connect {path}"))?;
        host.write_all(&prefix).await?;
        splice(guest, host).await;
    } else {
        bail!("uplink {service}: unsupported target {}", up.target);
    }
    Ok(())
}

/// Run `sh -c <script>` in the guest; returns the exit code and output.
pub async fn exec(dialer: &Dialer, script: &str) -> Result<(i32, String)> {
    let s = dial(
        dialer,
        &format!("exec {}", super::client::base64(script.as_bytes())),
    )
    .await?;
    let mut reader = BufReader::new(s);
    let mut out = String::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            bail!("guest command ended without an exit status");
        }
        if let Some((rest, code)) = super::client::split_exit(line.as_bytes()) {
            out.push_str(&String::from_utf8_lossy(rest));
            return Ok((code, out));
        }
        out.push_str(&line);
    }
}

/// Ask the guest to power off cleanly.
pub async fn poweroff(dialer: &Dialer) -> Result<()> {
    let mut s = dial(dialer, "poweroff").await?;
    let mut out = String::new();
    let _ = tokio::time::timeout(Duration::from_secs(2), s.read_to_string(&mut out)).await;
    Ok(())
}

/// HTTP/1.0 GET against the guest Docker socket; returns the body.
async fn docker_get(dialer: &Dialer, path: &str) -> Result<String> {
    let mut s = dial(dialer, "unix /var/run/docker.sock").await?;
    s.write_all(format!("GET {path} HTTP/1.0\r\nHost: docker\r\n\r\n").as_bytes())
        .await?;
    let mut raw = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut raw))
        .await
        .context("docker API timed out")??;
    let text = String::from_utf8_lossy(&raw);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    if !head.starts_with("HTTP/1.0 200") && !head.starts_with("HTTP/1.1 200") {
        bail!("docker API {path}: {}", head.lines().next().unwrap_or(""));
    }
    Ok(body.to_string())
}

/// Published TCP ports of running containers, keyed by host port.
fn published_ports(containers_json: &str) -> BTreeMap<u16, String> {
    let mut out = BTreeMap::new();
    let Ok(serde_json::Value::Array(list)) = serde_json::from_str(containers_json) else {
        return out;
    };
    for c in list {
        let name = c["Names"][0]
            .as_str()
            .unwrap_or_default()
            .trim_start_matches('/')
            .to_string();
        for p in c["Ports"].as_array().into_iter().flatten() {
            if p["Type"].as_str() != Some("tcp") {
                continue;
            }
            if let Some(port) = p["PublicPort"].as_u64().and_then(|n| u16::try_from(n).ok()) {
                out.insert(port, name.clone());
            }
        }
    }
    out
}

/// Mirror every published container port onto the host so `docker run -p`
/// behaves like Docker Desktop: `<publish_addr>:<port>` on macOS reaches the
/// container. Writes the current mapping to `ports_file` for status.
pub async fn publish_ports(dialer: Dialer, publish_addr: String, ports_file: PathBuf) {
    let mut active: HashMap<u16, JoinHandle<()>> = HashMap::new();
    let mut failed: BTreeMap<u16, String> = BTreeMap::new();
    loop {
        // Subscribe before scanning: a container started between the scan and
        // a later subscribe would go unseen until the periodic resync, and a
        // client that connects as soon as the container logs "ready"
        // (Testcontainers) would find the port closed.
        let events = container_events(&dialer).await;
        match docker_get(&dialer, "/containers/json").await {
            Ok(body) => {
                let want = published_ports(&body);
                active.retain(|port, task| {
                    let keep = want.contains_key(port);
                    if !keep {
                        task.abort();
                    }
                    keep
                });
                failed.retain(|port, _| want.contains_key(port));
                for port in want.keys() {
                    if active.contains_key(port) {
                        continue;
                    }
                    let addr = format!("{publish_addr}:{port}");
                    match TcpListener::bind(&addr).await {
                        Ok(listener) => {
                            failed.remove(port);
                            let dialer = dialer.clone();
                            let port = *port;
                            active.insert(port, tokio::spawn(forward_port(listener, port, dialer)));
                        }
                        Err(err) => {
                            failed.insert(*port, err.to_string());
                        }
                    }
                }
                let snapshot = serde_json::json!({
                    "publish_addr": publish_addr,
                    "ports": want.iter().map(|(p, c)| serde_json::json!({
                        "port": p,
                        "container": c,
                        "published": active.contains_key(p),
                        "error": failed.get(p),
                    })).collect::<Vec<_>>(),
                });
                let _ = super::write_atomic(&ports_file, snapshot.to_string().as_bytes());
                wait_for_container_event(events).await;
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

/// Subscribe to container events. dockerd sends the response head once the
/// subscription is live, so every event after this returns is buffered.
async fn container_events(dialer: &Dialer) -> Option<BufReader<UnixStream>> {
    let fut = async {
        let mut s = dial(dialer, "unix /var/run/docker.sock").await.ok()?;
        let filter = "%7B%22type%22%3A%5B%22container%22%5D%7D";
        s.write_all(
            format!("GET /events?filters={filter} HTTP/1.0\r\nHost: docker\r\n\r\n").as_bytes(),
        )
        .await
        .ok()?;
        let mut reader = BufReader::new(s);
        let mut line = String::new();
        // Skip the response head.
        loop {
            line.clear();
            if reader.read_line(&mut line).await.ok()? == 0 {
                return None;
            }
            if line == "\r\n" {
                break;
            }
        }
        Some(reader)
    };
    tokio::time::timeout(Duration::from_secs(5), fut)
        .await
        .ok()
        .flatten()
}

/// Block until the next container event (or a periodic resync timeout).
async fn wait_for_container_event(events: Option<BufReader<UnixStream>>) {
    match events {
        Some(mut reader) => {
            let mut line = String::new();
            let _ =
                tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut line)).await;
        }
        // No subscription: resync soon rather than waiting out the full period.
        None => tokio::time::sleep(Duration::from_secs(1)).await,
    }
    // No settle delay: Docker binds a published port before `start` returns,
    // so every millisecond here is a window where a client sees it closed. A
    // burst of events just costs a few rescans, each subscribed first.
}

/// Relay every connection on `listener` to `127.0.0.1:<port>` in the guest.
pub async fn forward_port(listener: TcpListener, port: u16, dialer: Dialer) {
    loop {
        let Ok((client, _)) = listener.accept().await else {
            continue;
        };
        let _ = client.set_nodelay(true);
        let dialer = dialer.clone();
        tokio::spawn(async move {
            if let Ok(guest) = dial(&dialer, &format!("tcp 127.0.0.1:{port}")).await {
                splice(client, guest).await;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::published_ports;

    #[test]
    fn published_ports_keeps_tcp_public_ports() {
        let body = r#"[{"Names":["/web"],"Ports":[
            {"IP":"0.0.0.0","PrivatePort":80,"PublicPort":8080,"Type":"tcp"},
            {"IP":"::","PrivatePort":80,"PublicPort":8080,"Type":"tcp"},
            {"PrivatePort":443,"Type":"tcp"},
            {"IP":"0.0.0.0","PrivatePort":53,"PublicPort":5353,"Type":"udp"}]}]"#;
        let ports = published_ports(body);
        assert_eq!(ports.len(), 1);
        assert_eq!(ports.get(&8080).map(String::as_str), Some("web"));
    }
}
// CODEGEN-END
