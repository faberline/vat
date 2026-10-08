// CODEGEN-BEGIN
//! The VMM process (`vat machine __vmm`): boots the guest on
//! Virtualization.framework and serves the host-side sockets until the guest
//! powers off or the process is asked to stop.
//!
//! Virtualization.framework objects are not thread-safe; every call on the
//! VM or its devices runs on the VM's dispatch queue. The [`Unsync`] wrapper
//! carries object handles onto that queue.
//!
//! The binary must be signed with `com.apple.security.virtualization`;
//! `vat machine start` maintains a signed copy for this purpose.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSString, NSURL};
use objc2_virtualization::*;
use tokio::sync::{mpsc, oneshot};

use super::addon;
use super::assets::BootAssets;
use super::bridge::{self, DialFuture, Dialer};
use super::{MachineConfig, MachinePaths, VmmState, GUEST_DIAL_PORT, HOST_UPLINK_PORT};

/// Moves a non-`Send` Objective-C handle onto the VM queue. Sound because
/// every use happens on that serial queue.
struct Unsync<T>(T);
unsafe impl<T> Send for Unsync<T> {}
unsafe impl<T> Sync for Unsync<T> {}

fn file_url(path: &std::path::Path) -> Retained<NSURL> {
    NSURL::fileURLWithPath(&NSString::from_str(&path.to_string_lossy()))
}

fn ns_err(err: Retained<NSError>) -> anyhow::Error {
    anyhow!("{}", err.localizedDescription())
}

struct UplinkIvars {
    tx: mpsc::UnboundedSender<OwnedFd>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "VatUplinkListenerDelegate"]
    #[ivars = UplinkIvars]
    struct UplinkDelegate;

    unsafe impl NSObjectProtocol for UplinkDelegate {}

    unsafe impl VZVirtioSocketListenerDelegate for UplinkDelegate {
        #[unsafe(method(listener:shouldAcceptNewConnection:fromSocketDevice:))]
        fn should_accept(
            &self,
            _listener: &VZVirtioSocketListener,
            connection: &VZVirtioSocketConnection,
            _device: &VZVirtioSocketDevice,
        ) -> bool {
            // The connection object owns its descriptor; keep our own dup.
            let fd = unsafe { libc::dup(connection.fileDescriptor()) };
            fd >= 0
                && self
                    .ivars()
                    .tx
                    .send(unsafe { OwnedFd::from_raw_fd(fd) })
                    .is_ok()
        }
    }
);

impl UplinkDelegate {
    fn new(tx: mpsc::UnboundedSender<OwnedFd>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(UplinkIvars { tx });
        unsafe { msg_send![super(this), init] }
    }
}

/// A started VM and its queue.
struct Machine {
    vm: Unsync<Retained<VZVirtualMachine>>,
    queue: DispatchRetained<DispatchQueue>,
}

impl Machine {
    fn on_queue<R: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&VZVirtualMachine) -> R + Send + 'static,
    ) -> R {
        let (tx, rx) = std::sync::mpsc::channel();
        let me = self.clone();
        self.queue.exec_sync(move || {
            let _ = tx.send(f(&me.vm.0));
        });
        rx.recv().expect("VM queue dropped the result")
    }

    fn state(self: &Arc<Self>) -> VZVirtualMachineState {
        self.on_queue(|vm| unsafe { vm.state() })
    }

    fn socket_device(vm: &VZVirtualMachine) -> Option<Retained<VZVirtioSocketDevice>> {
        let dev = unsafe { vm.socketDevices() }.firstObject()?;
        dev.downcast::<VZVirtioSocketDevice>().ok()
    }

    /// Open a vsock stream to `port` in the guest.
    fn connect(self: &Arc<Self>, port: u32) -> DialFuture {
        let (tx, rx) = oneshot::channel::<Result<OwnedFd>>();
        let me = self.clone();
        let tx = Unsync(std::sync::Mutex::new(Some(tx)));
        self.queue.exec_async(move || {
            let Some(dev) = Machine::socket_device(&me.vm.0) else {
                if let Some(tx) = tx.0.lock().unwrap().take() {
                    let _ = tx.send(Err(anyhow!("the VM has no vsock device")));
                }
                return;
            };
            let tx = Arc::new(tx);
            let block = RcBlock::new(
                move |conn: *mut VZVirtioSocketConnection, err: *mut NSError| {
                    let result = if conn.is_null() {
                        let msg = if err.is_null() {
                            "vsock connect failed".to_string()
                        } else {
                            unsafe { (*err).localizedDescription() }.to_string()
                        };
                        Err(anyhow!(msg))
                    } else {
                        let fd = unsafe { libc::dup((*conn).fileDescriptor()) };
                        if fd < 0 {
                            Err(std::io::Error::last_os_error().into())
                        } else {
                            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
                        }
                    };
                    if let Some(tx) = tx.0.lock().unwrap().take() {
                        let _ = tx.send(result);
                    }
                },
            );
            unsafe { dev.connectToPort_completionHandler(port, &block) };
        });
        Box::pin(async move {
            let fd = rx.await.map_err(|_| anyhow!("vsock connect dropped"))??;
            fd_to_stream(fd)
        })
    }
}

