//! The running network node: finds nearby devices, pairs with them, and keeps
//! one encrypted connection to each paired device.
//!
//! Scope of this version: same network only. Relays, DNS publishing and
//! router port mapping are all off, so nothing leaves the local network.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use iroh::{
    endpoint::{presets, Connection, ConnectionError, RecvStream, SendStream},
    endpoint_info::UserData,
    protocol::{AcceptError, ProtocolHandler, Router},
    Endpoint, EndpointAddr, RelayMode,
};
use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use n0_future::StreamExt;
use tokio::{
    sync::{broadcast, mpsc, oneshot},
    task::JoinHandle,
    time::timeout,
};
use tracing::{debug, info, warn};

use crate::{
    beacon::{self, Beacon, BEACON_PORT},
    clipboard::{self, ClipboardHeader, ClipboardItem},
    device::{sanitize_name, DeviceInfo, Platform},
    error::{Error, Result},
    identity::{DeviceId, Identity},
    pairing::{self, PairingTicket, CODE_TTL, PAIR_ALPN},
    protocol::{
        decode, read_frame, write_frame, Control, Hello, Ready, StreamHeader, ALPN,
        MAX_CONTROL_FRAME, MAX_STREAM_HEADER, PROTOCOL_VERSION,
    },
    transfer::{self, TransferEvent, Transfers},
    trust::{unix_now, PairedDevice, TrustStore},
};

/// How often we ask the network who is there. The library's default is
/// several times a second, which keeps the processor waking up for nothing;
/// a device appearing a couple of seconds later is not noticeable.
const DISCOVERY_CADENCE: Duration = Duration::from_secs(3);
/// mDNS service name. Only HENOS devices advertise and browse it.
const MDNS_SERVICE: &str = "henos";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const PING_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for one pairing attempt, start to finish.
const PAIR_TIMEOUT: Duration = Duration::from_secs(30);
/// How often we retry paired nearby devices we are not connected to.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(10);
/// Being refused by a device we paired with this recently does not count as
/// being unpaired: the other side may simply not have saved us yet.
const UNPAIR_GRACE_SECS: u64 = 30;
/// Upper bound for sending or receiving one item on its own stream.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);
/// Identifies the "I am here" exchange between devices on the same network.
const HELLO_ALPN: &[u8] = b"henos/hello/1";
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HELLO_FRAME: usize = 1024;
/// A device known only from its greetings is forgotten after this long
/// without a new one.
const GREETING_TTL: Duration = Duration::from_secs(65);
/// How often we re-check whether a device that said it sees us still does.
const GREETING_RECHECK: Duration = Duration::from_secs(60);
/// Most devices remembered from greetings alone.
const MAX_GREETED_DEVICES: usize = 64;
/// Beacons go out in a quick burst when HENOS starts or the person is
/// looking for devices, then settle to this slow rhythm.
const BEACON_BURST: [Duration; 5] = [
    Duration::from_millis(0),
    Duration::from_millis(400),
    Duration::from_millis(1200),
    Duration::from_secs(3),
    Duration::from_secs(6),
];
const BEACON_IDLE_INTERVAL: Duration = Duration::from_secs(20);
/// At most one answer to the same device within this time.
const BEACON_REPLY_GAP: Duration = Duration::from_secs(1);
/// How long a replaced connection is kept open for the other side to close.
const REPLACED_LINGER: Duration = Duration::from_secs(10);

/// Application close codes sent when a connection is shut on purpose.
mod close {
    pub const NORMAL: u32 = 0;
    pub const NOT_TRUSTED: u32 = 1;
    pub const INCOMPATIBLE: u32 = 2;
    pub const DUPLICATE: u32 = 3;
    pub const REPLACED: u32 = 4;
    pub const PAIR_WRONG_CODE: u32 = 10;
    pub const PAIR_NOT_OFFERING: u32 = 11;
}

/// Settings for [`Node::spawn`].
pub struct NodeConfig {
    pub identity: Identity,
    pub device: DeviceInfo,
    /// The paired-device list. Pairing and unpairing update it.
    pub trust: Arc<TrustStore>,
    /// Features to announce to peers, for example `"clipboard"`.
    pub capabilities: Vec<String>,
    /// Advertise on, and browse, the local network.
    pub discovery: bool,
    /// Connect to paired devices as soon as they are seen nearby.
    pub auto_connect: bool,
    /// How long a pairing code stays valid.
    pub pairing_code_ttl: Duration,
    /// UDP port for beacons, HENOS's own discovery; `None` turns them off
    /// and 0 picks a free port. With `discovery` off, the standard port is
    /// not opened either, so isolated nodes stay isolated; only another
    /// port (as tests use) is.
    pub beacon_port: Option<u16>,
}

impl NodeConfig {
    /// The normal configuration: discovery and auto-connect on.
    pub fn new(identity: Identity, device: DeviceInfo, trust: Arc<TrustStore>) -> Self {
        Self {
            identity,
            device,
            trust,
            capabilities: Vec::new(),
            discovery: true,
            auto_connect: true,
            pairing_code_ttl: CODE_TTL,
            beacon_port: Some(BEACON_PORT),
        }
    }
}

/// A device we hold a live, authenticated connection to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub id: DeviceId,
    pub device: DeviceInfo,
    pub capabilities: Vec<String>,
}

/// A HENOS device seen on the local network. Everything here except `trusted`
/// is unauthenticated broadcast data: fine for a device list, never for trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NearbyDevice {
    pub id: DeviceId,
    pub device: DeviceInfo,
    /// Whether we are paired with this device.
    pub trusted: bool,
    /// Whether the device says it is currently showing a pairing code.
    pub pairing: bool,
    pub addrs: Vec<SocketAddr>,
}

/// What [`Node::start_pairing`] hands to the UI to display.
#[derive(Debug, Clone)]
pub struct PairingOffer {
    /// The code to show, for typing on the other device.
    pub code: String,
    /// The same information for a QR code: `ticket.to_string()`.
    pub ticket: PairingTicket,
    pub expires_in: Duration,
}

/// Things that happen on the node, for UIs and sync logic to react to.
#[derive(Debug, Clone)]
pub enum NodeEvent {
    /// A nearby device appeared or its details changed.
    NearbyFound(NearbyDevice),
    NearbyLost(DeviceId),
    PeerConnected(Peer),
    PeerDisconnected(DeviceId),
    /// Pairing completed, started from either side.
    Paired(PairedDevice),
    /// A device was unpaired, by us or by the other device.
    Unpaired(DeviceId),
    /// Someone entered a wrong code. The code shown is no longer valid.
    PairingAttemptFailed,
    /// A paired device sent a clipboard item. Handled by
    /// [`crate::clipboard::ClipboardSync`].
    ClipboardReceived(Arc<ClipboardItem>),
    /// Something happened to a file transfer.
    Transfer(TransferEvent),
}

/// Handle to the running node. Dropping it does not stop the node; call
/// [`Node::shutdown`].
pub struct Node {
    shared: Arc<Shared>,
    router: Router,
    discovery_task: Option<JoinHandle<()>>,
    beacon_tasks: Vec<JoinHandle<()>>,
}

struct Shared {
    id: DeviceId,
    device: DeviceInfo,
    capabilities: Vec<String>,
    trust: Arc<TrustStore>,
    auto_connect: bool,
    pairing_code_ttl: Duration,
    endpoint: Endpoint,
    peers: Mutex<HashMap<DeviceId, PeerHandle>>,
    nearby: Mutex<HashMap<DeviceId, NearbyEntry>>,
    dialing: Mutex<HashSet<DeviceId>>,
    greetings: Mutex<HashMap<DeviceId, GreetState>>,
    transfers: Arc<Transfers>,
    /// The beacon socket, when beacons are on.
    beacon: Option<Arc<tokio::net::UdpSocket>>,
    /// Wakes the beacon sender for an immediate burst.
    beacon_boost: tokio::sync::Notify,
    /// When we last answered each device's beacon.
    beacon_replies: Mutex<HashMap<DeviceId, Instant>>,
    /// The code currently on offer, if we are in pairing mode.
    offer: Mutex<Option<Offer>>,
    /// Bumped whenever pairing mode is started or stopped by the user.
    offer_generation: AtomicU64,
    events: broadcast::Sender<NodeEvent>,
    /// Random per run. See [`Ready`].
    instance: u64,
    next_ready_seq: AtomicU64,
    next_session: AtomicU64,
    next_nonce: AtomicU64,
}

struct PeerHandle {
    peer: Peer,
    session: u64,
    /// The [`Ready`] that put this connection into use.
    ready: Ready,
    conn: Connection,
    outgoing: mpsc::Sender<Control>,
    pending_pings: Arc<Mutex<HashMap<u64, oneshot::Sender<()>>>>,
}

