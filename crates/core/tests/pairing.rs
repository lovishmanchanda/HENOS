//! End-to-end pairing tests: real nodes over loopback, discovery off.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use henos_core::{
    DeviceId, DeviceInfo, Error, Identity, Node, NodeConfig, NodeEvent, PairingTicket, Platform,
    TrustStore,
};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

async fn node_with(name: &str, trust: TrustStore, code_ttl: Option<Duration>) -> Node {
    let device = DeviceInfo {
        name: name.to_owned(),
        platform: Platform::current(),
    };
    let mut config = NodeConfig::new(Identity::generate(), device, Arc::new(trust));
    config.discovery = false;
    config.auto_connect = false;
    if let Some(ttl) = code_ttl {
        config.pairing_code_ttl = ttl;
    }
    Node::spawn(config).await.expect("node starts")
}

async fn node(name: &str) -> Node {
    node_with(name, TrustStore::in_memory(), None).await
}

fn loopback(node: &Node) -> Vec<SocketAddr> {
    node.local_addrs()
        .into_iter()
        .filter(SocketAddr::is_ipv4)
        .map(|a| SocketAddr::from((Ipv4Addr::LOCALHOST, a.port())))
        .collect()
}

/// The ticket `host` is showing, pointed at loopback.
fn ticket(host: &Node, code: &str) -> PairingTicket {
    PairingTicket {
        id: host.id(),
        code: code.to_owned(),
        addrs: loopback(host),
    }
}

fn wrong_code(right: &str) -> String {
    if right == "000000" {
        "000001"
    } else {
        "000000"
    }
    .to_owned()
}

async fn wait_for<F: Fn(&NodeEvent) -> bool>(
    events: &mut tokio::sync::broadcast::Receiver<NodeEvent>,
    matches: F,
) {
    timeout(WAIT, async {
        loop {
            let event = events.recv().await.expect("event stream open");
            if matches(&event) {
                return;
            }
        }
    })
    .await
    .expect("expected event arrives in time");
}

