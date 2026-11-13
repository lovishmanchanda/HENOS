//! Headless tool for developing and testing HENOS without the GUI apps.
//!
//! Each simulated device keeps its identity key and paired-device list in its
//! own `--data-dir`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use henos_clipboard_desktop::{ClipboardWatch, SystemClipboard};
use henos_core::{
    clipboard::CAPABILITY as CLIPBOARD_CAPABILITY, ClipboardBackend, ClipboardContent,
    ClipboardEvent, ClipboardSync, DeviceInfo, FileKeyStore, Identity, NearbyDevice, Node,
    NodeConfig, NodeEvent, PairedDevice, PairingTicket, TransferEvent, TrustStore,
};
use tokio::{
    sync::{broadcast::error::RecvError, mpsc},
    task::JoinHandle,
};

#[derive(Parser)]
#[command(name = "henos", version, about = "henos developer tool")]
struct Cli {
    /// Folder holding this device's identity key and paired-device list.
    /// Each simulated device needs its own.
    #[arg(long, env = "HENOS_DATA_DIR", global = true)]
    data_dir: Option<PathBuf>,

    /// Name shown to other devices. Defaults to the computer's name.
    #[arg(long, global = true)]
    name: Option<String>,

    /// Print debug logs.
    #[arg(long, short, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print this device's id, creating the identity on first use.
    Id,
    /// List paired devices.
    Devices,
    /// Pair with another device.
    #[command(subcommand)]
    Pair(PairCommand),
    /// Forget a paired device, given its name or the start of its id.
    Unpair { device: String },
    /// Run a node: discover nearby devices and connect to paired ones.
    Run {
        /// Do not advertise or browse on the local network.
        #[arg(long)]
        no_discovery: bool,
        /// Exit after this many seconds instead of waiting for Ctrl-C.
        #[arg(long = "for", value_name = "SECONDS")]
        run_for: Option<u64>,
        /// Share this computer's real clipboard with paired devices.
        #[arg(long, conflicts_with = "clipboard_file")]
        clipboard: bool,
        /// Use a text file as a stand-in clipboard: saving the file is a
        /// "copy", and received items are written to it. Lets several
        /// simulated devices run on one computer.
        #[arg(long, value_name = "FILE")]
        clipboard_file: Option<PathBuf>,
        /// Accept every file offered by a paired device and save it into
        /// this folder. (The apps ask the person instead.)
        #[arg(long, value_name = "FOLDER")]
        receive_into: Option<PathBuf>,
        /// Offer this file to each paired device as soon as it connects.
        #[arg(long, value_name = "FILE")]
        send: Option<PathBuf>,
    },
}

/// What the tool does with files, for testing transfers without the apps.
struct Files {
    receive_into: Option<PathBuf>,
    send: Option<PathBuf>,
}

fn handle_files(node: &Arc<Node>, files: &Files, event: &NodeEvent) {
    match event {
        NodeEvent::PeerConnected(peer) => {
            if let Some(path) = files.send.clone() {
                let (node, to) = (node.clone(), peer.id);
                tokio::spawn(async move {
                    if let Err(e) = node.send_file(to, path).await {
                        println!("file         could not be offered: {e}");
                    }
                });
            }
        }
        NodeEvent::Transfer(TransferEvent::Offered { id, .. }) => match &files.receive_into {
            Some(folder) => {
                node.accept_file(*id, folder);
            }
            None => {
                node.decline_file(*id);
            }
        },
        _ => {}
    }
}

enum ClipboardMode {
    Off,
    System,
    File(PathBuf),
}

#[derive(Subcommand)]
enum PairCommand {
    /// Show a pairing code and wait for another device to use it.
    Show,
    /// Pair with a device that is showing a code.
    Join(JoinArgs),
}

#[derive(Args)]
struct JoinArgs {
    /// The 6-digit code shown on the other device.
    #[arg(long, conflicts_with = "ticket")]
    code: Option<String>,
    /// Which nearby device to pair with (name or start of id), if several
    /// are showing a code.
    #[arg(long, requires = "code")]
    device: Option<String>,
    /// The full ticket text (what a QR code carries) instead of a code.
    #[arg(long)]
    ticket: Option<String>,
}

/// One simulated device's on-disk state.
struct Device {
    identity: Identity,
    trust: Arc<TrustStore>,
    info: DeviceInfo,
}