struct NearbyEntry {
    device: NearbyDevice,
    addr: EndpointAddr,
    /// We hear this device's own announcements on the network.
    via_mdns: bool,
    /// When the device last told us directly that it is here.
    greeted_at: Option<Instant>,
}

/// What we know about whether a nearby device can see us.
#[derive(Default)]
struct GreetState {
    /// The device said it hears our announcements, as of `checked_at`.
    sees_us: bool,
    checked_at: Option<Instant>,
    in_flight: bool,
}

/// Sent to a device we can see, in case it cannot see us.
///
/// Network discovery is sometimes one-way: one computer hears the other's
/// announcements but not the reverse (common with Windows and with some
/// routers). A device that hears another therefore introduces itself over a
/// direct connection, so both end up seeing each other.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct Greeting {
    /// Same text as the network announcement: name, platform, pairing flag.
    announcement: String,
    /// Where we can be reached.
    addrs: Vec<SocketAddr>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct GreetingReply {
    /// The receiver already hears our announcements, so greetings can stop.
    sees_you: bool,
    /// The receiver's own current announcement, so one exchange brings both
    /// sides up to date.
    announcement: String,
}

struct Offer {
    code: String,
    expires: Instant,
    generation: u64,
}

#[derive(Clone, Copy)]
enum Role {
    Dialer,
    Acceptor,
}

enum Registered {
    New,
    Replaced,
    Rejected,
}

impl Node {
    /// Starts networking and, if enabled, local discovery.
    pub async fn spawn(config: NodeConfig) -> Result<Self> {
        let id = config.identity.id();
        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(config.identity.secret_key().clone())
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .map_err(|e| Error::Bind(e.to_string()))?;

        let beacon_port = if config.discovery {
            config.beacon_port
        } else {
            config.beacon_port.filter(|port| *port != BEACON_PORT)
        };
        let beacon_socket = beacon_port.and_then(|port| match beacon::bind(port) {
            Ok(socket) => Some(Arc::new(socket)),
            Err(e) => {
                // Not fatal: the other ways of finding devices still work.
                warn!("could not open the beacon port: {e}");
                None
            }
        });

        let (events, _) = broadcast::channel(256);
        let shared = Arc::new(Shared {
            id,
            device: config.device,
            capabilities: config.capabilities,
            trust: config.trust,
            auto_connect: config.auto_connect,
            pairing_code_ttl: config.pairing_code_ttl,
            endpoint: endpoint.clone(),
            peers: Mutex::default(),
            nearby: Mutex::default(),
            dialing: Mutex::default(),
            greetings: Mutex::default(),
            transfers: Arc::new(Transfers::new(events.clone())),
            beacon: beacon_socket,
            beacon_boost: tokio::sync::Notify::new(),
            beacon_replies: Mutex::default(),
            offer: Mutex::default(),
            offer_generation: AtomicU64::new(0),
            events,
            instance: rand::random(),
            next_ready_seq: AtomicU64::new(1),
            next_session: AtomicU64::new(1),
            next_nonce: AtomicU64::new(1),
        });

        let router = Router::builder(endpoint)
            .accept(
                ALPN,
                LinkProtocol {
                    shared: shared.clone(),
                },
            )
            .accept(
                PAIR_ALPN,
                PairProtocol {
                    shared: shared.clone(),
                },
            )
            .accept(
                HELLO_ALPN,
                HelloProtocol {
                    shared: shared.clone(),
                },
            )
            .spawn();

        let discovery_task = if config.discovery {
            match start_discovery(shared.clone()) {
                Ok(task) => Some(task),
                Err(e) => {
                    let _ = router.shutdown().await;
                    return Err(e);
                }
            }
        } else {
            None
        };

        let mut beacon_tasks = Vec::new();
        if shared.beacon.is_some() {
            beacon_tasks.push(tokio::spawn(receive_beacons(shared.clone())));
            if config.discovery {
                beacon_tasks.push(tokio::spawn(send_beacons(shared.clone())));
            }
        }

        info!(id = %id.fmt_short(), "node started");
        Ok(Self {
            beacon_tasks,
            shared,
            router,
            discovery_task,
        })
    }

    /// This device's id.
    pub fn id(&self) -> DeviceId {
        self.shared.id
    }

    /// This device's name and platform.
    pub fn device(&self) -> &DeviceInfo {
        &self.shared.device
    }

    /// The local sockets the node listens on.
    pub fn local_addrs(&self) -> Vec<SocketAddr> {
        self.shared.endpoint.bound_sockets()
    }