fn fd_to_stream(fd: OwnedFd) -> Result<tokio::net::UnixStream> {
    let std_stream = std::os::unix::net::UnixStream::from(fd);
    std_stream.set_nonblocking(true)?;
    Ok(tokio::net::UnixStream::from_std(std_stream)?)
}

fn build_config(
    paths: &MachinePaths,
    cfg: &MachineConfig,
    assets: &BootAssets,
) -> Result<(Retained<VZVirtualMachineConfiguration>, bool)> {
    unsafe {
        let boot = VZLinuxBootLoader::initWithKernelURL(
            VZLinuxBootLoader::alloc(),
            &file_url(&assets.kernel),
        );
        boot.setInitialRamdiskURL(Some(&file_url(&assets.initramfs)));
        let now = chrono::Utc::now().timestamp();
        boot.setCommandLine(&NSString::from_str(&format!(
            "console=hvc0 rdinit=/init loglevel=4 vat.time={now}"
        )));

        let vz = VZVirtualMachineConfiguration::new();
        vz.setBootLoader(Some(&boot));
        let max_cpus = VZVirtualMachineConfiguration::maximumAllowedCPUCount() as u32;
        vz.setCPUCount(cfg.cpus.clamp(1, max_cpus.max(1)) as usize);
        vz.setMemorySize(cfg.memory_mib * 1024 * 1024);

        let console = VZVirtioConsoleDeviceSerialPortConfiguration::new();
        let console_att = VZFileSerialPortAttachment::initWithURL_append_error(
            VZFileSerialPortAttachment::alloc(),
            &file_url(&paths.console_log),
            false,
        )
        .map_err(ns_err)
        .context("open the console log")?;
        console.setAttachment(Some(&console_att));
        vz.setSerialPorts(&NSArray::from_retained_slice(&[Retained::into_super(
            console,
        )]));

        let net = VZVirtioNetworkDeviceConfiguration::new();
        net.setAttachment(Some(&VZNATNetworkDeviceAttachment::new()));
        let mac = cfg.mac.as_deref().and_then(|m| {
            VZMACAddress::initWithString(VZMACAddress::alloc(), &NSString::from_str(m))
        });
        if let Some(mac) = mac {
            net.setMACAddress(&mac);
        }
        vz.setNetworkDevices(&NSArray::from_retained_slice(&[Retained::into_super(net)]));

        let disk = VZDiskImageStorageDeviceAttachment::initWithURL_readOnly_cachingMode_synchronizationMode_error(
            VZDiskImageStorageDeviceAttachment::alloc(),
            &file_url(&paths.data_img),
            false,
            VZDiskImageCachingMode::Cached,
            VZDiskImageSynchronizationMode::Fsync,
        )
        .map_err(ns_err)
        .context("attach the data disk")?;
        let block = VZVirtioBlockDeviceConfiguration::initWithAttachment(
            VZVirtioBlockDeviceConfiguration::alloc(),
            &disk,
        );
        vz.setStorageDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            block,
        )]));

        let mut fs_devices: Vec<Retained<VZDirectorySharingDeviceConfiguration>> = Vec::new();
        let state = VZSharedDirectory::initWithURL_readOnly(
            VZSharedDirectory::alloc(),
            &file_url(&paths.share),
            false,
        );
        let state_share =
            VZSingleDirectoryShare::initWithDirectory(VZSingleDirectoryShare::alloc(), &state);
        let state_dev = VZVirtioFileSystemDeviceConfiguration::initWithTag(
            VZVirtioFileSystemDeviceConfiguration::alloc(),
            &NSString::from_str("vat"),
        );
        state_dev.setShare(Some(&state_share));
        fs_devices.push(Retained::into_super(state_dev));

        let mounts: Vec<_> = cfg
            .host_mounts
            .iter()
            .filter(|m| m.source.is_dir())
            .collect();
        if !mounts.is_empty() {
            let keys: Vec<Retained<NSString>> =
                mounts.iter().map(|m| NSString::from_str(&m.name)).collect();
            let dirs: Vec<Retained<VZSharedDirectory>> = mounts
                .iter()
                .map(|m| {
                    VZSharedDirectory::initWithURL_readOnly(
                        VZSharedDirectory::alloc(),
                        &file_url(&m.source),
                        false,
                    )
                })
                .collect();
            let key_refs: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
            let dict = NSDictionary::from_retained_objects(&key_refs, &dirs);
            let share = VZMultipleDirectoryShare::initWithDirectories(
                VZMultipleDirectoryShare::alloc(),
                &dict,
            );
            let dev = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                VZVirtioFileSystemDeviceConfiguration::alloc(),
                &NSString::from_str("vat-host"),
            );
            dev.setShare(Some(&share));
            fs_devices.push(Retained::into_super(dev));
        }

        let rosetta =
            VZLinuxRosettaDirectoryShare::availability() == VZLinuxRosettaAvailability::Installed;
        if rosetta {
            if let Ok(share) =
                VZLinuxRosettaDirectoryShare::initWithError(VZLinuxRosettaDirectoryShare::alloc())
            {
                let dev = VZVirtioFileSystemDeviceConfiguration::initWithTag(
                    VZVirtioFileSystemDeviceConfiguration::alloc(),
                    &NSString::from_str("rosetta"),
                );
                dev.setShare(Some(&share));
                fs_devices.push(Retained::into_super(dev));
            }
        }
        vz.setDirectorySharingDevices(&NSArray::from_retained_slice(&fs_devices));

        vz.setEntropyDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioEntropyDeviceConfiguration::new(),
        )]));
        vz.setSocketDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioSocketDeviceConfiguration::new(),
        )]));
        vz.setMemoryBalloonDevices(&NSArray::from_retained_slice(&[Retained::into_super(
            VZVirtioTraditionalMemoryBalloonDeviceConfiguration::new(),
        )]));

        vz.validateWithError()
            .map_err(ns_err)
            .context("invalid VM configuration")?;
        Ok((vz, rosetta))
    }
}

