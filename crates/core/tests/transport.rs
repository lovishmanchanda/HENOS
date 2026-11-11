//! End-to-end tests of the encrypted transport: real nodes talking over
//! loopback, with discovery off so the tests do not depend on the network.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use henos_core::{
    DeviceId, DeviceInfo, Error, Identity, Node, NodeConfig, NodeEvent, Platform, TrustStore,
};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

async fn node(name: &str, identity: Identity, trusted: &[DeviceId]) -> Node {
    let device = DeviceInfo {
        name: name.to_owned(),
        platform: Platform::current(),
    };
    let trust = Arc::new(TrustStore::in_memory_with(trusted.iter().copied()));
    let mut config = NodeConfig::new(identity, device, trust);
    config.capabilities = vec!["clipboard".to_owned()];
    config.discovery = false;
    config.auto_connect = false;
    Node::spawn(config).await.expect("node starts")
}

/// Two nodes that trust each other.
async fn pair() -> (Node, Node) {
    let (ia, ib) = (Identity::generate(), Identity::generate());
    let (ida, idb) = (ia.id(), ib.id());
    (
        node("Alpha", ia, &[idb]).await,
        node("Beta", ib, &[ida]).await,
    )
}

fn loopback(node: &Node) -> Vec<SocketAddr> {
    node.local_addrs()
        .into_iter()
        .filter(SocketAddr::is_ipv4)
        .map(|a| SocketAddr::from((Ipv4Addr::LOCALHOST, a.port())))
        .collect()
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

#[tokio::test(flavor = "multi_thread")]
async fn trusted_devices_connect_and_exchange_messages() {
    let (a, b) = pair().await;
    let mut b_events = b.subscribe();

    let peer = a.connect_direct(b.id(), &loopback(&b)).await.unwrap();
    assert_eq!(peer.id, b.id());
    assert_eq!(peer.device.name, "Beta");
    assert_eq!(peer.capabilities, vec!["clipboard".to_owned()]);

    wait_for(
        &mut b_events,
        |e| matches!(e, NodeEvent::PeerConnected(p) if p.id == a.id()),
    )
    .await;
    assert_eq!(b.peers()[0].device.name, "Alpha");

    // A message makes the full round trip in both directions.
    a.ping(b.id()).await.unwrap();
    b.ping(a.id()).await.unwrap();

    // Connecting again reuses the existing connection.
    a.connect_direct(b.id(), &loopback(&b)).await.unwrap();
    assert_eq!(a.peers().len(), 1);
    assert_eq!(b.peers().len(), 1);

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unpaired_device_is_refused() {
    let (ia, ib) = (Identity::generate(), Identity::generate());
    let idb = ib.id();
    // Alpha trusts Beta, but Beta has never paired with Alpha.
    let a = node("Alpha", ia, &[idb]).await;
    let b = node("Beta", ib, &[]).await;

    let err = a.connect_direct(b.id(), &loopback(&b)).await.unwrap_err();
    assert!(matches!(err, Error::Rejected), "got {err:?}");
    assert!(a.peers().is_empty());
    assert!(b.peers().is_empty());
    assert!(matches!(a.ping(b.id()).await, Err(Error::NotConnected(_))));

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn we_never_dial_an_unpaired_device() {
    let (ia, ib) = (Identity::generate(), Identity::generate());
    let ida = ia.id();
    let a = node("Alpha", ia, &[]).await;
    let b = node("Beta", ib, &[ida]).await;

    let err = a.connect_direct(b.id(), &loopback(&b)).await.unwrap_err();
    assert!(
        matches!(err, Error::NotTrusted(id) if id == b.id()),
        "got {err:?}"
    );

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_is_bound_to_the_expected_key() {
    // Dialing Beta's address while expecting a different device's key must
    // fail: an attacker who takes over an IP address cannot impersonate a
    // paired device.
    let (a, b) = pair().await;
    let imposter_target = Identity::generate().id();
    let a2 = node("Alpha2", Identity::generate(), &[imposter_target]).await;

    let result = timeout(WAIT * 2, a2.connect_direct(imposter_target, &loopback(&b))).await;
    assert!(matches!(result, Ok(Err(_))), "got {result:?}");
    assert!(b.peers().is_empty());

    a.shutdown().await;
    a2.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn simultaneous_dials_settle_on_one_connection() {
    if std::env::var_os("HENOS_TEST_LOG").is_some() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("henos_core=debug")
            .with_test_writer()
            .try_init();
    }
    for round in 0..20 {
        tracing::info!(round, "---- round");
        let (a, b) = pair().await;
        let (addr_a, addr_b) = (loopback(&a), loopback(&b));
        let (mut a_events, mut b_events) = (a.subscribe(), b.subscribe());

        let (ra, rb) = tokio::join!(
            a.connect_direct(b.id(), &addr_b),
            b.connect_direct(a.id(), &addr_a),
        );
        assert_eq!(ra.unwrap().id, b.id());
        assert_eq!(rb.unwrap().id, a.id());

        // Let any losing connection finish closing, then check both sides
        // agree and the surviving connection works both ways.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(a.peers().len(), 1);
        assert_eq!(b.peers().len(), 1);
        a.ping(b.id()).await.unwrap();
        b.ping(a.id()).await.unwrap();

        // Neither side ever saw the connection drop and come back.
        for events in [&mut a_events, &mut b_events] {
            let (mut connected, mut disconnected) = (0, 0);
            while let Ok(event) = events.try_recv() {
                match event {
                    NodeEvent::PeerConnected(_) => connected += 1,
                    NodeEvent::PeerDisconnected(_) => disconnected += 1,
                    _ => {}
                }
            }
            assert_eq!((connected, disconnected), (1, 0));
        }

        a.shutdown().await;
        b.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnect_is_noticed_and_reconnect_works() {
    let (ia, ib) = (Identity::generate(), Identity::generate());
    let (ida, idb) = (ia.id(), ib.id());
    let a = node("Alpha", ia, &[idb]).await;
    let b = node("Beta", ib.clone(), &[ida]).await;
    let mut a_events = a.subscribe();

    a.connect_direct(idb, &loopback(&b)).await.unwrap();
    b.shutdown().await;
    wait_for(
        &mut a_events,
        |e| matches!(e, NodeEvent::PeerDisconnected(id) if *id == idb),
    )
    .await;
    assert!(a.peers().is_empty());

    // Beta comes back (same identity, new port) and Alpha can reach it again.
    let b = node("Beta", ib, &[ida]).await;
    a.connect_direct(idb, &loopback(&b)).await.unwrap();
    a.ping(idb).await.unwrap();

    a.shutdown().await;
    b.shutdown().await;
}