    /// Subscribes to node events from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<NodeEvent> {
        self.shared.events.subscribe()
    }

    /// HENOS devices currently visible on the local network.
    pub fn nearby(&self) -> Vec<NearbyDevice> {
        let nearby = self.shared.nearby.lock().unwrap();
        nearby.values().map(|e| e.device.clone()).collect()
    }

    /// Devices we are connected to right now.
    pub fn peers(&self) -> Vec<Peer> {
        let peers = self.shared.peers.lock().unwrap();
        peers.values().map(|h| h.peer.clone()).collect()
    }

    /// Devices we are paired with, connected or not.
    pub fn paired_devices(&self) -> Vec<PairedDevice> {
        self.shared.trust.list()
    }

    /// Enters pairing mode and returns a fresh code to show. Any earlier code
    /// stops working. The code is good for one pairing and one wrong guess.
    pub fn start_pairing(&self) -> PairingOffer {
        let shared = &self.shared;
        let generation = shared.offer_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let code = pairing::generate_code();
        let ttl = shared.pairing_code_ttl;
        *shared.offer.lock().unwrap() = Some(Offer {
            code: code.clone(),
            expires: Instant::now() + ttl,
            generation,
        });
        announce(shared);

        // Leave pairing mode when the code expires.
        let timer_shared = shared.clone();
        tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            let expired = {
                let mut offer = timer_shared.offer.lock().unwrap();
                let ours = matches!(&*offer, Some(o) if o.generation == generation);
                if ours {
                    *offer = None;
                }
                ours
            };
            if expired {
                announce(&timer_shared);
            }
        });

        let ticket = PairingTicket {
            id: shared.id,
            code: code.clone(),
            addrs: shared.endpoint.addr().ip_addrs().copied().collect(),
        };
        PairingOffer {
            code,
            ticket,
            expires_in: ttl,
        }
    }

    /// Leaves pairing mode. The current code stops working.
    pub fn stop_pairing(&self) {
        self.shared.offer_generation.fetch_add(1, Ordering::SeqCst);
        *self.shared.offer.lock().unwrap() = None;
        announce(&self.shared);
    }

    /// Whether a pairing code is currently on offer.
    pub fn is_pairing(&self) -> bool {
        self.shared.is_pairing()
    }

    /// Pairs with a nearby device that is showing `code`.
    pub async fn pair_with_code(&self, id: DeviceId, code: &str) -> Result<PairedDevice> {
        let code = pairing::normalize_code(code)?;
        let addr = {
            let nearby = self.shared.nearby.lock().unwrap();
            nearby.get(&id).map(|e| e.addr.clone())
        };
        pair(
            &self.shared,
            addr.unwrap_or_else(|| EndpointAddr::new(id)),
            code,
        )
        .await
    }

    /// Pairs using a scanned or pasted ticket.
    pub async fn pair_with_ticket(&self, ticket: &PairingTicket) -> Result<PairedDevice> {
        let code = pairing::normalize_code(&ticket.code)?;
        let mut addr = EndpointAddr::new(ticket.id);
        for a in &ticket.addrs {
            addr = addr.with_ip_addr(*a);
        }
        if ticket.addrs.is_empty() {
            let nearby = self.shared.nearby.lock().unwrap();
            if let Some(entry) = nearby.get(&ticket.id) {
                addr = entry.addr.clone();
            }
        }
        pair(&self.shared, addr, code).await
    }

    /// Stops trusting a device and drops its connection. The other device
    /// notices and forgets us too. Returns whether the device was paired.
    pub fn unpair(&self, id: DeviceId) -> Result<bool> {
        let removed = self.shared.trust.remove(&id);
        // Cut the connection even if saving the list failed.
        let conn = {
            let peers = self.shared.peers.lock().unwrap();
            peers.get(&id).map(|h| h.conn.clone())
        };
        if let Some(conn) = conn {
            conn.close(close::NOT_TRUSTED.into(), b"unpaired");
        }
        let removed = removed?;
        if removed {
            info!(peer = %id.fmt_short(), "unpaired");
            self.shared.refresh_nearby(id);
            self.shared.emit(NodeEvent::Unpaired(id));
        }
        Ok(removed)
    }

    /// Looks for devices right now instead of waiting for the next routine
    /// check. Call when the person opens the app or starts pairing.
    pub fn look_now(&self) {
        self.shared.beacon_boost.notify_one();
    }

    /// The address beacons are received on, if beacons are on.
    pub fn beacon_addr(&self) -> Option<SocketAddr> {
        self.shared.beacon.as_ref()?.local_addr().ok()
    }

    /// Sends one beacon straight to `addr` (another device's beacon address).
    pub async fn beacon_to(&self, addr: SocketAddr) -> Result<()> {
        send_beacon(&self.shared, addr, false).await
    }

    /// Tells the device at `addrs` that we are here, so that it lists us as
    /// nearby even if it cannot hear our network announcements. Returns
    /// whether it could already see us.
    pub async fn greet(&self, id: DeviceId, addrs: &[SocketAddr]) -> Result<bool> {
        let mut addr = EndpointAddr::new(id);
        for a in addrs {
            addr = addr.with_ip_addr(*a);
        }
        send_greeting(&self.shared, addr).await
    }

    /// Connects to a paired device that was discovered nearby. Returns the
    /// existing connection if there already is one.
    pub async fn connect(&self, id: DeviceId) -> Result<Peer> {
        let addr = {
            let nearby = self.shared.nearby.lock().unwrap();
            nearby.get(&id).map(|e| e.addr.clone())
        };
        // Without a discovery record, let the endpoint try to resolve the id.
        connect(&self.shared, addr.unwrap_or_else(|| EndpointAddr::new(id))).await
    }

    /// Connects to a paired device at known socket addresses, bypassing
    /// discovery.
    pub async fn connect_direct(&self, id: DeviceId, addrs: &[SocketAddr]) -> Result<Peer> {
        let mut addr = EndpointAddr::new(id);
        for a in addrs {
            addr = addr.with_ip_addr(*a);
        }
        connect(&self.shared, addr).await
    }

    /// Offers a file to a connected device. Returns the transfer's id; what
    /// happens next arrives as [`NodeEvent::Transfer`] events.
    pub async fn send_file(
        &self,
        to: DeviceId,
        path: impl Into<std::path::PathBuf>,
    ) -> Result<u64> {
        let conn = {
            let peers = self.shared.peers.lock().unwrap();
            let handle = peers.get(&to).ok_or(Error::NotConnected(to))?;
            if !handle
                .peer
                .capabilities
                .iter()
                .any(|c| c == transfer::CAPABILITY)
            {
                return Err(Error::Protocol(
                    "the other device cannot receive files".into(),
                ));
            }
            handle.conn.clone()
        };
        self.shared.transfers.send(conn, to, path.into()).await
    }

    /// Accepts an offered file and saves it into `folder`. Returns false if
    /// the offer is no longer waiting.
    pub fn accept_file(&self, id: u64, folder: impl Into<std::path::PathBuf>) -> bool {
        self.shared.transfers.answer(id, Some(folder.into()))
    }

    /// Declines an offered file.
    pub fn decline_file(&self, id: u64) -> bool {
        self.shared.transfers.answer(id, None)
    }

    /// Stops a transfer that is waiting or under way.
    pub fn cancel_transfer(&self, id: u64) {
        self.shared.transfers.cancel(id);
    }

    /// The features this device announces to peers.
    pub fn capabilities(&self) -> &[String] {
        &self.shared.capabilities
    }

    /// The live connection to `id`, if there is one.
    pub fn peer(&self, id: &DeviceId) -> Option<Peer> {
        self.shared.peer(id)
    }

    /// Sends a clipboard item to every connected device that takes part in
    /// clipboard sync. Returns how many devices it was delivered to.
    pub async fn send_clipboard(&self, item: Arc<ClipboardItem>) -> usize {
        let targets: Vec<(DeviceId, Connection)> = {
            let peers = self.shared.peers.lock().unwrap();
            peers
                .values()
                .filter(|h| {
                    h.peer
                        .capabilities
                        .iter()
                        .any(|c| c == clipboard::CAPABILITY)
                })
                .map(|h| (h.peer.id, h.conn.clone()))
                .collect()
        };
        let mut sends = tokio::task::JoinSet::new();
        for (id, conn) in targets {
            let item = item.clone();
            sends.spawn(async move {
                let sent = timeout(STREAM_TIMEOUT, send_clipboard_item(&conn, &item)).await;
                match sent {
                    Ok(Ok(())) => true,
                    Ok(Err(e)) => {
                        debug!(peer = %id.fmt_short(), "could not send clipboard item: {e}");
                        false
                    }
                    Err(_) => {
                        debug!(peer = %id.fmt_short(), "sending clipboard item timed out");
                        false
                    }
                }
            });
        }
        let mut delivered = 0;
        while let Some(result) = sends.join_next().await {
            if matches!(result, Ok(true)) {
                delivered += 1;
            }
        }
        delivered
    }

    /// Sends a ping over the encrypted connection and returns the round trip
    /// time.
    pub async fn ping(&self, id: DeviceId) -> Result<Duration> {
        let (outgoing, pending) = {
            let peers = self.shared.peers.lock().unwrap();
            let handle = peers.get(&id).ok_or(Error::NotConnected(id))?;
            (handle.outgoing.clone(), handle.pending_pings.clone())
        };
        let nonce = self.shared.next_nonce.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(nonce, tx);

        let started = Instant::now();
        let result = async {
            outgoing
                .send(Control::Ping { nonce })
                .await
                .map_err(|_| Error::ConnectionClosed)?;
            match timeout(PING_TIMEOUT, rx).await {
                Ok(Ok(())) => Ok(started.elapsed()),
                Ok(Err(_)) => Err(Error::ConnectionClosed),
                Err(_) => Err(Error::Timeout("ping")),
            }
        }
        .await;
        pending.lock().unwrap().remove(&nonce);
        result
    }

    /// Closes every connection and stops the node.
    pub async fn shutdown(&self) {
        if let Some(task) = &self.discovery_task {
            task.abort();
        }
        for task in &self.beacon_tasks {
            task.abort();
        }
        let conns: Vec<Connection> = {
            let peers = self.shared.peers.lock().unwrap();
            peers.values().map(|h| h.conn.clone()).collect()
        };
        for conn in conns {
            conn.close(close::NORMAL.into(), b"shutdown");
        }
        if let Err(e) = self.router.shutdown().await {
            warn!("error while shutting down: {e}");
        }
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("id", &self.shared.id.fmt_short().to_string())
            .field("device", &self.shared.device)
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn peer(&self, id: &DeviceId) -> Option<Peer> {
        self.peers.lock().unwrap().get(id).map(|h| h.peer.clone())
    }

    fn emit(&self, event: NodeEvent) {
        // No subscribers is fine.
        let _ = self.events.send(event);
    }

    /// Whether this device picks the connection to use with `other`: the one
    /// with the smaller id does.
    fn decides_for(&self, other: &DeviceId) -> bool {
        self.id.as_bytes() < other.as_bytes()
    }

    /// Puts a session into use on the deciding device and returns the
    /// announcement to send. The newest connection always wins; an older one
    /// is left for the other side to close.
    ///
    /// The sequence number is taken under the same lock as the insert, so
    /// "highest number" and "the one in use here" can never disagree.
    fn register_as_decider(&self, mut handle: PeerHandle) -> (Registered, Ready) {
        let mut peers = self.peers.lock().unwrap();
        let ready = Ready {
            instance: self.instance,
            seq: self.next_ready_seq.fetch_add(1, Ordering::Relaxed),
        };
        handle.ready = ready;
        let old = peers.insert(handle.peer.id, handle);
        drop(peers);
        let outcome = match old {
            None => Registered::New,
            Some(old) => {
                // Not closed right away: the other device may still be using
                // it until it sees the `Ready` for the new one.
                retire(self.transfers.clone(), old);
                Registered::Replaced
            }
        };
        (outcome, ready)
    }

    /// Puts a session into use on the other device, once the decider has
    /// announced it. `Ready` messages for two connections can arrive in
    /// either order, so an announcement older than the one in use is ignored.
    fn register_as_follower(&self, handle: PeerHandle) -> Registered {
        let mut peers = self.peers.lock().unwrap();
        if let Some(old) = peers.get(&handle.peer.id) {
            if old.ready.instance == handle.ready.instance && old.ready.seq > handle.ready.seq {
                return Registered::Rejected;
            }
        }
        match peers.insert(handle.peer.id, handle) {
            None => Registered::New,
            Some(old) => {
                drop(peers);
                retire(self.transfers.clone(), old);
                Registered::Replaced
            }
        }
    }

    /// Removes a session if it is still the current one for that device.
    fn unregister(&self, id: DeviceId, session: u64) {
        let removed = {
            let mut peers = self.peers.lock().unwrap();
            match peers.get(&id) {
                Some(handle) if handle.session == session => peers.remove(&id).is_some(),
                _ => false,
            }
        };
        if removed {
            info!(peer = %id.fmt_short(), "disconnected");
            self.emit(NodeEvent::PeerDisconnected(id));
        }
    }

    fn is_pairing(&self) -> bool {
        let offer = self.offer.lock().unwrap();
        matches!(&*offer, Some(o) if o.expires > Instant::now())
    }

    /// Takes the current code for one pairing attempt. While an attempt runs
    /// there is no code on offer, so attempts cannot overlap.
    fn take_offer(&self) -> Option<Offer> {
        let offer = self.offer.lock().unwrap().take()?;
        (offer.expires > Instant::now()).then_some(offer)
    }

    /// Puts a code back after an attempt that broke off before the code was
    /// tested, unless it expired or the user changed pairing mode meanwhile.
    fn restore_offer(&self, offer: Offer) {
        if offer.expires <= Instant::now()
            || offer.generation != self.offer_generation.load(Ordering::SeqCst)
        {
            return;
        }
        let mut slot = self.offer.lock().unwrap();
        if slot.is_none() {
            *slot = Some(offer);
        }
    }

    /// Re-evaluates a nearby device's `trusted` flag after pairing changes.
    fn refresh_nearby(&self, id: DeviceId) {
        let updated = {
            let mut nearby = self.nearby.lock().unwrap();
            nearby.get_mut(&id).and_then(|entry| {
                let trusted = self.trust.is_trusted(&id);
                (entry.device.trusted != trusted).then(|| {
                    entry.device.trusted = trusted;
                    entry.device.clone()
                })
            })
        };
        if let Some(device) = updated {
            self.emit(NodeEvent::NearbyFound(device));
        }
    }

    /// A paired device told us, over an authenticated connection, that it no
    /// longer trusts us. Forget it too, so unpairing on one device unpairs
    /// both.
    fn peer_unpaired_us(&self, id: DeviceId) {
        let Some(record) = self.trust.get(&id) else {
            return;
        };
        if unix_now().saturating_sub(record.paired_at) < UNPAIR_GRACE_SECS {
            return;
        }
        match self.trust.remove(&id) {
            Ok(true) => {
                info!(peer = %id.fmt_short(), "unpaired by the other device");
                self.refresh_nearby(id);
                self.emit(NodeEvent::Unpaired(id));
            }
            Ok(false) => {}
            Err(e) => warn!("could not save the paired-device list: {e}"),
        }
    }
}

