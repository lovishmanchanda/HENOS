//! henos-app: the part every HENOS app shares above the core.
//!
//! [`App`] bundles the network node, the paired-device list and clipboard
//! sync into one object with a small surface made for user interfaces:
//!
//! - [`App::state`] returns a plain snapshot of everything a screen shows.
//! - [`App::subscribe_changes`] says when to ask for a new snapshot.
//! - A handful of actions: start or stop pairing, pair, unpair, pause.
//!
//! The desktop shell (and later the Android one) supplies the system
//! clipboard and draws the snapshot. Nothing here knows about windows.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use henos_core::{
    clipboard::CAPABILITY as CLIPBOARD_CAPABILITY, ClipboardBackend, ClipboardContent,
    ClipboardEvent, ClipboardSync, DeviceId, DeviceInfo, FileKeyStore, Identity, KeyStore,
    LocalCopy, Node, NodeConfig, NodeEvent, PairingTicket, TrustStore,
};
use serde::Serialize;
use tokio::{sync::broadcast, task::JoinHandle};
use tracing::warn;

/// Errors are shown to people, so they travel as ready-to-read sentences.
pub type Result<T> = std::result::Result<T, String>;

/// How many activity entries are kept for display.
const ACTIVITY_LIMIT: usize = 30;

/// Settings for [`App::start`].
pub struct AppConfig {
    /// Folder for this device's identity key and paired-device list.
    pub data_dir: PathBuf,
    /// Name shown to other devices. Defaults to the computer's name.
    pub device_name: Option<String>,
    /// Where the identity key is kept. Defaults to a private file in
    /// `data_dir`.
    pub key_store: Option<Box<dyn KeyStore>>,
    /// Advertise on, and browse, the local network. Off only in tests.
    pub discovery: bool,
}

impl AppConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            device_name: None,
            key_store: None,
            discovery: true,
        }
    }
}