/// Running host forwards by record file: ((host port, guest port), task).
type Forwards = std::collections::HashMap<&'static str, ((u16, u16), tokio::task::JoinHandle<()>)>;

/// Start or stop the addons' host listeners to match `cfg`, recording each
/// outcome in its state file (e.g. `k8s-api.json` for `vat k8s status`).
async fn sync_forwards(
    paths: &MachinePaths,
    cfg: &MachineConfig,
    dialer: &Dialer,
    running: &mut Forwards,
) {
    for fwd in addon::installed().iter().flat_map(|a| a.forwards(cfg)) {
        let path = paths.dir.join(fwd.record);
        if fwd.ports.is_some() && running.get(fwd.record).map(|(p, _)| *p) == fwd.ports {
            continue;
        }
        if let Some((_, t)) = running.remove(fwd.record) {
            t.abort();
        }
        let Some((host_port, guest_port)) = fwd.ports else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let record = |v: serde_json::Value| {
            let _ = super::write_atomic(&path, v.to_string().as_bytes());
        };
        let addr = format!("127.0.0.1:{host_port}");
        match tokio::net::TcpListener::bind(&addr).await {
            Ok(listener) => {
                running.insert(
                    fwd.record,
                    (
                        (host_port, guest_port),
                        tokio::spawn(bridge::forward_port(listener, guest_port, dialer.clone())),
                    ),
                );
                record(serde_json::json!({ "addr": addr, "listening": true }));
            }
            Err(err) => {
                eprintln!("vmm: cannot forward {} on {addr}: {err}", fwd.name);
                record(
                    serde_json::json!({ "addr": addr, "listening": false, "error": err.to_string() }),
                );
            }
        }
    }
}

