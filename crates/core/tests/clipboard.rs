//! End-to-end clipboard sync tests: real nodes over loopback, with an
//! in-memory stand-in for the system clipboard.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use henos_core::{
    clipboard::{CAPABILITY, MAX_TEXT_BYTES},
    ClipboardBackend, ClipboardContent, ClipboardEvent, ClipboardSync, DeviceId, DeviceInfo, Error,
    Identity, LocalCopy, Node, NodeConfig, Platform, TrustStore,
};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

/// Stand-in for a system clipboard. Remembers every write.
#[derive(Default)]
struct FakeClipboard {
    writes: Mutex<Vec<String>>,
    /// What the clipboard holds: the latest of a local copy or a write.
    held: Mutex<Option<String>>,
}

impl FakeClipboard {
    fn current(&self) -> Option<String> {
        self.writes.lock().unwrap().last().cloned()
    }

    fn write_count(&self) -> usize {
        self.writes.lock().unwrap().len()
    }
}

impl ClipboardBackend for FakeClipboard {
    fn write(&self, content: &ClipboardContent) -> Result<(), String> {
        match content {
            ClipboardContent::Text(text) => {
                self.writes.lock().unwrap().push(text.clone());
                *self.held.lock().unwrap() = Some(text.clone());
            }
            _ => return Err("unsupported content".into()),
        }
        Ok(())
    }
}

/// One simulated device: node, clipboard and the sync between them.
struct Device {
    node: Arc<Node>,
    clipboard: Arc<FakeClipboard>,
    sync: Arc<ClipboardSync>,
}

impl Device {
    async fn copy(&self, text: &str) -> LocalCopy {
        // As on a real device: the text is on the clipboard first, then the
        // platform reports it.
        if !text.is_empty() {
            *self.clipboard.held.lock().unwrap() = Some(text.to_owned());
        }
        self.sync
            .local_copy(ClipboardContent::Text(text.to_owned()))
            .await
            .unwrap()
    }

