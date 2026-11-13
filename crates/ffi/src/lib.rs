//! Bindings that let the Android app use HENOS.
//!
//! Kotlin sees one object, [`Henos`], with plain blocking methods. The screen
//! state crosses as JSON (the same snapshot the desktop window reads), so the
//! two apps cannot drift apart in what they show.
//!
//! Kotlin supplies two callbacks: where to put received clipboard text, and a
//! nudge whenever the state changed.

use std::sync::Arc;

use henos_app::{App, AppConfig};
use henos_core::{ClipboardBackend, ClipboardContent};
use tokio::runtime::Runtime;

uniffi::setup_scaffolding!();

/// Anything that went wrong, as a sentence that can be shown to the person.
///
/// "Flat": Kotlin gets an exception whose message is the sentence itself.
#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum HenosError {
    #[error("{0}")]
    Failed(String),
}

impl From<String> for HenosError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

/// Implemented in Kotlin: puts received text on the phone's clipboard.
#[uniffi::export(with_foreign)]
pub trait ClipboardSink: Send + Sync {
    fn write_text(&self, text: String);
}

/// Implemented in Kotlin: called whenever [`Henos::state_json`] would return
/// something new. May be called from any thread.
#[uniffi::export(with_foreign)]
pub trait ChangeListener: Send + Sync {
    fn on_change(&self);
}

struct SinkBackend(Arc<dyn ClipboardSink>);

impl ClipboardBackend for SinkBackend {
    fn write(&self, content: &ClipboardContent) -> Result<(), String> {
        match content {
            ClipboardContent::Text(text) => {
                self.0.write_text(text.clone());
                Ok(())
            }
            _ => Err("this kind of clipboard content is not supported yet".into()),
        }
    }
}

/// A running HENOS instance.
#[derive(uniffi::Object)]
pub struct Henos {
    app: Arc<App>,
    runtime: Runtime,
}

#[uniffi::export]
impl Henos {
    /// Starts HENOS. `data_dir` is the app's private storage folder.
    #[uniffi::constructor]
    pub fn start(
        data_dir: String,
        device_name: String,
        clipboard: Arc<dyn ClipboardSink>,
        listener: Arc<dyn ChangeListener>,
    ) -> Result<Arc<Self>, HenosError> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("henos")
            .build()
            .map_err(|e| format!("Could not start HENOS: {e}"))?;

        let mut config = AppConfig::new(data_dir);
        config.device_name = Some(device_name);
        let app = runtime.block_on(App::start(config, Arc::new(SinkBackend(clipboard))))?;

        let mut changes = app.subscribe_changes();
        runtime.spawn(async move {
            use tokio::sync::broadcast::error::RecvError;
            while let Ok(()) | Err(RecvError::Lagged(_)) = changes.recv().await {
                listener.on_change();
            }
        });

        Ok(Arc::new(Self { app, runtime }))
    }

    /// Everything the screen shows, as JSON. Same shape as the desktop app.
    pub fn state_json(&self) -> String {
        serde_json::to_string(&self.app.state()).unwrap_or_else(|_| "{}".into())
    }

    /// Enters pairing mode. The code and QR appear in the state.
    pub fn start_pairing(&self) -> Result<(), HenosError> {
        let _context = self.runtime.enter();
        self.app.start_pairing()?;
        Ok(())
    }

    pub fn stop_pairing(&self) {
        let _context = self.runtime.enter();
        self.app.stop_pairing();
    }

    /// Pairs with a nearby device showing `code`. Blocks until done.
    pub fn pair_with_code(&self, device_id: String, code: String) -> Result<(), HenosError> {
        self.runtime
            .block_on(self.app.pair_with_code(&device_id, &code))?;
        Ok(())
    }

    /// Pairs using the text of a scanned QR code. Blocks until done.
    pub fn pair_with_ticket(&self, ticket: String) -> Result<(), HenosError> {
        self.runtime.block_on(self.app.pair_with_ticket(&ticket))?;
        Ok(())
    }

    pub fn unpair(&self, device_id: String) -> Result<(), HenosError> {
        let _context = self.runtime.enter();
        self.app.unpair(&device_id)?;
        Ok(())
    }

    /// Looks for devices right away, for when the app comes on screen.
    pub fn look_now(&self) {
        self.app.look_now();
    }

    pub fn set_clipboard_paused(&self, paused: bool) {
        self.app.set_clipboard_paused(paused);
    }

    /// Sends text the person copied on the phone to the paired devices.
    /// Blocks until it has been handed over.
    pub fn copy_text(&self, text: String) {
        self.runtime
            .block_on(self.app.local_copy(ClipboardContent::Text(text)));
    }

    /// Stops networking. The object must not be used afterwards.
    pub fn shutdown(&self) {
        self.runtime.block_on(self.app.shutdown());
    }
}