/// Closes a connection that has been replaced by a newer one, once nothing
/// is using it any more: after a short wait for the other device to switch
/// over, and never while a file is still moving on it.
fn retire(transfers: Arc<Transfers>, old: PeerHandle) {
    tokio::spawn(async move {
        // Hold the whole handle, not just the connection: its session stops
        // as soon as the handle is dropped.
        let old = old;
        tokio::time::sleep(REPLACED_LINGER).await;
        while transfers.is_busy(old.conn.stable_id()) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        old.conn.close(close::REPLACED.into(), b"replaced");
    });
}

/// Dials `addr` unless we are already connected to that device.
async fn connect(shared: &Arc<Shared>, addr: EndpointAddr) -> Result<Peer> {
    let id = addr.id;
    if !shared.trust.is_trusted(&id) {
        return Err(Error::NotTrusted(id));
    }
    if let Some(peer) = shared.peer(&id) {
        return Ok(peer);
    }

    let conn = timeout(CONNECT_TIMEOUT, shared.endpoint.connect(addr, ALPN))
        .await
        .map_err(|_| Error::Timeout("connect"))?
        .map_err(|e| Error::Connect(e.to_string()))?;

    match open_session(shared, conn, Role::Dialer).await {
        Ok((peer, session)) => {
            tokio::spawn(session.run());
            Ok(peer)
        }
        Err(Error::Duplicate) => {
            // The other device dialed us at the same time and that connection
            // won. It is normally in use already.
            for _ in 0..40 {
                if let Some(peer) = shared.peer(&id) {
                    return Ok(peer);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(Error::Connect("lost a simultaneous connection race".into()))
        }
        Err(Error::Rejected) => {
            shared.peer_unpaired_us(id);
            Err(Error::Rejected)
        }
        Err(e) => Err(e),
    }
}

/// Runs the handshake on a new connection and registers the session.
async fn open_session(
    shared: &Arc<Shared>,
    conn: Connection,
    role: Role,
) -> Result<(Peer, Session)> {
    let remote = conn.remote_id();

    // The encrypted handshake already proved the remote holds this key. Now
    // check it is a key we paired with, before exchanging any data.
    if !shared.trust.is_trusted(&remote) {
        conn.close(close::NOT_TRUSTED.into(), b"not trusted");
        return Err(Error::NotTrusted(remote));
    }

    let we_decide = shared.decides_for(&remote);
    let handshake = async {
        let (mut send, mut recv) = match role {
            Role::Dialer => conn.open_bi().await,
            Role::Acceptor => conn.accept_bi().await,
        }
        .map_err(|e| Error::Connect(e.to_string()))?;
        let ours = Hello::new(&shared.device, &shared.capabilities);
        write_frame(&mut send, &ours, MAX_CONTROL_FRAME).await?;
        let theirs: Hello = read_handshake_frame(&mut recv).await?;
        if theirs.protocol_version != PROTOCOL_VERSION {
            return Err(Error::IncompatibleVersion {
                ours: PROTOCOL_VERSION,
                theirs: theirs.protocol_version,
            });
        }
        // The decider announces the connection; the other side waits for it.
        let ready = if we_decide {
            None
        } else {
            Some(read_handshake_frame::<Ready>(&mut recv).await?)
        };
        Ok::<_, Error>((theirs, ready, send, recv))
    };
    let (hello, ready, mut send, recv) = match timeout(HANDSHAKE_TIMEOUT, handshake).await {
        Ok(Ok(done)) => done,
        Ok(Err(e @ Error::IncompatibleVersion { .. })) => {
            conn.close(close::INCOMPATIBLE.into(), b"incompatible version");
            return Err(e);
        }
        Ok(Err(e)) => return Err(explain_close(&conn, e)),
        Err(_) => {
            conn.close(close::NORMAL.into(), b"handshake timeout");
            return Err(Error::Timeout("handshake"));
        }
    };

    let peer = Peer {
        id: remote,
        device: hello.device.sanitized(),
        capabilities: hello.capabilities,
    };
    let session_id = shared.next_session.fetch_add(1, Ordering::Relaxed);
    // On the deciding side this is a placeholder, filled in on registration.
    let ready = ready.unwrap_or(Ready {
        instance: shared.instance,
        seq: 0,
    });
    let (outgoing, outgoing_rx) = mpsc::channel(64);
    let pending_pings = Arc::new(Mutex::new(HashMap::new()));
    let handle = PeerHandle {
        peer: peer.clone(),
        session: session_id,
        ready,
        conn: conn.clone(),
        outgoing,
        pending_pings: pending_pings.clone(),
    };

    let (registered, ready) = if we_decide {
        shared.register_as_decider(handle)
    } else {
        (shared.register_as_follower(handle), ready)
    };
    match registered {
        Registered::Rejected => {
            debug!(peer = %remote.fmt_short(), seq = ready.seq, "ignoring superseded connection");
            conn.close(close::DUPLICATE.into(), b"duplicate");
            return Err(Error::Duplicate);
        }
        Registered::New => {
            info!(peer = %remote.fmt_short(), name = %peer.device.name, "connected");
            shared.emit(NodeEvent::PeerConnected(peer.clone()));
            // Remember where it was, to reconnect next time without searching.
            let addrs = {
                let nearby = shared.nearby.lock().unwrap();
                nearby.get(&remote).map(|e| e.device.addrs.clone())
            };
            if let Some(addrs) = addrs {
                if let Err(e) = shared.trust.remember_addrs(&remote, addrs) {
                    debug!("could not save the device's address: {e}");
                }
            }
        }
        Registered::Replaced => {
            debug!(peer = %remote.fmt_short(), "connection replaced");
        }
    }
    debug!(
        peer = %remote.fmt_short(),
        session = session_id,
        seq = ready.seq,
        decider = we_decide,
        dialed = matches!(role, Role::Dialer),
        "session in use"
    );
    if we_decide {
        if let Err(e) = write_frame(&mut send, &ready, MAX_CONTROL_FRAME).await {
            conn.close(close::NORMAL.into(), b"");
            shared.unregister(remote, session_id);
            return Err(e);
        }
    }

    let session = Session {
        shared: shared.clone(),
        conn,
        peer_id: remote,
        session_id,
        send,
        recv,
        outgoing_rx,
        pending_pings,
    };
    Ok((peer, session))
}

/// Reads one message of the connection handshake.
async fn read_handshake_frame<T: serde::de::DeserializeOwned>(recv: &mut RecvStream) -> Result<T> {
    let body = read_frame(recv, MAX_CONTROL_FRAME)
        .await?
        .ok_or_else(|| Error::Protocol("connection closed during the handshake".into()))?;
    decode(&body)
}

/// The application close code the other side closed the connection with.
fn remote_close_code(conn: &Connection) -> Option<u64> {
    match conn.close_reason() {
        Some(ConnectionError::ApplicationClosed(closed)) => Some(closed.error_code.into_inner()),
        _ => None,
    }
}

/// If the other side closed the connection with one of our close codes, turn
/// the generic stream error into the specific reason.
fn explain_close(conn: &Connection, fallback: Error) -> Error {
    let Some(code) = remote_close_code(conn) else {
        return fallback;
    };
    if code == u64::from(close::NOT_TRUSTED) {
        Error::Rejected
    } else if code == u64::from(close::DUPLICATE) || code == u64::from(close::REPLACED) {
        Error::Duplicate
    } else if code == u64::from(close::INCOMPATIBLE) {
        Error::IncompatibleVersion {
            ours: PROTOCOL_VERSION,
            theirs: 0,
        }
    } else if code == u64::from(close::PAIR_WRONG_CODE) {
        Error::WrongPairingCode
    } else if code == u64::from(close::PAIR_NOT_OFFERING) {
        Error::NotInPairingMode
    } else {
        fallback
    }
}

/// The live half of a connection: pumps the control stream until it ends.
struct Session {
    shared: Arc<Shared>,
    conn: Connection,
    peer_id: DeviceId,
    session_id: u64,
    send: SendStream,
    recv: RecvStream,
    outgoing_rx: mpsc::Receiver<Control>,
    pending_pings: Arc<Mutex<HashMap<u64, oneshot::Sender<()>>>>,
}

impl Session {
    async fn run(self) {
        let Session {
            shared,
            conn,
            peer_id,
            session_id,
            mut send,
            mut recv,
            mut outgoing_rx,
            pending_pings,
        } = self;

        // Reading a frame is not cancel-safe, so it gets its own task and
        // hands complete frames over a channel.
        let (incoming_tx, mut incoming_rx) = mpsc::channel::<Vec<u8>>(16);
        let reader = tokio::spawn(async move {
            loop {
                match read_frame(&mut recv, MAX_CONTROL_FRAME).await {
                    Ok(Some(body)) => {
                        if incoming_tx.send(body).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        debug!("control stream ended: {e}");
                        break;
                    }
                }
            }
        });

        // Items such as clipboard content arrive on their own streams, one at
        // a time per device, so a single peer cannot make us hold many large
        // items in memory at once.
        let streams = {
            let (shared, conn) = (shared.clone(), conn.clone());
            tokio::spawn(async move {
                while let Ok(mut recv) = conn.accept_uni().await {
                    let handled =
                        timeout(STREAM_TIMEOUT, receive_stream(&shared, peer_id, &mut recv)).await;
                    let failure = match handled {
                        Ok(Ok(())) => continue,
                        Ok(Err(e)) => e.to_string(),
                        Err(_) => "timed out".to_owned(),
                    };
                    debug!(peer = %peer_id.fmt_short(), "dropped incoming stream: {failure}");
                    let _ = recv.stop(0u32.into());
                }
            })
        };

        // Files are offered on streams of their own, each handled separately
        // so a large file does not hold anything else up.
        let offers = {
            let (shared, conn) = (shared.clone(), conn.clone());
            tokio::spawn(async move {
                let conn_id = conn.stable_id();
                while let Ok((send, mut recv)) = conn.accept_bi().await {
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        let header = timeout(STREAM_TIMEOUT, async {
                            let body = read_frame(&mut recv, MAX_STREAM_HEADER).await.ok()??;
                            decode::<StreamHeader>(&body).ok()
                        })
                        .await;
                        match header {
                            Ok(Some(StreamHeader::File(header)))
                                if shared
                                    .capabilities
                                    .iter()
                                    .any(|c| c == transfer::CAPABILITY) =>
                            {
                                shared
                                    .transfers
                                    .receive(peer_id, conn_id, header, send, recv)
                                    .await;
                            }
                            _ => {
                                debug!(peer = %peer_id.fmt_short(), "dropped an unexpected stream")
                            }
                        }
                    });
                }
            })
        };

        loop {
            tokio::select! {
                body = incoming_rx.recv() => {
                    let Some(body) = body else { break };
                    match decode::<Control>(&body) {
                        Ok(Control::Ping { nonce }) => {
                            let pong = Control::Pong { nonce };
                            if write_frame(&mut send, &pong, MAX_CONTROL_FRAME).await.is_err() {
                                break;
                            }
                        }
                        Ok(Control::Pong { nonce }) => {
                            if let Some(tx) = pending_pings.lock().unwrap().remove(&nonce) {
                                let _ = tx.send(());
                            }
                        }
                        // A newer app may send kinds we do not know yet.
                        Err(_) => debug!("ignoring unknown control message"),
                    }
                }
                msg = outgoing_rx.recv() => {
                    let Some(msg) = msg else { break };
                    if write_frame(&mut send, &msg, MAX_CONTROL_FRAME).await.is_err() {
                        break;
                    }
                }
            }
        }

        reader.abort();
        streams.abort();
        offers.abort();
        debug!(
            peer = %peer_id.fmt_short(),
            session = session_id,
            reason = ?conn.close_reason(),
            "session ended"
        );
        let unpaired_by_peer = remote_close_code(&conn) == Some(u64::from(close::NOT_TRUSTED));
        conn.close(close::NORMAL.into(), b"");
        // Wake anyone still waiting for a pong.
        pending_pings.lock().unwrap().clear();
        shared.unregister(peer_id, session_id);
        if unpaired_by_peer {
            shared.peer_unpaired_us(peer_id);
        }
    }
}