fn ids(node: &Node) -> Vec<DeviceId> {
    node.paired_devices().iter().map(|d| d.id).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn right_code_pairs_both_devices_and_they_can_connect() {
    let host = node("Desk").await;
    let phone = node("Phone").await;
    let mut host_events = host.subscribe();

    // Before pairing, neither will talk to the other.
    assert!(matches!(
        phone.connect_direct(host.id(), &loopback(&host)).await,
        Err(Error::NotTrusted(_))
    ));

    let offer = host.start_pairing();
    assert!(host.is_pairing());
    // The ticket text is what a QR code would carry.
    let scanned: PairingTicket = offer.ticket.to_string().parse().unwrap();
    assert_eq!(scanned.id, host.id());
    assert_eq!(scanned.code, offer.code);

    let paired = phone
        .pair_with_ticket(&ticket(&host, &offer.code))
        .await
        .unwrap();
    assert_eq!(paired.id, host.id());
    assert_eq!(paired.device.name, "Desk");

    wait_for(
        &mut host_events,
        |e| matches!(e, NodeEvent::Paired(d) if d.id == phone.id() && d.device.name == "Phone"),
    )
    .await;
    assert_eq!(ids(&host), vec![phone.id()]);
    assert_eq!(ids(&phone), vec![host.id()]);
    // A code pairs one device, then pairing mode ends.
    assert!(!host.is_pairing());

    // Now the normal encrypted connection works, in either direction.
    phone
        .connect_direct(host.id(), &loopback(&host))
        .await
        .unwrap();
    phone.ping(host.id()).await.unwrap();
    host.ping(phone.id()).await.unwrap();

    host.shutdown().await;
    phone.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_code_fails_and_burns_the_code() {
    let host = node("Desk").await;
    let stranger = node("Stranger").await;
    let mut host_events = host.subscribe();

    let offer = host.start_pairing();
    let err = stranger
        .pair_with_ticket(&ticket(&host, &wrong_code(&offer.code)))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::WrongPairingCode), "got {err:?}");
    wait_for(&mut host_events, |e| {
        matches!(e, NodeEvent::PairingAttemptFailed)
    })
    .await;

    assert!(host.paired_devices().is_empty());
    assert!(stranger.paired_devices().is_empty());
    assert!(!host.is_pairing());

    // Even the right code is useless now: one guess per code.
    let err = stranger
        .pair_with_ticket(&ticket(&host, &offer.code))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotInPairingMode), "got {err:?}");
    assert!(host.paired_devices().is_empty());

    host.shutdown().await;
    stranger.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn device_not_in_pairing_mode_cannot_be_paired_with() {
    let host = node("Desk").await;
    let stranger = node("Stranger").await;

    let err = stranger
        .pair_with_ticket(&ticket(&host, "123456"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotInPairingMode), "got {err:?}");

    // Stopping pairing mode invalidates the code.
    let offer = host.start_pairing();
    host.stop_pairing();
    let err = stranger
        .pair_with_ticket(&ticket(&host, &offer.code))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotInPairingMode), "got {err:?}");
    assert!(host.paired_devices().is_empty());

    host.shutdown().await;
    stranger.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn code_expires() {
    let host = node_with(
        "Desk",
        TrustStore::in_memory(),
        Some(Duration::from_millis(200)),
    )
    .await;
    let phone = node("Phone").await;

    let offer = host.start_pairing();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!host.is_pairing());
    let err = phone
        .pair_with_ticket(&ticket(&host, &offer.code))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotInPairingMode), "got {err:?}");

    host.shutdown().await;
    phone.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_code_replaces_the_old_one() {
    let host = node("Desk").await;
    let phone = node("Phone").await;

    let first = host.start_pairing();
    let second = host.start_pairing();
    if first.code != second.code {
        let err = phone
            .pair_with_ticket(&ticket(&host, &first.code))
            .await
            .unwrap_err();
        assert!(matches!(err, Error::WrongPairingCode), "got {err:?}");
    }

    host.shutdown().await;
    phone.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_is_bound_to_the_device_in_the_ticket() {
    // The ticket names one device but points at another's address. The
    // encrypted connection is to the named key, so this cannot reach the
    // wrong device at all.
    let host = node("Desk").await;
    let other = node("Other").await;
    let phone = node("Phone").await;

    let offer = host.start_pairing();
    let forged = PairingTicket {
        id: host.id(),
        code: offer.code.clone(),
        addrs: loopback(&other),
    };
    let result = timeout(WAIT * 2, phone.pair_with_ticket(&forged)).await;
    assert!(matches!(result, Ok(Err(_))), "got {result:?}");
    assert!(phone.paired_devices().is_empty());
    assert!(other.paired_devices().is_empty());
    // The real host's code was never touched.
    assert!(host.is_pairing());

    host.shutdown().await;
    other.shutdown().await;
    phone.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unpairing_on_one_device_unpairs_both() {
    // Two devices paired long ago.
    let (ia, ib) = (Identity::generate(), Identity::generate());
    let (ida, idb) = (ia.id(), ib.id());
    let make = |name: &str, identity: Identity, other: DeviceId| {
        let device = DeviceInfo {
            name: name.to_owned(),
            platform: Platform::current(),
        };
        let trust = Arc::new(TrustStore::in_memory_with([other]));
        let mut config = NodeConfig::new(identity, device, trust);
        config.discovery = false;
        config.auto_connect = false;
        Node::spawn(config)
    };
    let a = make("Alpha", ia, idb).await.unwrap();
    let b = make("Beta", ib, ida).await.unwrap();
    let mut b_events = b.subscribe();

    b.connect_direct(ida, &loopback(&a)).await.unwrap();
    b.ping(ida).await.unwrap();

    assert!(a.unpair(idb).unwrap());
    assert!(!a.unpair(idb).unwrap());
    assert!(a.paired_devices().is_empty());

    // Beta loses the connection and forgets Alpha as well.
    wait_for(
        &mut b_events,
        |e| matches!(e, NodeEvent::Unpaired(id) if *id == ida),
    )
    .await;
    assert!(b.paired_devices().is_empty());
    assert!(b.peers().is_empty());
    assert!(matches!(
        b.connect_direct(ida, &loopback(&a)).await,
        Err(Error::NotTrusted(_))
    ));

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unpairing_while_offline_is_discovered_on_next_connect() {
    let (ia, ib) = (Identity::generate(), Identity::generate());
    let (ida, idb) = (ia.id(), ib.id());
    let device = |name: &str| DeviceInfo {
        name: name.to_owned(),
        platform: Platform::current(),
    };
    // Alpha has already forgotten Beta; Beta still thinks they are paired.
    let mut config_a = NodeConfig::new(ia, device("Alpha"), Arc::new(TrustStore::in_memory()));
    config_a.discovery = false;
    let a = Node::spawn(config_a).await.unwrap();
    let mut config_b = NodeConfig::new(
        ib,
        device("Beta"),
        Arc::new(TrustStore::in_memory_with([ida])),
    );
    config_b.discovery = false;
    config_b.auto_connect = false;
    let b = Node::spawn(config_b).await.unwrap();
    let _ = idb;

    let err = b.connect_direct(ida, &loopback(&a)).await.unwrap_err();
    assert!(matches!(err, Error::Rejected), "got {err:?}");
    assert!(b.paired_devices().is_empty());

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_with_yourself_or_a_bad_code_is_refused_locally() {
    let host = node("Desk").await;
    let offer = host.start_pairing();

    let err = host
        .pair_with_ticket(&ticket(&host, &offer.code))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Pairing(_)), "got {err:?}");

    let phone = node("Phone").await;
    let err = phone.pair_with_code(host.id(), "12ab").await.unwrap_err();
    assert!(matches!(err, Error::InvalidCode), "got {err:?}");
    // Neither attempt touched the code on offer.
    assert!(host.is_pairing());

    host.shutdown().await;
    phone.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn one_way_discovery_is_repaired_by_a_greeting() {
    // The host can see the joiner, but the joiner cannot see the host (as
    // when one computer's network announcements do not arrive).
    let host = node("Desk").await;
    let joiner = node("Laptop").await;
    assert!(joiner.nearby().is_empty());
    let mut joiner_events = joiner.subscribe();

    // The host introduces itself directly.
    let already_seen = host.greet(joiner.id(), &loopback(&joiner)).await.unwrap();
    assert!(!already_seen);
    wait_for(
        &mut joiner_events,
        |e| matches!(e, NodeEvent::NearbyFound(d) if d.id == host.id()),
    )
    .await;
    let seen = joiner.nearby();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].device.name, "Desk");
    assert!(!seen[0].trusted && !seen[0].pairing);

    // When the host starts showing a code, a new greeting carries that.
    let offer = host.start_pairing();
    host.greet(joiner.id(), &loopback(&joiner)).await.unwrap();
    wait_for(
        &mut joiner_events,
        |e| matches!(e, NodeEvent::NearbyFound(d) if d.id == host.id() && d.pairing),
    )
    .await;

    // The greeting cannot say where the host is on loopback, so point the
    // joiner there; what matters is that it now knows the host exists.
    let paired = joiner
        .pair_with_ticket(&ticket(&host, &offer.code))
        .await
        .unwrap();
    assert_eq!(paired.device.name, "Desk");

    // A greeting is not trust: it never pairs or connects anything by itself.
    let stranger = node("Stranger").await;
    stranger.greet(host.id(), &loopback(&host)).await.unwrap();
    assert!(!host.paired_devices().iter().any(|d| d.id == stranger.id()));
    assert!(host
        .nearby()
        .iter()
        .any(|d| d.id == stranger.id() && !d.trusted));

    host.shutdown().await;
    joiner.shutdown().await;
    stranger.shutdown().await;
}