/// Start the services the addons serve to the guest from the VMM process
/// (local GCP).
async fn start_builtins(
    paths: &MachinePaths,
    cfg: &MachineConfig,
    dialer: &Dialer,
) -> addon::Builtins {
    let exec_dialer = dialer.clone();
    let exec: addon::GuestExec = Arc::new(move |script: String| {
        let dialer = exec_dialer.clone();
        Box::pin(async move { bridge::exec(&dialer, &script).await })
    });
    let mut builtins = addon::Builtins::new();
    for addon in addon::installed() {
        builtins.extend(addon.builtins(paths, cfg, exec.clone()).await);
    }
    builtins
}

/// Entry point of the hidden `vat machine __vmm` verb.
pub fn run(name: &str, assets: BootAssets) -> Result<()> {
    let t0 = Instant::now();
    let paths = MachinePaths::new(name)?;
    let mut cfg = MachineConfig::load(&paths.config)?
        .with_context(|| format!("machine {name} is not configured"))?;
    if cfg.mac.is_none() {
        let mac = unsafe { VZMACAddress::randomLocallyAdministeredAddress().string() };
        cfg.mac = Some(mac.to_string());
        cfg.save(&paths.config)?;
    }
    let (vz, rosetta) = build_config(&paths, &cfg, &assets)?;

    let queue = DispatchQueue::new("dev.vat.machine", None);
    let vm = unsafe {
        VZVirtualMachine::initWithConfiguration_queue(VZVirtualMachine::alloc(), &vz, &queue)
    };
    let machine = Arc::new(Machine {
        vm: Unsync(vm),
        queue,
    });

    let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
    machine.on_queue(move |vm| {
        let block = RcBlock::new(move |err: *mut NSError| {
            let msg =
                (!err.is_null()).then(|| unsafe { (*err).localizedDescription() }.to_string());
            let _ = tx.send(msg);
        });
        unsafe { vm.startWithCompletionHandler(&block) };
    });
    if let Some(err) = rx.recv().context("VM start callback dropped")? {
        bail!("VM failed to start: {err}");
    }
    let vm_start_ms = t0.elapsed().as_millis() as u64;
    let record = |state: &str| {
        let s = VmmState {
            pid: std::process::id(),
            started_at: chrono::Utc::now().timestamp(),
            vm_start_ms,
            rosetta,
            state: state.to_string(),
        };
        let _ = super::write_atomic(
            &paths.vmm_state,
            serde_json::to_vec_pretty(&s).unwrap_or_default().as_slice(),
        );
    };
    record("running");
    eprintln!("vmm: VM started in {vm_start_ms} ms (rosetta: {rosetta})");

    // Guest -> host uplinks arrive through a delegate on the VM queue.
    let (uplink_tx, mut uplink_rx) = mpsc::unbounded_channel::<OwnedFd>();
    let delegate = UplinkDelegate::new(uplink_tx);
    {
        let delegate = Unsync(delegate.clone());
        machine.on_queue(move |vm| {
            if let Some(dev) = Machine::socket_device(vm) {
                let listener = unsafe { VZVirtioSocketListener::new() };
                unsafe {
                    listener.setDelegate(Some(ProtocolObject::from_ref(&*delegate.0)));
                    dev.setSocketListener_forPort(&listener, HOST_UPLINK_PORT);
                }
            }
        });
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let result = rt.block_on(async {
        let dial_machine = machine.clone();
        let dialer: Dialer = Arc::new(move || dial_machine.connect(GUEST_DIAL_PORT));

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let docker = tokio::spawn(bridge::serve_docker(
            paths.docker_sock.clone(),
            dialer.clone(),
            shutdown_rx,
        ));
        let control = tokio::spawn(bridge::serve_control(
            paths.control_sock.clone(),
            dialer.clone(),
        ));
        let publisher = tokio::spawn(bridge::publish_ports(
            dialer.clone(),
            cfg.publish_addr.clone(),
            paths.dir.join("ports.json"),
        ));
        let builtins = Arc::new(start_builtins(&paths, &cfg, &dialer).await);
        let uplinks = Arc::new(cfg.effective_uplinks());
        tokio::spawn(async move {
            while let Some(fd) = uplink_rx.recv().await {
                let uplinks = uplinks.clone();
                let builtins = builtins.clone();
                tokio::spawn(async move {
                    match fd_to_stream(fd) {
                        Ok(s) => {
                            if let Err(err) = bridge::handle_uplink(s, uplinks, builtins).await {
                                eprintln!("uplink: {err:#}");
                            }
                        }
                        Err(err) => eprintln!("uplink: {err:#}"),
                    }
                });
            }
        });

        // Host forwards (the K3s API) follow the config; SIGHUP re-reads it
        // so `vat k8s up|down` can toggle K8s without restarting the VM.
        let mut forwards = Forwards::new();
        sync_forwards(&paths, &cfg, &dialer, &mut forwards).await;

        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate())?;
        let mut int = signal(SignalKind::interrupt())?;
        let mut hup = signal(SignalKind::hangup())?;
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        let stopped = |s: VZVirtualMachineState| {
            s == VZVirtualMachineState::Stopped || s == VZVirtualMachineState::Error
        };
        loop {
            tokio::select! {
                _ = term.recv() => break,
                _ = int.recv() => break,
                _ = hup.recv() => {
                    match MachineConfig::load(&paths.config) {
                        Ok(Some(fresh)) => sync_forwards(&paths, &fresh, &dialer, &mut forwards).await,
                        Ok(None) => {}
                        Err(err) => eprintln!("vmm: reload config: {err:#}"),
                    }
                }
                _ = tick.tick() => {
                    let m = machine.clone();
                    let s = tokio::task::spawn_blocking(move || m.state()).await?;
                    if stopped(s) {
                        eprintln!("vmm: guest stopped ({s:?})");
                        return Ok::<_, anyhow::Error>(());
                    }
                }
            }
        }
        // Graceful shutdown: ask the guest, then force after a grace period.
        eprintln!("vmm: shutting down the guest");
        record("stopping");
        // Close host-held Docker streams first (the publisher's `/events`
        // among them) so dockerd exits without waiting on them.
        publisher.abort();
        docker.abort();
        for (_, (_, task)) in forwards.drain() {
            task.abort();
        }
        let _ = shutdown_tx.send(true);
        let _ = tokio::time::timeout(Duration::from_secs(3), bridge::poweroff(&dialer)).await;
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let m = machine.clone();
            if stopped(tokio::task::spawn_blocking(move || m.state()).await?) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        control.abort();
        Ok(())
    });

    if !matches!(
        machine.state(),
        VZVirtualMachineState::Stopped | VZVirtualMachineState::Error
    ) {
        eprintln!("vmm: forcing the guest off");
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        machine.on_queue(move |vm| {
            let block = RcBlock::new(move |_err: *mut NSError| {
                let _ = tx.send(());
            });
            unsafe { vm.stopWithCompletionHandler(&block) };
        });
        let _ = rx.recv_timeout(Duration::from_secs(5));
    }
    record("stopped");
    let _ = std::fs::remove_file(&paths.docker_sock);
    let _ = std::fs::remove_file(&paths.control_sock);
    drop(delegate);
    result
}
// CODEGEN-END