/// Sends one clipboard item on its own stream and waits until the other
/// device has taken it.
async fn send_clipboard_item(conn: &Connection, item: &ClipboardItem) -> Result<()> {
    let mut send = conn
        .open_uni()
        .await
        .map_err(|e| Error::Connect(e.to_string()))?;
    let header = StreamHeader::Clipboard(ClipboardHeader::for_item(item));
    write_frame(&mut send, &header, MAX_STREAM_HEADER).await?;
    clipboard::write_payload(&mut send, item).await?;
    send.finish().map_err(|e| Error::Protocol(e.to_string()))?;
    match send.stopped().await {
        // `None`: everything was delivered. `Some`: the other side refused it.
        Ok(None) => Ok(()),
        Ok(Some(_)) => Err(Error::Protocol("the other device refused the item".into())),
        Err(e) => Err(Error::Connect(e.to_string())),
    }
}

/// Handles one incoming stream from a paired device.
async fn receive_stream(shared: &Shared, from: DeviceId, recv: &mut RecvStream) -> Result<()> {
    let body = read_frame(recv, MAX_STREAM_HEADER)
        .await?
        .ok_or_else(|| Error::Protocol("stream closed before its header".into()))?;
    // A stream kind from a newer version fails to decode here and is dropped.
    match decode::<StreamHeader>(&body)? {
        StreamHeader::Clipboard(header) => {
            if !shared
                .capabilities
                .iter()
                .any(|c| c == clipboard::CAPABILITY)
            {
                return Err(Error::Protocol(
                    "clipboard sync is off on this device".into(),
                ));
            }
            let item = clipboard::read_payload(recv, &header, from).await?;
            shared.emit(NodeEvent::ClipboardReceived(Arc::new(item)));
            Ok(())
        }
        // Files come on two-way streams, so the receiver can answer.
        StreamHeader::File(_) => Err(Error::Protocol(
            "file offered on the wrong kind of stream".into(),
        )),
    }
}

/// Accepts incoming connections from paired devices.
#[derive(Clone)]
struct LinkProtocol {
    shared: Arc<Shared>,
}

impl fmt::Debug for LinkProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LinkProtocol")
    }
}

