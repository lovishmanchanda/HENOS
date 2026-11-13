//! End-to-end file transfer tests: real nodes over loopback.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use henos_core::{
    transfer::CAPABILITY, DeviceId, DeviceInfo, Error, Identity, Node, NodeConfig, NodeEvent,
    Platform, TransferEvent, TrustStore,
};
use tokio::{sync::broadcast::Receiver, time::timeout};

const WAIT: Duration = Duration::from_secs(20);

async fn node(name: &str, identity: Identity, trusted: &[DeviceId], files: bool) -> Node {
    let device = DeviceInfo {
        name: name.to_owned(),
        platform: Platform::current(),
    };
    let trust = Arc::new(TrustStore::in_memory_with(trusted.iter().copied()));
    let mut config = NodeConfig::new(identity, device, trust);
    config.discovery = false;
    config.auto_connect = false;
    if files {
        config.capabilities = vec![CAPABILITY.to_owned()];
    }
    Node::spawn(config).await.expect("node starts")
}

/// Two connected nodes that trust each other.
async fn pair(receiver_takes_files: bool) -> (Node, Node) {
    let (ia, ib) = (Identity::generate(), Identity::generate());
    let (ida, idb) = (ia.id(), ib.id());
    let a = node("Sender", ia, &[idb], true).await;
    let b = node("Receiver", ib, &[ida], receiver_takes_files).await;
    let addrs: Vec<SocketAddr> = b
        .local_addrs()
        .into_iter()
        .filter(SocketAddr::is_ipv4)
        .map(|x| SocketAddr::from((Ipv4Addr::LOCALHOST, x.port())))
        .collect();
    a.connect_direct(idb, &addrs).await.unwrap();
    timeout(WAIT, async {
        while b.peers().is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (a, b)
}

/// Waits for the next transfer event matching `pick`.
async fn next<T>(
    events: &mut Receiver<NodeEvent>,
    pick: impl Fn(&TransferEvent) -> Option<T>,
) -> T {
    timeout(WAIT, async {
        loop {
            if let NodeEvent::Transfer(event) = events.recv().await.expect("events open") {
                if let Some(found) = pick(&event) {
                    return found;
                }
            }
        }
    })
    .await
    .expect("expected transfer event arrives in time")
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

#[tokio::test(flavor = "multi_thread")]
async fn accepted_file_arrives_intact() {
    let (a, b) = pair(true).await;
    let (mut a_events, mut b_events) = (a.subscribe(), b.subscribe());
    let (outbox, inbox) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());

    // 3 MB of varied bytes, so chunking and ordering matter.
    let content: Vec<u8> = (0..3_000_000u32)
        .map(|i| (i.wrapping_mul(31) >> 3) as u8)
        .collect();
    let source = write(outbox.path(), "holiday photo.jpg", &content);

    let sent = a.send_file(b.id(), &source).await.unwrap();
    let (offer, name, size) = next(&mut b_events, |e| match e {
        TransferEvent::Offered { id, name, size, .. } => Some((*id, name.clone(), *size)),
        _ => None,
    })
    .await;
    assert_eq!(name, "holiday photo.jpg");
    assert_eq!(size, content.len() as u64);
    // Nothing is written before the person says yes.
    assert_eq!(std::fs::read_dir(inbox.path()).unwrap().count(), 0);

    assert!(b.accept_file(offer, inbox.path()));
    let saved = next(&mut b_events, |e| match e {
        TransferEvent::Finished { id, path } if *id == offer => path.clone(),
        TransferEvent::Failed { reason, .. } => panic!("receive failed: {reason}"),
        _ => None,
    })
    .await;
    assert_eq!(saved, inbox.path().join("holiday photo.jpg"));
    assert_eq!(std::fs::read(&saved).unwrap(), content);
    // No temporary file is left behind.
    assert_eq!(std::fs::read_dir(inbox.path()).unwrap().count(), 1);

    // The sender hears that it arrived.
    next(&mut a_events, |e| match e {
        TransferEvent::Finished { id, .. } if *id == sent => Some(()),
        TransferEvent::Failed { reason, .. } => panic!("send failed: {reason}"),
        _ => None,
    })
    .await;

    // Sending the same file again does not overwrite the first copy.
    a.send_file(b.id(), &source).await.unwrap();
    let again = next(&mut b_events, |e| match e {
        TransferEvent::Offered { id, .. } => Some(*id),
        _ => None,
    })
    .await;
    b.accept_file(again, inbox.path());
    let second = next(&mut b_events, |e| match e {
        TransferEvent::Finished { id, path } if *id == again => path.clone(),
        _ => None,
    })
    .await;
    assert_eq!(second, inbox.path().join("holiday photo (1).jpg"));

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn declined_file_is_never_written() {
    let (a, b) = pair(true).await;
    let (mut a_events, mut b_events) = (a.subscribe(), b.subscribe());
    let (outbox, inbox) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let source = write(outbox.path(), "notes.txt", b"private notes");

    let sent = a.send_file(b.id(), &source).await.unwrap();
    let offer = next(&mut b_events, |e| match e {
        TransferEvent::Offered { id, .. } => Some(*id),
        _ => None,
    })
    .await;
    assert!(b.decline_file(offer));
    // Answering twice does nothing.
    assert!(!b.accept_file(offer, inbox.path()));

    let reason = next(&mut a_events, |e| match e {
        TransferEvent::Failed { id, reason } if *id == sent => Some(reason.clone()),
        TransferEvent::Finished { .. } => panic!("a declined file was sent"),
        _ => None,
    })
    .await;
    assert!(reason.contains("declined"), "got {reason}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(std::fs::read_dir(inbox.path()).unwrap().count(), 0);

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hostile_file_names_stay_inside_the_chosen_folder() {
    let (a, b) = pair(true).await;
    let mut b_events = b.subscribe();
    let (outbox, inbox) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // The sender's own file name is harmless here; what matters is that the
    // receiver cleans whatever name it is told. A leading dot is the part
    // of that which an ordinary file name can carry.
    let source = write(outbox.path(), ".hidden-config", b"x");

    a.send_file(b.id(), &source).await.unwrap();
    let (offer, name) = next(&mut b_events, |e| match e {
        TransferEvent::Offered { id, name, .. } => Some((*id, name.clone())),
        _ => None,
    })
    .await;
    assert_eq!(name, "hidden-config");
    b.accept_file(offer, inbox.path());
    let saved = next(&mut b_events, |e| match e {
        TransferEvent::Finished { path, .. } => path.clone(),
        _ => None,
    })
    .await;
    assert_eq!(saved.parent().unwrap(), inbox.path());

    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn files_only_go_to_connected_devices_that_take_them() {
    let (a, b) = pair(false).await;
    let outbox = tempfile::tempdir().unwrap();
    let source = write(outbox.path(), "a.txt", b"x");

    // The receiver does not take files.
    let err = a.send_file(b.id(), &source).await.unwrap_err();
    assert!(matches!(err, Error::Protocol(_)), "got {err:?}");
    // A device we are not connected to.
    let stranger = Identity::generate().id();
    assert!(matches!(
        a.send_file(stranger, &source).await,
        Err(Error::NotConnected(_))
    ));
    // A folder is not a file.
    let (c, d) = pair(true).await;
    assert!(c.send_file(d.id(), outbox.path()).await.is_err());
    // A file that does not exist.
    assert!(c
        .send_file(d.id(), outbox.path().join("missing"))
        .await
        .is_err());

    for n in [a, b, c, d] {
        n.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_file_can_be_sent() {
    let (a, b) = pair(true).await;
    let mut b_events = b.subscribe();
    let (outbox, inbox) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let source = write(outbox.path(), "empty.txt", b"");

    a.send_file(b.id(), &source).await.unwrap();
    let offer = next(&mut b_events, |e| match e {
        TransferEvent::Offered { id, .. } => Some(*id),
        _ => None,
    })
    .await;
    b.accept_file(offer, inbox.path());
    let saved = next(&mut b_events, |e| match e {
        TransferEvent::Finished { path, .. } => path.clone(),
        TransferEvent::Failed { reason, .. } => panic!("failed: {reason}"),
        _ => None,
    })
    .await;
    assert_eq!(std::fs::read(saved).unwrap(), b"");

    a.shutdown().await;
    b.shutdown().await;
}