    /// Waits until this device's clipboard holds `text`.
    async fn expect_clipboard(&self, text: &str) {
        let reached = timeout(WAIT, async {
            while self.clipboard.current().as_deref() != Some(text) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            reached.is_ok(),
            "clipboard is {:?}, expected {text:?}",
            self.clipboard.current()
        );
    }

    /// What the platform does after our own write: reports a change.
    async fn platform_reports_change(&self) -> LocalCopy {
        let current = self.clipboard.current().expect("something was written");
        self.copy(&current).await
    }

    fn loopback(&self) -> Vec<SocketAddr> {
        self.node
            .local_addrs()
            .into_iter()
            .filter(SocketAddr::is_ipv4)
            .map(|a| SocketAddr::from((Ipv4Addr::LOCALHOST, a.port())))
            .collect()
    }

    async fn connect(&self, other: &Device) {
        self.node
            .connect_direct(other.node.id(), &other.loopback())
            .await
            .unwrap();
    }
}

async fn device(
    name: &str,
    identity: Identity,
    trusted: &[DeviceId],
    clipboard_on: bool,
) -> Device {
    let info = DeviceInfo {
        name: name.to_owned(),
        platform: Platform::current(),
    };
    let trust = Arc::new(TrustStore::in_memory_with(trusted.iter().copied()));
    let mut config = NodeConfig::new(identity, info, trust);
    config.discovery = false;
    config.auto_connect = false;
    if clipboard_on {
        config.capabilities = vec![CAPABILITY.to_owned()];
    }
    let node = Arc::new(Node::spawn(config).await.expect("node starts"));
    let clipboard = Arc::new(FakeClipboard::default());
    let sync = ClipboardSync::start(node.clone(), clipboard.clone());
    Device {
        node,
        clipboard,
        sync,
    }
}

/// `n` devices that are all paired with each other and connected.
async fn mesh(n: usize) -> Vec<Device> {
    let identities: Vec<Identity> = (0..n).map(|_| Identity::generate()).collect();
    let ids: Vec<DeviceId> = identities.iter().map(Identity::id).collect();
    let mut devices = Vec::new();
    for (i, identity) in identities.into_iter().enumerate() {
        let others: Vec<DeviceId> = ids.iter().copied().filter(|id| *id != ids[i]).collect();
        devices.push(device(&format!("Device {i}"), identity, &others, true).await);
    }
    for i in 0..n {
        for j in (i + 1)..n {
            devices[i].connect(&devices[j]).await;
        }
    }
    // Both ends of every connection are up before the test starts.
    timeout(WAIT, async {
        while devices.iter().any(|d| d.node.peers().len() != n - 1) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("mesh connects");
    devices
}

async fn shutdown(devices: Vec<Device>) {
    for d in devices {
        d.sync.stop();
        d.node.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn copy_on_one_device_arrives_on_the_other() {
    let devices = mesh(2).await;
    let (a, b) = (&devices[0], &devices[1]);
    let mut b_events = b.sync.subscribe();

    assert_eq!(a.copy("hello from A").await, LocalCopy::Sent { devices: 1 });
    b.expect_clipboard("hello from A").await;
    let event = timeout(WAIT, b_events.recv()).await.unwrap().unwrap();
    assert_eq!(
        event,
        ClipboardEvent::Received {
            from: a.node.id(),
            bytes: "hello from A".len(),
            image: false
        }
    );

    // And back the other way, with text that is not plain ASCII.
    assert_eq!(
        b.copy("répondre ✓ 回答").await,
        LocalCopy::Sent { devices: 1 }
    );
    a.expect_clipboard("répondre ✓ 回答").await;

    shutdown(devices).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn received_item_is_not_sent_back() {
    let devices = mesh(2).await;
    let (a, b) = (&devices[0], &devices[1]);

    a.copy("ping").await;
    b.expect_clipboard("ping").await;

    // B's platform now reports the clipboard changed (because we wrote it).
    // That must be recognised and not bounce back to A.
    assert_eq!(b.platform_reports_change().await, LocalCopy::Echo);
    // Copying the very same thing again on A is not news either.
    assert_eq!(a.copy("ping").await, LocalCopy::Echo);

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(a.clipboard.write_count(), 0, "nothing came back to A");
    assert_eq!(b.clipboard.write_count(), 1);

    shutdown(devices).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn three_devices_each_get_one_copy_and_no_storm() {
    if std::env::var_os("HENOS_TEST_LOG").is_some() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("henos_core=debug")
            .with_test_writer()
            .try_init();
    }
    let devices = mesh(3).await;

    assert_eq!(
        devices[0].copy("to everyone").await,
        LocalCopy::Sent { devices: 2 }
    );
    devices[1].expect_clipboard("to everyone").await;
    devices[2].expect_clipboard("to everyone").await;
    for d in &devices[1..] {
        assert_eq!(d.platform_reports_change().await, LocalCopy::Echo);
    }

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(devices[0].clipboard.write_count(), 0);
    assert_eq!(devices[1].clipboard.write_count(), 1);
    assert_eq!(devices[2].clipboard.write_count(), 1);

    shutdown(devices).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn simultaneous_copies_converge_on_one_item() {
    for round in 0..10 {
        let devices = mesh(2).await;
        let (a, b) = (&devices[0], &devices[1]);
        let (text_a, text_b) = (format!("from A {round}"), format!("from B {round}"));

        let (sent_a, sent_b) = tokio::join!(a.copy(&text_a), b.copy(&text_b));
        assert_eq!(sent_a, LocalCopy::Sent { devices: 1 });
        assert_eq!(sent_b, LocalCopy::Sent { devices: 1 });
        // However the two copies interleave, both devices end up holding
        // the same text; they never swap.
        let held = |d: &Device| d.clipboard.held.lock().unwrap().clone();
        let agreed = timeout(WAIT, async {
            while held(a) != held(b) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            agreed.is_ok(),
            "devices disagree in round {round}: {:?} vs {:?}",
            held(a),
            held(b)
        );
        // And it stays that way.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(held(a), held(b), "devices drifted apart in round {round}");

        shutdown(devices).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn device_with_clipboard_sync_off_neither_sends_nor_receives() {
    let (ia, ib) = (Identity::generate(), Identity::generate());
    let (ida, idb) = (ia.id(), ib.id());
    let a = device("On", ia, &[idb], true).await;
    let b = device("Off", ib, &[ida], false).await;
    a.connect(&b).await;
    timeout(WAIT, async {
        while b.node.peers().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();

    // A sees that B does not take part, so it sends nothing.
    assert_eq!(a.copy("secret").await, LocalCopy::Sent { devices: 0 });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(b.clipboard.write_count(), 0);

    shutdown(vec![a, b]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_sent_without_a_connection() {
    let devices = mesh(2).await;
    let lonely = device("Lonely", Identity::generate(), &[], true).await;

    assert_eq!(
        lonely.copy("to nobody").await,
        LocalCopy::Sent { devices: 0 }
    );
    devices[0].copy("between the pair").await;
    devices[1].expect_clipboard("between the pair").await;
    assert_eq!(lonely.clipboard.write_count(), 0);

    lonely.sync.stop();
    lonely.node.shutdown().await;
    shutdown(devices).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn size_limits_are_enforced() {
    let devices = mesh(2).await;
    let (a, b) = (&devices[0], &devices[1]);

    // Too large: refused locally, nothing sent.
    let too_big = "x".repeat(MAX_TEXT_BYTES + 1);
    let err = a
        .sync
        .local_copy(ClipboardContent::Text(too_big))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::FrameTooLarge { .. }));

    // Empty: ignored.
    assert_eq!(a.copy("").await, LocalCopy::Echo);

    // Exactly at the limit: goes through intact.
    let largest = "y".repeat(MAX_TEXT_BYTES);
    assert_eq!(a.copy(&largest).await, LocalCopy::Sent { devices: 1 });
    b.expect_clipboard(&largest).await;
    assert_eq!(b.clipboard.write_count(), 1);

    shutdown(devices).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn newer_copy_replaces_older_one_in_order() {
    let devices = mesh(2).await;
    let (a, b) = (&devices[0], &devices[1]);

    for i in 0..20 {
        a.copy(&format!("item {i}")).await;
    }
    b.expect_clipboard("item 19").await;
    // Then B copies; A must take it even though A's own items are recent.
    b.copy("from B").await;
    a.expect_clipboard("from B").await;

    shutdown(devices).await;
}