impl ProtocolHandler for LinkProtocol {
    async fn accept(&self, connection: Connection) -> std::result::Result<(), AcceptError> {
        let remote = connection.remote_id();
        match open_session(&self.shared, connection, Role::Acceptor).await {
            Ok((_peer, session)) => session.run().await,
            Err(Error::NotTrusted(_)) => {
                info!(peer = %remote.fmt_short(), "refused connection from an unpaired device");
            }
            Err(e) => debug!(peer = %remote.fmt_short(), "incoming connection failed: {e}"),
        }
        Ok(())
    }
}

/// Accepts pairing attempts. Unlike [`LinkProtocol`] this talks to devices we
/// do not know, so it does nothing unless a pairing code is on offer.
#[derive(Clone)]
struct PairProtocol {
    shared: Arc<Shared>,
}

impl fmt::Debug for PairProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PairProtocol")
    }
}

impl ProtocolHandler for PairProtocol {
    async fn accept(&self, connection: Connection) -> std::result::Result<(), AcceptError> {
        let remote = connection.remote_id();
        if let Err(e) = host_pairing(&self.shared, &connection).await {
            debug!(peer = %remote.fmt_short(), "pairing attempt failed: {e}");
        }
        Ok(())
    }
}

/// Host side of one pairing attempt.
async fn host_pairing(shared: &Arc<Shared>, conn: &Connection) -> Result<()> {
    let joiner = conn.remote_id();

    let opening = timeout(PAIR_TIMEOUT, async {
        let (send, mut recv) = conn
            .accept_bi()
            .await
            .map_err(|e| Error::Connect(e.to_string()))?;
        let start = pairing::read_start(&mut recv).await?;
        Ok::<_, Error>((send, recv, start))
    })
    .await
    .map_err(|_| Error::Timeout("pairing"))?;
    let (mut send, mut recv, start) = opening?;

    let Some(offer) = shared.take_offer() else {
        conn.close(close::PAIR_NOT_OFFERING.into(), b"not in pairing mode");
        return Err(Error::NotInPairingMode);
    };

    let verified = timeout(
        PAIR_TIMEOUT,
        pairing::host_verify(
            &mut send,
            &mut recv,
            start,
            &offer.code,
            &shared.id,
            &joiner,
            &shared.device,
        ),
    )
    .await;

    let (device, accept) = match verified {
        Ok(Ok(verified)) => verified,
        Ok(Err(Error::WrongPairingCode)) => {
            // One guess was used. The code is gone for good.
            conn.close(close::PAIR_WRONG_CODE.into(), b"wrong code");
            warn!(peer = %joiner.fmt_short(), "wrong pairing code entered, code discarded");
            announce(shared);
            shared.emit(NodeEvent::PairingAttemptFailed);
            return Err(Error::WrongPairingCode);
        }
        Ok(Err(e)) => {
            // Broke off before the code was tested: nothing was learned.
            shared.restore_offer(offer);
            return Err(e);
        }
        Err(_) => {
            shared.restore_offer(offer);
            return Err(Error::Timeout("pairing"));
        }
    };

    // The joiner proved it knows the code. Trust it, then tell it so.
    let record = PairedDevice::new(joiner, device);
    if let Err(e) = shared.trust.add(record.clone()) {
        conn.close(close::NORMAL.into(), b"could not save pairing");
        announce(shared);
        return Err(e);
    }
    announce(shared);
    pairing::host_accept(&mut send, accept).await?;
    let _ = send.finish();
    // Give the joiner a moment to read the answer and hang up.
    let _ = timeout(Duration::from_secs(5), conn.closed()).await;

    info!(peer = %joiner.fmt_short(), name = %record.device.name, "paired");
    shared.refresh_nearby(joiner);
    shared.emit(NodeEvent::Paired(record));
    Ok(())
}

/// Joiner side of one pairing attempt.
async fn pair(shared: &Arc<Shared>, addr: EndpointAddr, code: String) -> Result<PairedDevice> {
    let host = addr.id;
    if host == shared.id {
        return Err(Error::Pairing("a device cannot pair with itself".into()));
    }

    let conn = timeout(
        CONNECT_TIMEOUT,
        shared.endpoint.connect(addr.clone(), PAIR_ALPN),
    )
    .await
    .map_err(|_| Error::Timeout("connect"))?
    .map_err(|e| Error::Connect(e.to_string()))?;

    let joined = timeout(PAIR_TIMEOUT, async {
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| Error::Connect(e.to_string()))?;
        pairing::join(
            &mut send,
            &mut recv,
            &code,
            &shared.id,
            &host,
            &shared.device,
        )
        .await
    })
    .await;

    let device = match joined {
        Ok(Ok(device)) => device,
        Ok(Err(e)) => {
            let e = explain_close(&conn, e);
            conn.close(close::NORMAL.into(), b"pairing failed");
            return Err(e);
        }
        Err(_) => {
            conn.close(close::NORMAL.into(), b"pairing timeout");
            return Err(Error::Timeout("pairing"));
        }
    };

    // The host proved it knows the code and has already saved us.
    let record = PairedDevice::new(host, device);
    let saved = shared.trust.add(record.clone());
    conn.close(close::NORMAL.into(), b"paired");
    saved?;

    info!(peer = %host.fmt_short(), name = %record.device.name, "paired");
    shared.refresh_nearby(host);
    shared.emit(NodeEvent::Paired(record.clone()));
    dial_in_background(shared, host, addr);
    Ok(record)
}

/// Starts advertising and browsing on the local network.
fn start_discovery(shared: Arc<Shared>) -> Result<JoinHandle<()>> {
    let mdns = MdnsAddressLookup::builder()
        .service_name(MDNS_SERVICE)
        .discovery_cadence(DISCOVERY_CADENCE)
        .build(shared.id)
        .map_err(|e| Error::Bind(format!("local discovery unavailable: {e}")))?;
    shared
        .endpoint
        .address_lookup()
        .map_err(|e| Error::Bind(e.to_string()))?
        .add(mdns.clone());
    announce(&shared);

    Ok(tokio::spawn(async move {
        knock_on_absent_devices(&shared).await;
        let mut events = mdns.subscribe().await;
        let mut retry = tokio::time::interval(RECONNECT_INTERVAL);
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                event = events.next() => {
                    let Some(event) = event else { break };
                    match event {
                        DiscoveryEvent::Discovered { endpoint_info, .. } => {
                            let id = endpoint_info.endpoint_id;
                            if id == shared.id {
                                continue;
                            }
                            let (device, pairing) = endpoint_info
                                .user_data()
                                .and_then(|d| decode_announcement(d.as_ref()))
                                .unwrap_or_else(|| {
                                    let unnamed = DeviceInfo {
                                        name: sanitize_name(""),
                                        platform: Platform::Unknown,
                                    };
                                    (unnamed, false)
                                });
                            let mut device = NearbyDevice {
                                id,
                                device,
                                trusted: shared.trust.is_trusted(&id),
                                pairing,
                                addrs: endpoint_info.ip_addrs().copied().collect(),
                            };
                            let changed = {
                                let mut nearby = shared.nearby.lock().unwrap();
                                let greeted_at = nearby.get(&id).and_then(|e| e.greeted_at);
                                // What the device told us directly beats the
                                // broadcast, which is often stale about
                                // whether a code is being shown.
                                if let (Some(old), Some(_)) = (nearby.get(&id), greeted_at) {
                                    device.device = old.device.device.clone();
                                    device.pairing = old.device.pairing;
                                }
                                let entry = NearbyEntry {
                                    device: device.clone(),
                                    addr: endpoint_info.to_endpoint_addr(),
                                    via_mdns: true,
                                    greeted_at,
                                };
                                match nearby.insert(id, entry) {
                                    Some(old) => old.device != device,
                                    None => true,
                                }
                            };
                            if changed {
                                shared.emit(NodeEvent::NearbyFound(device));
                            }
                            maybe_dial(&shared, id);
                            maybe_greet(&shared, id);
                        }
                        DiscoveryEvent::Expired { endpoint_id } => {
                            // Keep the device if it is still greeting us.
                            let removed = {
                                let mut nearby = shared.nearby.lock().unwrap();
                                let fresh = nearby.get_mut(&endpoint_id).is_some_and(|entry| {
                                    entry.via_mdns = false;
                                    entry.greeted_at.is_some_and(|t| t.elapsed() < GREETING_TTL)
                                });
                                !fresh && nearby.remove(&endpoint_id).is_some()
                            };
                            shared.greetings.lock().unwrap().remove(&endpoint_id);
                            if removed {
                                shared.emit(NodeEvent::NearbyLost(endpoint_id));
                            }
                        }
                        _ => {}
                    }
                }
                _ = retry.tick() => {
                    let ids: Vec<DeviceId> =
                        shared.nearby.lock().unwrap().keys().copied().collect();
                    for id in ids {
                        maybe_dial(&shared, id);
                        maybe_greet(&shared, id);
                    }
                    forget_silent_greeters(&shared);
                    knock_on_absent_devices(&shared).await;
                }
            }
        }
    }))
}