/// Everything a screen needs, as plain data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppState {
    pub version: String,
    pub device: DeviceView,
    /// Devices we are paired with, connected ones first.
    pub paired: Vec<PairedView>,
    /// Other HENOS devices seen on the network that we are not paired with.
    pub nearby: Vec<NearbyView>,
    /// The code we are showing, if we are in pairing mode.
    pub pairing: Option<PairingView>,
    pub clipboard_paused: bool,
    /// Recent events, newest first. Never contains clipboard content.
    pub activity: Vec<Activity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub platform: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedView {
    pub id: String,
    pub name: String,
    pub platform: String,
    /// An encrypted connection is up right now.
    pub connected: bool,
    /// Visible on the local network right now.
    pub nearby: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NearbyView {
    pub id: String,
    pub name: String,
    pub platform: String,
    /// The device says it is showing a pairing code.
    pub pairing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingView {
    /// Six digits, for typing on the other device.
    pub code: String,
    /// The same information as text, for pasting.
    pub ticket: String,
    /// The ticket as a QR code, ready to place in a page.
    pub qr_svg: String,
    pub seconds_left: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    /// Milliseconds since the Unix epoch.
    pub at_ms: u64,
    pub kind: ActivityKind,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ActivityKind {
    Paired,
    Unpaired,
    PairingFailed,
    ClipboardSent,
    ClipboardReceived,
}

struct ShownOffer {
    code: String,
    ticket: String,
    qr_svg: String,
    expires: Instant,
}

/// A running HENOS instance.
pub struct App {
    node: Arc<Node>,
    sync: Arc<ClipboardSync>,
    paused: Arc<AtomicBool>,
    offer: Mutex<Option<ShownOffer>>,
    activity: Mutex<VecDeque<Activity>>,
    /// Last known names, so a device can still be named after it is gone.
    names: Mutex<HashMap<DeviceId, String>>,
    changes: broadcast::Sender<()>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

/// Wraps the platform clipboard so that pausing stops incoming items too.
struct PausableBackend {
    inner: Arc<dyn ClipboardBackend>,
    paused: Arc<AtomicBool>,
}

impl ClipboardBackend for PausableBackend {
    fn write(&self, content: &ClipboardContent) -> std::result::Result<(), String> {
        if self.paused.load(Ordering::Relaxed) {
            return Err("clipboard sharing is paused".into());
        }
        self.inner.write(content)
    }
}

impl App {
    /// Loads (or creates) this device's identity and paired-device list,
    /// starts networking and clipboard sync.
    pub async fn start(
        config: AppConfig,
        clipboard: Arc<dyn ClipboardBackend>,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&config.data_dir).map_err(|e| {
            format!(
                "Could not create the data folder {}: {e}",
                config.data_dir.display()
            )
        })?;
        let key_store = config
            .key_store
            .unwrap_or_else(|| Box::new(FileKeyStore::in_dir(&config.data_dir)));
        let identity = Identity::load_or_create(key_store.as_ref()).map_err(explain)?;
        let trust = Arc::new(TrustStore::open(&config.data_dir).map_err(explain)?);

        let names = trust
            .list()
            .into_iter()
            .map(|d| (d.id, d.device.name))
            .collect();

        let mut node_config = NodeConfig::new(
            identity,
            DeviceInfo::for_this_device(config.device_name),
            trust,
        );
        node_config.capabilities = vec![CLIPBOARD_CAPABILITY.to_owned()];
        node_config.discovery = config.discovery;
        let node = Arc::new(Node::spawn(node_config).await.map_err(explain)?);

        let paused = Arc::new(AtomicBool::new(false));
        let sync = ClipboardSync::start(
            node.clone(),
            Arc::new(PausableBackend {
                inner: clipboard,
                paused: paused.clone(),
            }),
        );

        let (changes, _) = broadcast::channel(16);
        let app = Arc::new(Self {
            node,
            sync,
            paused,
            offer: Mutex::new(None),
            activity: Mutex::default(),
            names: Mutex::new(names),
            changes,
            tasks: Mutex::default(),
        });

        let node_task = tokio::spawn(watch_node(Arc::downgrade(&app), app.node.subscribe()));
        let sync_task = tokio::spawn(watch_clipboard(Arc::downgrade(&app), app.sync.subscribe()));
        app.tasks.lock().unwrap().extend([node_task, sync_task]);
        Ok(app)
    }

    /// A snapshot of everything a screen shows.
    pub fn state(&self) -> AppState {
        let nearby = self.node.nearby();
        let mut paired: Vec<PairedView> = self
            .node
            .paired_devices()
            .into_iter()
            .map(|record| {
                // Prefer what the device says about itself right now.
                let live = self.node.peer(&record.id);
                let device = live.as_ref().map_or(&record.device, |p| &p.device);
                PairedView {
                    id: record.id.to_string(),
                    name: device.name.clone(),
                    platform: device.platform.to_string(),
                    connected: live.is_some(),
                    nearby: nearby.iter().any(|n| n.id == record.id),
                }
            })
            .collect();
        paired.sort_by(|a, b| {
            (!a.connected, a.name.to_lowercase(), &a.id).cmp(&(
                !b.connected,
                b.name.to_lowercase(),
                &b.id,
            ))
        });

        let mut unpaired: Vec<NearbyView> = nearby
            .iter()
            .filter(|n| !n.trusted)
            .map(|n| NearbyView {
                id: n.id.to_string(),
                name: n.device.name.clone(),
                platform: n.device.platform.to_string(),
                pairing: n.pairing,
            })
            .collect();
        unpaired.sort_by(|a, b| {
            (!a.pairing, a.name.to_lowercase(), &a.id).cmp(&(
                !b.pairing,
                b.name.to_lowercase(),
                &b.id,
            ))
        });

        let pairing = if self.node.is_pairing() {
            self.offer
                .lock()
                .unwrap()
                .as_ref()
                .map(|offer| PairingView {
                    code: offer.code.clone(),
                    ticket: offer.ticket.clone(),
                    qr_svg: offer.qr_svg.clone(),
                    seconds_left: offer
                        .expires
                        .saturating_duration_since(Instant::now())
                        .as_secs(),
                })
        } else {
            None
        };

        AppState {
            version: henos_core::VERSION.to_owned(),
            device: DeviceView {
                id: self.node.id().to_string(),
                name: self.node.device().name.clone(),
                platform: self.node.device().platform.to_string(),
            },
            paired,
            nearby: unpaired,
            pairing,
            clipboard_paused: self.paused.load(Ordering::Relaxed),
            activity: self.activity.lock().unwrap().iter().cloned().collect(),
        }
    }

    /// Fires whenever [`App::state`] would return something different.
    pub fn subscribe_changes(&self) -> broadcast::Receiver<()> {
        self.changes.subscribe()
    }

    /// Enters pairing mode and returns a fresh code to show.
    pub fn start_pairing(&self) -> Result<PairingView> {
        let offer = self.node.start_pairing();
        let ticket = offer.ticket.to_string();
        let qr_svg = qr_svg(&ticket)?;
        let view = PairingView {
            code: offer.code.clone(),
            ticket: ticket.clone(),
            qr_svg: qr_svg.clone(),
            seconds_left: offer.expires_in.as_secs(),
        };
        *self.offer.lock().unwrap() = Some(ShownOffer {
            code: offer.code,
            ticket,
            qr_svg,
            expires: Instant::now() + offer.expires_in,
        });
        self.changed();

        // Tell the screen when the code runs out.
        let changes = self.changes.clone();
        let wait = offer.expires_in + Duration::from_millis(200);
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let _ = changes.send(());
        });
        Ok(view)
    }

    /// Leaves pairing mode. The code shown stops working.
    pub fn stop_pairing(&self) {
        self.node.stop_pairing();
        *self.offer.lock().unwrap() = None;
        self.changed();
    }

    /// Pairs with a nearby device that is showing `code`.
    pub async fn pair_with_code(&self, device_id: &str, code: &str) -> Result<PairedView> {
        let id = parse_id(device_id)?;
        let record = self.node.pair_with_code(id, code).await.map_err(explain)?;
        Ok(self.paired_view(record.id))
    }

    /// Pairs using ticket text, as carried by the QR code.
    pub async fn pair_with_ticket(&self, ticket: &str) -> Result<PairedView> {
        let ticket: PairingTicket = ticket.parse().map_err(explain)?;
        let record = self.node.pair_with_ticket(&ticket).await.map_err(explain)?;
        Ok(self.paired_view(record.id))
    }

    /// Forgets a paired device. It forgets this one too.
    pub fn unpair(&self, device_id: &str) -> Result<()> {
        let id = parse_id(device_id)?;
        self.node.unpair(id).map_err(explain)?;
        Ok(())
    }

    /// Looks for devices right away. Call when the person opens the app or
    /// the pairing screen, so they do not wait for the next routine check.
    pub fn look_now(&self) {
        self.node.look_now();
    }

    /// Pauses or resumes clipboard sharing in both directions.
    pub fn set_clipboard_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        self.changed();
    }

    pub fn clipboard_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Call when the user copied something on this device.
    pub async fn local_copy(&self, content: ClipboardContent) {
        if self.paused.load(Ordering::Relaxed) {
            return;
        }
        match self.sync.local_copy(content).await {
            Ok(LocalCopy::Sent { .. } | LocalCopy::Echo) => {}
            Err(e) => warn!("copied content was not shared: {e}"),
        }
    }

    /// The local sockets the node listens on.
    pub fn local_addrs(&self) -> Vec<std::net::SocketAddr> {
        self.node.local_addrs()
    }

    /// Stops networking. The app cannot be used afterwards.
    pub async fn shutdown(&self) {
        for task in self.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        self.sync.stop();
        self.node.shutdown().await;
    }

    fn paired_view(&self, id: DeviceId) -> PairedView {
        let wanted = id.to_string();
        self.state()
            .paired
            .into_iter()
            .find(|p| p.id == wanted)
            .unwrap_or_else(|| PairedView {
                id: wanted,
                name: self.name_of(id),
                platform: "unknown".into(),
                connected: false,
                nearby: false,
            })
    }

    fn name_of(&self, id: DeviceId) -> String {
        if let Some(peer) = self.node.peer(&id) {
            return peer.device.name;
        }
        self.names
            .lock()
            .unwrap()
            .get(&id)
            .cloned()
            .unwrap_or_else(|| format!("device {}", id.fmt_short()))
    }

    fn remember_name(&self, id: DeviceId, name: &str) {
        self.names.lock().unwrap().insert(id, name.to_owned());
    }

    fn log(&self, kind: ActivityKind, text: String) {
        let mut activity = self.activity.lock().unwrap();
        activity.push_front(Activity {
            at_ms: now_ms(),
            kind,
            text,
        });
        activity.truncate(ACTIVITY_LIMIT);
    }

    fn changed(&self) {
        let _ = self.changes.send(());
    }
}

