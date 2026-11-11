//! End-to-end tests of the app layer: two apps on one machine, as a UI would
//! drive them, with an in-memory stand-in for the system clipboard.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use henos_app::{ActivityKind, App, AppConfig, AppState};
use henos_core::{ClipboardBackend, ClipboardContent, PairingTicket};
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(10);

#[derive(Default)]
struct FakeClipboard {
    writes: Mutex<Vec<String>>,
}

impl FakeClipboard {
    fn current(&self) -> Option<String> {
        self.writes.lock().unwrap().last().cloned()
    }
}

impl ClipboardBackend for FakeClipboard {
    fn write(&self, content: &ClipboardContent) -> Result<(), String> {
        if let ClipboardContent::Text(text) = content {
            self.writes.lock().unwrap().push(text.clone());
        }
        Ok(())
    }
}

async fn start(dir: &Path, name: &str) -> (Arc<App>, Arc<FakeClipboard>) {
    let clipboard = Arc::new(FakeClipboard::default());
    let mut config = AppConfig::new(dir);
    config.device_name = Some(name.to_owned());
    config.discovery = false;
    let app = App::start(config, clipboard.clone())
        .await
        .expect("app starts");
    (app, clipboard)
}

/// The ticket `host` is showing, pointed at loopback so the test does not
/// depend on the machine's network.
fn loopback_ticket(host: &App, shown: &str) -> String {
    let mut ticket: PairingTicket = shown.parse().unwrap();
    ticket.addrs = host
        .local_addrs()
        .into_iter()
        .filter(SocketAddr::is_ipv4)
        .map(|a| SocketAddr::from((Ipv4Addr::LOCALHOST, a.port())))
        .collect();
    ticket.to_string()
}