/// Our own beacon, as bytes.
fn our_beacon(shared: &Shared, is_reply: bool) -> Vec<u8> {
    let port = shared
        .endpoint
        .bound_sockets()
        .into_iter()
        .find(SocketAddr::is_ipv4)
        .map_or(0, |a| a.port());
    Beacon::new(
        *shared.id.as_bytes(),
        port,
        encode_announcement(&shared.device, shared.is_pairing()),
        is_reply,
    )
    .encode()
}

async fn send_beacon(shared: &Shared, to: SocketAddr, is_reply: bool) -> Result<()> {
    let Some(socket) = &shared.beacon else {
        return Err(Error::Connect("beacons are off".into()));
    };
    socket.send_to(&our_beacon(shared, is_reply), to).await?;
    Ok(())
}

/// Announces us to the whole local network: a quick burst at first and
/// whenever asked, then a slow routine.
async fn send_beacons(shared: Arc<Shared>) {
    loop {
        for wait in BEACON_BURST {
            tokio::time::sleep(wait).await;
            broadcast_beacon(&shared).await;
        }
        loop {
            tokio::select! {
                _ = tokio::time::sleep(BEACON_IDLE_INTERVAL) => broadcast_beacon(&shared).await,
                // Something changed or someone is looking: burst again.
                _ = shared.beacon_boost.notified() => break,
            }
        }
    }
}

async fn broadcast_beacon(shared: &Shared) {
    let local = shared
        .endpoint
        .addr()
        .ip_addrs()
        .filter_map(|a| match a.ip() {
            std::net::IpAddr::V4(ip) => Some(ip),
            std::net::IpAddr::V6(_) => None,
        })
        .collect::<Vec<_>>();
    for target in beacon::broadcast_targets(local, BEACON_PORT) {
        if let Err(e) = send_beacon(shared, target, false).await {
            debug!("could not send a beacon to {target}: {e}");
        }
    }
}

/// Listens for other devices' beacons and answers each sender directly.
async fn receive_beacons(shared: Arc<Shared>) {
    let Some(socket) = shared.beacon.clone() else {
        return;
    };
    let mut packet = [0u8; beacon::MAX_BEACON_BYTES + 1];
    loop {
        let (len, from) = match socket.recv_from(&mut packet).await {
            Ok(received) => received,
            Err(e) => {
                debug!("beacon socket error: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let Some(heard) = Beacon::decode(&packet[..len]) else {
            continue;
        };
        let Ok(id) = DeviceId::from_bytes(&heard.id) else {
            continue;
        };
        if id == shared.id {
            continue;
        }
        let Some((device, pairing)) = decode_announcement(&heard.announcement) else {
            continue;
        };
        // The packet says which port; where it came from says which address.
        let addr = SocketAddr::new(from.ip(), heard.port);
        note_direct(&shared, id, device, pairing, vec![addr]);

        if !heard.is_reply {
            let due = {
                let mut replies = shared.beacon_replies.lock().unwrap();
                if replies.len() > 256 {
                    replies.retain(|_, at| at.elapsed() < BEACON_REPLY_GAP);
                }
                let due = replies
                    .get(&id)
                    .is_none_or(|at| at.elapsed() >= BEACON_REPLY_GAP);
                if due {
                    replies.insert(id, Instant::now());
                }
                due
            };
            if due {
                let _ = send_beacon(&shared, from, true).await;
            }
        }
    }
}

/// Records a device that told us directly (by beacon) that it is here.
fn note_direct(
    shared: &Arc<Shared>,
    id: DeviceId,
    device: DeviceInfo,
    pairing: bool,
    addrs: Vec<SocketAddr>,
) {
    let changed = {
        let mut nearby = shared.nearby.lock().unwrap();
        match nearby.get_mut(&id) {
            Some(entry) => {
                entry.greeted_at = Some(Instant::now());
                let mut changed = entry.device.device != device || entry.device.pairing != pairing;
                entry.device.device = device;
                entry.device.pairing = pairing;
                for addr in addrs {
                    if !entry.device.addrs.contains(&addr) {
                        entry.device.addrs.push(addr);
                        entry.addr = entry.addr.clone().with_ip_addr(addr);
                        changed = true;
                    }
                }
                changed.then(|| entry.device.clone())
            }
            None => {
                let direct_only = nearby.values().filter(|e| !e.via_mdns).count();
                if direct_only >= MAX_GREETED_DEVICES {
                    return;
                }
                let found = NearbyDevice {
                    id,
                    device,
                    trusted: shared.trust.is_trusted(&id),
                    pairing,
                    addrs: addrs.clone(),
                };
                let mut addr = EndpointAddr::new(id);
                for a in addrs {
                    addr = addr.with_ip_addr(a);
                }
                nearby.insert(
                    id,
                    NearbyEntry {
                        device: found.clone(),
                        addr,
                        via_mdns: false,
                        greeted_at: Some(Instant::now()),
                    },
                );
                Some(found)
            }
        }
    };
    if let Some(device) = changed {
        shared.emit(NodeEvent::NearbyFound(device));
    }
    maybe_dial(shared, id);
}

/// Sends a beacon straight to the last known address of each paired device
/// that is neither connected nor visible. If it is still there it answers,
/// and we reconnect without any searching.
async fn knock_on_absent_devices(shared: &Arc<Shared>) {
    if shared.beacon.is_none() {
        return;
    }
    for record in shared.trust.list() {
        let visible = shared.nearby.lock().unwrap().contains_key(&record.id);
        if visible || shared.peer(&record.id).is_some() {
            continue;
        }
        for addr in shared.trust.last_addrs(&record.id) {
            if addr.is_ipv4() {
                let target = SocketAddr::new(addr.ip(), BEACON_PORT);
                let _ = send_beacon(shared, target, false).await;
            }
        }
    }
}

/// Drops devices we only knew from greetings that have stopped arriving.
fn forget_silent_greeters(shared: &Shared) {
    let lost: Vec<DeviceId> = {
        let mut nearby = shared.nearby.lock().unwrap();
        let stale: Vec<DeviceId> = nearby
            .iter()
            .filter(|(_, e)| {
                !e.via_mdns && e.greeted_at.is_none_or(|t| t.elapsed() >= GREETING_TTL)
            })
            .map(|(id, _)| *id)
            .collect();
        for id in &stale {
            nearby.remove(id);
        }
        stale
    };
    for id in lost {
        shared.emit(NodeEvent::NearbyLost(id));
    }
}

/// Introduces us, in the background, to a device we hear on the network,
/// unless it recently confirmed that it sees us.
fn maybe_greet(shared: &Arc<Shared>, id: DeviceId) {
    greet_in_background(shared, id, false);
}

/// With `force`, greets even a device that sees us and one we only know from
/// its own greetings: used when what we announce has changed.
fn greet_in_background(shared: &Arc<Shared>, id: DeviceId, force: bool) {
    let addr = {
        let nearby = shared.nearby.lock().unwrap();
        match nearby.get(&id) {
            Some(entry) if entry.via_mdns || force => entry.addr.clone(),
            _ => return,
        }
    };
    {
        let mut greetings = shared.greetings.lock().unwrap();
        let state = greetings.entry(id).or_default();
        let settled = state.sees_us
            && state
                .checked_at
                .is_some_and(|t| t.elapsed() < GREETING_RECHECK);
        if state.in_flight || (settled && !force) {
            return;
        }
        state.in_flight = true;
    }
    let shared = shared.clone();
    tokio::spawn(async move {
        let outcome = send_greeting(&shared, addr).await;
        let mut greetings = shared.greetings.lock().unwrap();
        if let Some(state) = greetings.get_mut(&id) {
            state.in_flight = false;
            match outcome {
                Ok(sees_us) => {
                    state.sees_us = sees_us;
                    state.checked_at = Some(Instant::now());
                }
                Err(e) => debug!(peer = %id.fmt_short(), "greeting failed: {e}"),
            }
        }
    });
}

/// Tells every nearby device, right away, what we now announce. Called when
/// that changes, such as entering or leaving pairing mode. Network
/// announcements alone are not enough: devices that already know us often do
/// not pick up the change.
fn greet_everyone(shared: &Arc<Shared>) {
    let ids: Vec<DeviceId> = shared.nearby.lock().unwrap().keys().copied().collect();
    for id in ids {
        greet_in_background(shared, id, true);
    }
}

/// Sends one greeting. Returns whether the other device already sees us.
async fn send_greeting(shared: &Arc<Shared>, addr: EndpointAddr) -> Result<bool> {
    let exchange = async {
        let conn = shared
            .endpoint
            .connect(addr, HELLO_ALPN)
            .await
            .map_err(|e| Error::Connect(e.to_string()))?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| Error::Connect(e.to_string()))?;
        let greeting = Greeting {
            announcement: encode_announcement(&shared.device, shared.is_pairing()),
            addrs: shared.endpoint.addr().ip_addrs().copied().take(8).collect(),
        };
        write_frame(&mut send, &greeting, MAX_HELLO_FRAME).await?;
        let body = read_frame(&mut recv, MAX_HELLO_FRAME)
            .await?
            .ok_or_else(|| Error::Protocol("no reply to greeting".into()))?;
        let reply: GreetingReply = decode(&body)?;
        let remote = conn.remote_id();
        conn.close(close::NORMAL.into(), b"");
        Ok::<_, Error>((remote, reply))
    };
    let (remote, reply) = timeout(HELLO_TIMEOUT, exchange)
        .await
        .map_err(|_| Error::Timeout("greeting"))??;

    // The reply says what the other device announces right now.
    if let Some((device, pairing)) = decode_announcement(&reply.announcement) {
        let changed = {
            let mut nearby = shared.nearby.lock().unwrap();
            nearby.get_mut(&remote).and_then(|entry| {
                entry.greeted_at = Some(Instant::now());
                let changed = entry.device.device != device || entry.device.pairing != pairing;
                entry.device.device = device;
                entry.device.pairing = pairing;
                changed.then(|| entry.device.clone())
            })
        };
        if let Some(device) = changed {
            shared.emit(NodeEvent::NearbyFound(device));
        }
    }
    Ok(reply.sees_you)
}

/// Receives greetings. Like network announcements, what a greeting says
/// about the device is unverified and only used for the device list; the
/// device's id, though, is proven by the encrypted connection.
#[derive(Clone)]
struct HelloProtocol {
    shared: Arc<Shared>,
}

impl fmt::Debug for HelloProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HelloProtocol")
    }
}