/// Turns node events into activity entries and change notices.
async fn watch_node(app: Weak<App>, mut events: broadcast::Receiver<NodeEvent>) {
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // We missed something: have the screen refresh.
                match app.upgrade() {
                    Some(app) => app.changed(),
                    None => break,
                }
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        let Some(app) = app.upgrade() else { break };
        match event {
            NodeEvent::Paired(record) => {
                app.remember_name(record.id, &record.device.name);
                *app.offer.lock().unwrap() = None;
                app.log(
                    ActivityKind::Paired,
                    format!("Paired with {}", record.device.name),
                );
            }
            NodeEvent::Unpaired(id) => {
                let name = app.name_of(id);
                app.log(
                    ActivityKind::Unpaired,
                    format!("No longer paired with {name}"),
                );
            }
            NodeEvent::PairingAttemptFailed => {
                *app.offer.lock().unwrap() = None;
                app.log(
                    ActivityKind::PairingFailed,
                    "A wrong pairing code was entered, so the code was discarded".into(),
                );
            }
            NodeEvent::PeerConnected(peer) => app.remember_name(peer.id, &peer.device.name),
            // Reported through the clipboard events instead.
            NodeEvent::ClipboardReceived(_) => continue,
            NodeEvent::NearbyFound(_)
            | NodeEvent::NearbyLost(_)
            | NodeEvent::PeerDisconnected(_) => {}
        }
        app.changed();
    }
}