async fn until(app: &App, what: &str, done: impl Fn(&AppState) -> bool) -> AppState {
    let reached = timeout(WAIT, async {
        loop {
            let state = app.state();
            if done(&state) {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    reached.unwrap_or_else(|_| panic!("timed out waiting for: {what}\n{:#?}", app.state()))
}

async fn copy(app: &App, text: &str) {
    app.local_copy(ClipboardContent::Text(text.to_owned()))
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pair_share_pause_and_unpair_as_a_user_would() {
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (desk, desk_clip) = start(dir_a.path(), "Desk").await;
    let (laptop, laptop_clip) = start(dir_b.path(), "Laptop").await;

    // A fresh install: nothing paired, nothing shown.
    let state = desk.state();
    assert_eq!(state.device.name, "Desk");
    assert!(state.paired.is_empty() && state.pairing.is_none() && state.activity.is_empty());

    // Desk shows a code. The snapshot carries the code, ticket and QR.
    let mut desk_changes = desk.subscribe_changes();
    let shown = desk.start_pairing().unwrap();
    assert_eq!(shown.code.len(), 6);
    assert!(shown.qr_svg.contains("<svg"));
    timeout(WAIT, desk_changes.recv()).await.unwrap().unwrap();
    let pairing = desk.state().pairing.expect("pairing view present");
    assert_eq!(pairing.code, shown.code);
    assert!(pairing.seconds_left > 100);

    // Laptop pairs with it.
    let paired = laptop
        .pair_with_ticket(&loopback_ticket(&desk, &shown.ticket))
        .await
        .unwrap();
    assert_eq!(paired.name, "Desk");

    // Both sides list each other, connect on their own, and stop showing
    // the code.
    let desk_state = until(&desk, "desk connected", |s| {
        s.paired.len() == 1 && s.paired[0].connected
    })
    .await;
    assert_eq!(desk_state.paired[0].name, "Laptop");
    assert!(desk_state.pairing.is_none());
    assert_eq!(desk_state.activity[0].kind, ActivityKind::Paired);
    assert_eq!(desk_state.activity[0].text, "Paired with Laptop");
    until(&laptop, "laptop connected", |s| {
        s.paired.len() == 1 && s.paired[0].connected
    })
    .await;

    // Copy on the laptop, paste on the desk.
    copy(&laptop, "meeting at 3").await;
    let desk_state = until(&desk, "desk received the copy", |s| {
        s.activity
            .first()
            .is_some_and(|a| a.kind == ActivityKind::ClipboardReceived)
    })
    .await;
    assert_eq!(desk_clip.current().as_deref(), Some("meeting at 3"));
    // The activity names the device and the size, never the text.
    let entry = &desk_state.activity[0].text;
    assert!(entry.contains("Laptop") && entry.contains("12 bytes"));
    assert!(!entry.contains("meeting"));
    let laptop_state = laptop.state();
    assert_eq!(laptop_state.activity[0].kind, ActivityKind::ClipboardSent);

    // Paused: nothing goes out and nothing comes in.
    desk.set_clipboard_paused(true);
    assert!(desk.state().clipboard_paused);
    copy(&desk, "typed while paused").await;
    copy(&laptop, "sent to a paused desk").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(laptop_clip.current(), None);
    assert_eq!(desk_clip.current().as_deref(), Some("meeting at 3"));

    // Resumed: sharing works again.
    desk.set_clipboard_paused(false);
    copy(&desk, "back on").await;
    until(&laptop, "laptop received after resume", |_| {
        laptop_clip.current().as_deref() == Some("back on")
    })
    .await;

    // Unpair on the desk: gone from the list at once, logged a moment later.
    desk.unpair(&laptop.state().device.id).unwrap();
    assert!(desk.state().paired.is_empty());
    let desk_state = until(&desk, "desk logged the unpairing", |s| {
        s.activity
            .first()
            .is_some_and(|a| a.kind == ActivityKind::Unpaired)
    })
    .await;
    assert_eq!(desk_state.activity[0].text, "No longer paired with Laptop");

    desk.shutdown().await;
    laptop.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn identity_and_pairings_survive_a_restart() {
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (desk, _) = start(dir_a.path(), "Desk").await;
    let (laptop, _) = start(dir_b.path(), "Laptop").await;
    let desk_id = desk.state().device.id;

    let shown = desk.start_pairing().unwrap();
    laptop
        .pair_with_ticket(&loopback_ticket(&desk, &shown.ticket))
        .await
        .unwrap();
    until(&desk, "desk paired", |s| s.paired.len() == 1).await;
    desk.shutdown().await;
    laptop.shutdown().await;

    // Same folders, new processes.
    let (desk, _) = start(dir_a.path(), "Desk").await;
    let state = desk.state();
    assert_eq!(state.device.id, desk_id);
    assert_eq!(state.paired.len(), 1);
    assert_eq!(state.paired[0].name, "Laptop");
    assert!(!state.paired[0].connected);
    desk.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn mistakes_get_plain_explanations() {
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (desk, _) = start(dir_a.path(), "Desk").await;
    let (laptop, _) = start(dir_b.path(), "Laptop").await;

    // Not a ticket at all.
    let err = laptop.pair_with_ticket("hello").await.unwrap_err();
    assert_eq!(err, "That is not a valid HENOS pairing code.");
    // Not a device id.
    assert_eq!(
        laptop.unpair("nope").unwrap_err(),
        "That is not a valid device id."
    );
    // Not six digits.
    let err = laptop
        .pair_with_code(&desk.state().device.id, "12")
        .await
        .unwrap_err();
    assert_eq!(err, "A pairing code is 6 digits.");

    // Wrong code: explained on the joiner, logged on the host, code gone.
    let shown = desk.start_pairing().unwrap();
    let wrong = if shown.code == "000000" {
        "000001"
    } else {
        "000000"
    };
    let bad_ticket = loopback_ticket(&desk, &shown.ticket).replace(&shown.code, wrong);
    let err = laptop.pair_with_ticket(&bad_ticket).await.unwrap_err();
    assert!(err.starts_with("That code is not right."), "got {err}");
    let state = until(&desk, "desk logged the failed attempt", |s| {
        s.activity
            .first()
            .is_some_and(|a| a.kind == ActivityKind::PairingFailed)
    })
    .await;
    assert!(state.pairing.is_none());
    assert!(state.paired.is_empty());

    // Code no longer on offer.
    let err = laptop
        .pair_with_ticket(&loopback_ticket(&desk, &shown.ticket))
        .await
        .unwrap_err();
    assert!(
        err.starts_with("The other device is not showing"),
        "got {err}"
    );

    // Stopping pairing mode clears the view.
    desk.start_pairing().unwrap();
    assert!(desk.state().pairing.is_some());
    desk.stop_pairing();
    assert!(desk.state().pairing.is_none());

    desk.shutdown().await;
    laptop.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn state_serialises_for_the_interface() {
    let dir = tempfile::tempdir().unwrap();
    let (desk, _) = start(dir.path(), "Desk").await;
    desk.start_pairing().unwrap();
    let json = serde_json::to_value(desk.state()).unwrap();
    // The interface reads these exact names.
    assert_eq!(json["device"]["name"], "Desk");
    assert!(json["pairing"]["qrSvg"].as_str().unwrap().contains("<svg"));
    assert!(json["pairing"]["secondsLeft"].as_u64().unwrap() > 0);
    assert_eq!(json["clipboardPaused"], false);
    assert!(json["paired"].as_array().unwrap().is_empty());
    assert!(json["nearby"].as_array().unwrap().is_empty());
    desk.shutdown().await;
}