impl ProtocolHandler for HelloProtocol {
    async fn accept(&self, connection: Connection) -> std::result::Result<(), AcceptError> {
        let remote = connection.remote_id();
        let handled = timeout(HELLO_TIMEOUT, receive_greeting(&self.shared, &connection)).await;
        match handled {
            Ok(Ok(())) => {
                // Let the sender read the reply and hang up.
                let _ = timeout(Duration::from_secs(2), connection.closed()).await;
            }
            Ok(Err(e)) => debug!(peer = %remote.fmt_short(), "bad greeting: {e}"),
            Err(_) => debug!(peer = %remote.fmt_short(), "greeting timed out"),
        }
        Ok(())
    }
}

async fn receive_greeting(shared: &Arc<Shared>, conn: &Connection) -> Result<()> {
    let id = conn.remote_id();
    let (mut send, mut recv) = conn
        .accept_bi()
        .await
        .map_err(|e| Error::Connect(e.to_string()))?;
    let body = read_frame(&mut recv, MAX_HELLO_FRAME)
        .await?
        .ok_or_else(|| Error::Protocol("empty greeting".into()))?;
    let greeting: Greeting = decode(&body)?;
    let (device, pairing) = decode_announcement(&greeting.announcement)
        .ok_or_else(|| Error::Protocol("unreadable greeting".into()))?;
    let addrs: Vec<SocketAddr> = greeting.addrs.into_iter().take(8).collect();

    let (sees_you, changed) = {
        let mut nearby = shared.nearby.lock().unwrap();
        match nearby.get_mut(&id) {
            // We already hear its announcements. The greeting is still the
            // fresher word on its name and whether it is showing a code.
            Some(entry) if entry.via_mdns => {
                entry.greeted_at = Some(Instant::now());
                let changed = entry.device.device != device || entry.device.pairing != pairing;
                entry.device.device = device;
                entry.device.pairing = pairing;
                (true, changed.then(|| entry.device.clone()))
            }
            existing => {
                let known = existing.is_some();
                let greeted_only = nearby.values().filter(|e| !e.via_mdns).count();
                if !known && greeted_only >= MAX_GREETED_DEVICES {
                    return Err(Error::Protocol("too many greeted devices".into()));
                }
                let device = NearbyDevice {
                    id,
                    device,
                    trusted: shared.trust.is_trusted(&id),
                    pairing,
                    addrs: addrs.clone(),
                };
                let mut addr = EndpointAddr::new(id);
                for a in &addrs {
                    addr = addr.with_ip_addr(*a);
                }
                let entry = NearbyEntry {
                    device: device.clone(),
                    addr,
                    via_mdns: false,
                    greeted_at: Some(Instant::now()),
                };
                let changed = match nearby.insert(id, entry) {
                    Some(old) => old.device != device,
                    None => true,
                };
                (false, changed.then_some(device))
            }
        }
    };

    let reply = GreetingReply {
        sees_you,
        announcement: encode_announcement(&shared.device, shared.is_pairing()),
    };
    write_frame(&mut send, &reply, MAX_HELLO_FRAME).await?;
    let _ = send.finish();
    if let Some(device) = changed {
        shared.emit(NodeEvent::NearbyFound(device));
    }
    maybe_dial(shared, id);
    Ok(())
}

/// Dials a nearby paired device in the background, if auto-connect is on.
fn maybe_dial(shared: &Arc<Shared>, id: DeviceId) {
    let addr = {
        let nearby = shared.nearby.lock().unwrap();
        nearby.get(&id).map(|e| e.addr.clone())
    };
    if let Some(addr) = addr {
        dial_in_background(shared, id, addr);
    }
}

fn dial_in_background(shared: &Arc<Shared>, id: DeviceId, addr: EndpointAddr) {
    if !shared.auto_connect || !shared.trust.is_trusted(&id) || shared.peer(&id).is_some() {
        return;
    }
    if !shared.dialing.lock().unwrap().insert(id) {
        return;
    }
    let shared = shared.clone();
    tokio::spawn(async move {
        if let Err(e) = connect(&shared, addr).await {
            debug!(peer = %id.fmt_short(), "auto-connect failed: {e}");
        }
        shared.dialing.lock().unwrap().remove(&id);
    });
}

/// Publishes our name, platform and pairing-mode flag on the local network.
fn announce(shared: &Arc<Shared>) {
    let text = encode_announcement(&shared.device, shared.is_pairing());
    match UserData::try_from(text) {
        Ok(data) => shared.endpoint.set_user_data_for_address_lookup(Some(data)),
        Err(e) => warn!("could not announce device details: {e}"),
    }
    greet_everyone(shared);
    // And tell the network at once, so the change is seen without delay.
    shared.beacon_boost.notify_one();
}

/// The local-network announcement: `1|platform|flags|name`, where flags is
/// `p` while a pairing code is on offer and `-` otherwise.
fn encode_announcement(device: &DeviceInfo, pairing: bool) -> String {
    let flags = if pairing { "p" } else { "-" };
    format!("1|{}|{}|{}", device.platform.as_str(), flags, device.name)
}

fn decode_announcement(data: &str) -> Option<(DeviceInfo, bool)> {
    let mut parts = data.splitn(4, '|');
    if parts.next()? != "1" {
        return None;
    }
    let platform = Platform::parse(parts.next()?);
    let pairing = parts.next()?.contains('p');
    let name = sanitize_name(parts.next()?);
    Some((DeviceInfo { name, platform }, pairing))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announcement_round_trips() {
        let device = DeviceInfo {
            name: "Sam's Mac | work".into(),
            platform: Platform::MacOs,
        };
        for pairing in [false, true] {
            let encoded = encode_announcement(&device, pairing);
            assert!(encoded.len() <= UserData::MAX_LENGTH);
            assert_eq!(
                decode_announcement(&encoded),
                Some((device.clone(), pairing))
            );
        }
    }

    #[test]
    fn foreign_announcements_are_ignored_or_cleaned() {
        assert_eq!(decode_announcement("2|macos|-|x"), None);
        assert_eq!(decode_announcement("garbage"), None);
        let (cleaned, pairing) = decode_announcement("1|plan9|-|bad\u{0007}name").unwrap();
        assert_eq!(cleaned.platform, Platform::Unknown);
        assert_eq!(cleaned.name, "badname");
        assert!(!pairing);
    }
}