impl Device {
    fn open(data_dir: &Path, name: Option<String>) -> Result<Self> {
        let identity = Identity::load_or_create(&FileKeyStore::in_dir(data_dir))
            .with_context(|| format!("could not load identity from {}", data_dir.display()))?;
        let trust = TrustStore::open(data_dir).with_context(|| {
            format!("could not load paired devices from {}", data_dir.display())
        })?;
        Ok(Self {
            identity,
            trust: Arc::new(trust),
            info: DeviceInfo::for_this_device(name),
        })
    }

    async fn start(self, discovery: bool, clipboard: bool) -> Result<Arc<Node>> {
        let mut config = NodeConfig::new(self.identity, self.info, self.trust);
        config.discovery = discovery;
        if clipboard {
            config.capabilities.push(CLIPBOARD_CAPABILITY.to_owned());
        }
        config
            .capabilities
            .push(henos_core::transfer::CAPABILITY.to_owned());
        let node = Node::spawn(config).await?;
        println!(
            "device   {} ({})",
            node.device().name,
            node.device().platform
        );
        println!("id       {}", node.id());
        Ok(Arc::new(node))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_max_level(if cli.verbose {
            tracing::Level::DEBUG
        } else {
            tracing::Level::WARN
        })
        .with_writer(std::io::stderr)
        .init();

    let data_dir = cli
        .data_dir
        .context("pass --data-dir <folder> (or set HENOS_DATA_DIR)")?;
    let device = Device::open(&data_dir, cli.name)?;

    match cli.command {
        Command::Id => {
            println!("{}", device.identity.id());
            Ok(())
        }
        Command::Devices => {
            let paired = device.trust.list();
            if paired.is_empty() {
                println!("no paired devices");
            }
            for d in paired {
                println!(
                    "{}  {} ({})",
                    d.id.fmt_short(),
                    d.device.name,
                    d.device.platform
                );
            }
            Ok(())
        }
        Command::Unpair { device: selector } => {
            let target = select_paired(&device.trust.list(), &selector)?;
            device.trust.remove(&target.id)?;
            println!("unpaired {}", target.device.name);
            Ok(())
        }
        Command::Pair(PairCommand::Show) => pair_show(device).await,
        Command::Pair(PairCommand::Join(args)) => pair_join(device, args).await,
        Command::Run {
            no_discovery,
            run_for,
            clipboard,
            clipboard_file,
            receive_into,
            send,
        } => {
            let mode = match (clipboard, clipboard_file) {
                (_, Some(file)) => ClipboardMode::File(file),
                (true, None) => ClipboardMode::System,
                (false, None) => ClipboardMode::Off,
            };
            let files = Files { receive_into, send };
            run(device, !no_discovery, run_for, mode, files).await
        }
    }
}

async fn run(
    device: Device,
    discovery: bool,
    run_for: Option<u64>,
    clipboard: ClipboardMode,
    files: Files,
) -> Result<()> {
    let sharing = !matches!(clipboard, ClipboardMode::Off);
    let node = device.start(discovery, sharing).await?;
    println!("paired with {} device(s)", node.paired_devices().len());
    let clipboard = start_clipboard(&node, clipboard)?;
    println!("waiting for devices on the local network...");

    let mut events = node.subscribe();
    let stop = async {
        match run_for {
            Some(secs) => tokio::time::sleep(Duration::from_secs(secs)).await,
            None => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    };
    tokio::pin!(stop);

    loop {
        tokio::select! {
            _ = &mut stop => break,
            event = events.recv() => match event {
                Ok(event) => {
                    handle_files(&node, &files, &event);
                    report(&node, event)
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            },
        }
    }

    drop(clipboard);
    node.shutdown().await;
    Ok(())
}

/// Everything that keeps clipboard sharing alive. Stops when dropped.
struct ClipboardRuntime {
    sync: Arc<ClipboardSync>,
    _watch: Option<ClipboardWatch>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for ClipboardRuntime {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        self.sync.stop();
    }
}

/// A text file standing in for the system clipboard.
struct FileClipboard {
    path: PathBuf,
}

impl ClipboardBackend for FileClipboard {
    fn write(&self, content: &ClipboardContent) -> Result<(), String> {
        let ClipboardContent::Text(text) = content else {
            return Err("only text is supported".into());
        };
        // Write then rename, so the file is never seen half-written.
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }
}

fn start_clipboard(node: &Arc<Node>, mode: ClipboardMode) -> Result<Option<ClipboardRuntime>> {
    // Copies made on this device, from whichever source is in use.
    let (copies_tx, mut copies_rx) = mpsc::unbounded_channel::<ClipboardContent>();
    let mut tasks = Vec::new();

    let (backend, watch): (Arc<dyn ClipboardBackend>, Option<ClipboardWatch>) = match mode {
        ClipboardMode::Off => return Ok(None),
        ClipboardMode::System => {
            let backend = SystemClipboard::new().map_err(anyhow::Error::msg)?;
            let watch = henos_clipboard_desktop::watch(move |content| {
                let _ = copies_tx.send(content);
            })
            .map_err(anyhow::Error::msg)?;
            println!("clipboard    sharing this computer's clipboard");
            (Arc::new(backend), Some(watch))
        }
        ClipboardMode::File(path) => {
            println!("clipboard    using {} as the clipboard", path.display());
            let watched = path.clone();
            tasks.push(tokio::spawn(async move {
                // Whatever is in the file at startup is not a new copy.
                let mut last = tokio::fs::read_to_string(&watched).await.ok();
                loop {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let now = tokio::fs::read_to_string(&watched).await.ok();
                    if now != last {
                        if let Some(text) = now.clone() {
                            let _ = copies_tx.send(ClipboardContent::Text(text));
                        }
                        last = now;
                    }
                }
            }));
            (Arc::new(FileClipboard { path }), None)
        }
    };

    let sync = ClipboardSync::start(node.clone(), backend);

    let sender = sync.clone();
    tasks.push(tokio::spawn(async move {
        while let Some(content) = copies_rx.recv().await {
            if let Err(e) = sender.local_copy(content).await {
                println!("clipboard    not sent: {e}");
            }
        }
    }));

    // Report activity, never the content itself.
    let mut events = sync.subscribe();
    let reporter = node.clone();
    tasks.push(tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ClipboardEvent::Sent { bytes, devices, .. }) => {
                    println!("clipboard    sent {bytes} bytes to {devices} device(s)")
                }
                Ok(ClipboardEvent::Received { from, bytes, .. }) => {
                    let name = reporter
                        .peer(&from)
                        .map(|p| p.device.name)
                        .unwrap_or_else(|| from.fmt_short().to_string());
                    println!("clipboard    received {bytes} bytes from {name}");
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    }));

    Ok(Some(ClipboardRuntime {
        sync,
        _watch: watch,
        tasks,
    }))
}

async fn pair_show(device: Device) -> Result<()> {
    let node = device.start(true, false).await?;
    let mut events = node.subscribe();
    let offer = node.start_pairing();
    println!();
    println!("pairing code   {} {}", &offer.code[..3], &offer.code[3..]);
    println!("ticket         {}", offer.ticket);
    println!(
        "valid for {} seconds, waiting for the other device...",
        offer.expires_in.as_secs()
    );

    let expired = tokio::time::sleep(offer.expires_in);
    tokio::pin!(expired);
    let outcome = loop {
        tokio::select! {
            _ = &mut expired => break Err("the code expired"),
            _ = tokio::signal::ctrl_c() => break Err("cancelled"),
            event = events.recv() => match event {
                Ok(NodeEvent::Paired(d)) => break Ok(d),
                Ok(NodeEvent::PairingAttemptFailed) => {
                    break Err("a wrong code was entered, so this code was discarded")
                }
                Ok(_) | Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break Err("node stopped"),
            },
        }
    };

    node.shutdown().await;
    match outcome {
        Ok(d) => {
            println!("paired with {} ({})", d.device.name, d.device.platform);
            Ok(())
        }
        Err(why) => bail!("not paired: {why}"),
    }
}

async fn pair_join(device: Device, args: JoinArgs) -> Result<()> {
    let ticket: Option<PairingTicket> = args.ticket.as_deref().map(str::parse).transpose()?;
    if ticket.is_none() && args.code.is_none() {
        bail!("pass --code <6 digits> or --ticket <text>");
    }

    let node = device.start(true, false).await?;
    let result = async {
        match ticket {
            Some(ticket) => Ok(node.pair_with_ticket(&ticket).await?),
            None => {
                let code = args.code.as_deref().unwrap_or_default();
                let host = find_pairing_device(&node, args.device.as_deref()).await?;
                println!("pairing with {}...", host.device.name);
                Ok(node.pair_with_code(host.id, code).await?)
            }
        }
    }
    .await;
    // Let the first real connection come up before leaving.
    if result.is_ok() {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    node.shutdown().await;

    let paired: PairedDevice = result.map_err(|e: anyhow::Error| e.context("not paired"))?;
    println!(
        "paired with {} ({})",
        paired.device.name, paired.device.platform
    );
    Ok(())
}

/// Waits for a nearby device that is showing a pairing code.
async fn find_pairing_device(node: &Node, selector: Option<&str>) -> Result<NearbyDevice> {
    println!("looking for a device showing a pairing code...");
    let mut candidates = Vec::new();
    for _ in 0..60 {
        candidates = node
            .nearby()
            .into_iter()
            .filter(|d| d.pairing)
            .filter(|d| selector.is_none_or(|s| matches_device(s, &d.id.to_string(), &d.device)))
            .collect();
        if !candidates.is_empty() {
            // Give other devices a moment to show up before deciding.
            tokio::time::sleep(Duration::from_millis(750)).await;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    match candidates.len() {
        0 => bail!("no nearby device is showing a pairing code"),
        1 => Ok(candidates.remove(0)),
        _ => {
            for d in &candidates {
                eprintln!("  {}  {}", d.id.fmt_short(), d.device.name);
            }
            bail!("several devices are showing a code; choose one with --device")
        }
    }
}

fn matches_device(selector: &str, id: &str, device: &DeviceInfo) -> bool {
    id.starts_with(&selector.to_lowercase()) || device.name.eq_ignore_ascii_case(selector)
}

fn select_paired(paired: &[PairedDevice], selector: &str) -> Result<PairedDevice> {
    let mut found: Vec<_> = paired
        .iter()
        .filter(|d| matches_device(selector, &d.id.to_string(), &d.device))
        .collect();
    match found.len() {
        0 => bail!("no paired device matches {selector:?}"),
        1 => Ok(found.remove(0).clone()),
        _ => bail!("{selector:?} matches several devices; use more of the id"),
    }
}

fn report(node: &Arc<Node>, event: NodeEvent) {
    match event {
        NodeEvent::NearbyFound(d) => println!(
            "nearby       {} ({}, {}) {}{}",
            d.device.name,
            d.device.platform,
            d.id.fmt_short(),
            if d.trusted { "paired" } else { "not paired" },
            if d.pairing {
                ", showing a pairing code"
            } else {
                ""
            },
        ),
        NodeEvent::NearbyLost(id) => println!("gone         {}", id.fmt_short()),
        NodeEvent::PeerConnected(peer) => {
            println!(
                "connected    {} ({}, {})",
                peer.device.name,
                peer.device.platform,
                peer.id.fmt_short()
            );
            // Prove the encrypted channel carries data both ways.
            let node = node.clone();
            tokio::spawn(async move {
                match node.ping(peer.id).await {
                    Ok(rtt) => println!(
                        "ping         {} answered in {:.1} ms",
                        peer.device.name,
                        rtt.as_secs_f64() * 1000.0
                    ),
                    Err(e) => println!("ping         {} failed: {e}", peer.device.name),
                }
            });
        }
        NodeEvent::PeerDisconnected(id) => println!("disconnected {}", id.fmt_short()),
        NodeEvent::Paired(d) => println!("paired       {} ({})", d.device.name, d.device.platform),
        NodeEvent::Unpaired(id) => println!("unpaired     {}", id.fmt_short()),
        NodeEvent::PairingAttemptFailed => println!("pairing      wrong code entered"),
        // Reported by the clipboard runtime, without the content.
        NodeEvent::ClipboardReceived(_) => {}
        NodeEvent::Transfer(event) => match event {
            TransferEvent::Offered { name, size, .. } => {
                println!("file         offered: {name} ({size} bytes)")
            }
            TransferEvent::Sending { name, size, .. } => {
                println!("file         offering {name} ({size} bytes)")
            }
            TransferEvent::Accepted { .. } => println!("file         accepted, transferring"),
            TransferEvent::Progress { .. } => {}
            TransferEvent::Finished {
                path: Some(path), ..
            } => {
                println!("file         saved to {}", path.display())
            }
            TransferEvent::Finished { path: None, .. } => println!("file         delivered"),
            TransferEvent::Failed { reason, .. } => println!("file         failed: {reason}"),
        },
    }
}