/// Turns clipboard events into activity entries. Sizes only, never content.
async fn watch_clipboard(app: Weak<App>, mut events: broadcast::Receiver<ClipboardEvent>) {
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        };
        let Some(app) = app.upgrade() else { break };
        match event {
            ClipboardEvent::Sent { devices: 0, .. } => continue,
            ClipboardEvent::Sent { bytes, devices } => {
                let to = if devices == 1 {
                    "1 device".to_owned()
                } else {
                    format!("{devices} devices")
                };
                app.log(
                    ActivityKind::ClipboardSent,
                    format!("Sent copied text ({}) to {to}", human_size(bytes)),
                );
            }
            ClipboardEvent::Received { from, bytes } => {
                let name = app.name_of(from);
                app.log(
                    ActivityKind::ClipboardReceived,
                    format!("Received copied text ({}) from {name}", human_size(bytes)),
                );
            }
        }
        app.changed();
    }
}

fn parse_id(device_id: &str) -> Result<DeviceId> {
    device_id
        .parse()
        .map_err(|_| "That is not a valid device id.".to_owned())
}

/// Turns a core error into a sentence for a person.
fn explain(error: henos_core::Error) -> String {
    use henos_core::Error;
    match error {
        Error::WrongPairingCode => {
            "That code is not right. The other device has discarded it, so ask it for a new one."
                .into()
        }
        Error::NotInPairingMode => {
            "The other device is not showing a pairing code, or the code has expired.".into()
        }
        Error::InvalidCode => "A pairing code is 6 digits.".into(),
        Error::InvalidTicket(_) => "That is not a valid HENOS pairing code.".into(),
        Error::Timeout(_) | Error::Connect(_) => {
            "Could not reach the other device. Check that both are on the same network.".into()
        }
        other => {
            let text = other.to_string();
            let mut chars = text.chars();
            match chars.next() {
                Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
                None => text,
            }
        }
    }
}

fn qr_svg(text: &str) -> Result<String> {
    let code = qrcode::QrCode::new(text.as_bytes())
        .map_err(|e| format!("Could not draw the QR code: {e}"))?;
    Ok(code
        .render::<qrcode::render::svg::Color>()
        .min_dimensions(220, 220)
        .dark_color(qrcode::render::svg::Color("#000000"))
        .light_color(qrcode::render::svg::Color("#ffffff"))
        .build())
}

fn human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} bytes")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_read_as_sentences() {
        assert!(explain(henos_core::Error::WrongPairingCode).contains("not right"));
        assert_eq!(
            explain(henos_core::Error::Rejected),
            "The other device does not trust this device."
        );
    }

    #[test]
    fn qr_code_is_an_svg() {
        let svg = qr_svg("henos://pair?v=1&id=abc&code=123456").unwrap();
        assert!(svg.contains("<svg"));
    }

    #[test]
    fn sizes_are_readable() {
        assert_eq!(human_size(12), "12 bytes");
        assert_eq!(human_size(2048), "2.0 KB");
    }
}
