//! Sending files between paired devices.
//!
//! A file travels on its own stream of the encrypted connection:
//!
//! ```text
//! sender                                   receiver
//!   FileHeader { name, size }   ------->     (asks the person)
//!              <-------   FileAnswer { accept }
//!   the file's bytes            ------->     (written to a temporary file)
//!              <-------   FileReceipt { ok }  (renamed into place)
//! ```
//!
//! Nothing is written until the person accepts. The name is treated as
//! untrusted: it is reduced to a plain file name, so a device cannot make
//! another write outside the folder the person chose. An existing file is
//! never overwritten; the new one gets a number added.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{broadcast, oneshot, Notify},
    time::timeout,
};

use crate::{
    error::{Error, Result},
    identity::DeviceId,
    node::NodeEvent,
    protocol::{decode, read_frame, write_frame, StreamHeader, MAX_STREAM_HEADER},
};

/// Capability a device announces when it can send and receive files.
pub const CAPABILITY: &str = "files";

/// How long an offer waits for the person to answer.
const OFFER_TIMEOUT: Duration = Duration::from_secs(120);
/// How often progress is reported.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
/// Offers from one device waiting for an answer at the same time.
const MAX_PENDING_PER_DEVICE: usize = 8;
const CHUNK: usize = 64 * 1024;
const MAX_NAME_BYTES: usize = 200;

/// First frame of a file stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileHeader {
    name: String,
    size: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileAnswer {
    accept: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileReceipt {
    ok: bool,
}

/// What happens to a file transfer, for UIs to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferEvent {
    /// Another device wants to send us a file. Answer with
    /// `Node::accept_file` or `Node::decline_file`.
    Offered {
        id: u64,
        from: DeviceId,
        name: String,
        size: u64,
    },
    /// We offered a file and are waiting for the other device's answer.
    Sending {
        id: u64,
        to: DeviceId,
        name: String,
        size: u64,
    },
    /// The offer was accepted; bytes are moving.
    Accepted {
        id: u64,
    },
    Progress {
        id: u64,
        bytes: u64,
    },
    /// Done. For a received file, `path` is where it was saved.
    Finished {
        id: u64,
        path: Option<PathBuf>,
    },
    /// Declined, cancelled or broken off. `reason` is a sentence.
    Failed {
        id: u64,
        reason: String,
    },
}

enum Decision {
    Accept(PathBuf),
    Decline,
}

/// Keeps track of offers waiting for an answer and transfers in flight.
pub(crate) struct Transfers {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, (DeviceId, oneshot::Sender<Decision>)>>,
    cancels: Mutex<HashMap<u64, Arc<Notify>>>,
    /// How many transfers are using each connection.
    busy: Mutex<HashMap<usize, usize>>,
    events: broadcast::Sender<NodeEvent>,
}

/// Marks a connection as in use for as long as it lives.
struct InUse {
    transfers: Arc<Transfers>,
    conn_id: usize,
}

impl InUse {
    fn new(transfers: &Arc<Transfers>, conn_id: usize) -> Self {
        *transfers.busy.lock().unwrap().entry(conn_id).or_insert(0) += 1;
        Self {
            transfers: transfers.clone(),
            conn_id,
        }
    }
}

impl Drop for InUse {
    fn drop(&mut self) {
        let mut busy = self.transfers.busy.lock().unwrap();
        if let Some(count) = busy.get_mut(&self.conn_id) {
            *count -= 1;
            if *count == 0 {
                busy.remove(&self.conn_id);
            }
        }
    }
}

impl Transfers {
    pub(crate) fn new(events: broadcast::Sender<NodeEvent>) -> Self {
        Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::default(),
            cancels: Mutex::default(),
            busy: Mutex::default(),
            events,
        }
    }

    /// Whether a file is being offered or moved on this connection.
    pub(crate) fn is_busy(&self, conn_id: usize) -> bool {
        self.busy.lock().unwrap().contains_key(&conn_id)
    }

    fn emit(&self, event: TransferEvent) {
        let _ = self.events.send(NodeEvent::Transfer(event));
    }

    fn fail(&self, id: u64, reason: impl Into<String>) {
        self.cancels.lock().unwrap().remove(&id);
        self.emit(TransferEvent::Failed {
            id,
            reason: reason.into(),
        });
    }

    /// Offers `path` to the device on `conn`. Returns the transfer's id; the
    /// rest is reported through events.
    pub(crate) async fn send(
        self: &Arc<Self>,
        conn: Connection,
        to: DeviceId,
        path: PathBuf,
    ) -> Result<u64> {
        let metadata = tokio::fs::metadata(&path).await?;
        if !metadata.is_file() {
            return Err(Error::Protocol(
                "only files can be sent, not folders".into(),
            ));
        }
        let name = path
            .file_name()
            .map(|n| sanitize_file_name(&n.to_string_lossy()))
            .unwrap_or_else(|| "file".into());
        let size = metadata.len();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let cancel = Arc::new(Notify::new());
        self.cancels.lock().unwrap().insert(id, cancel.clone());
        self.emit(TransferEvent::Sending {
            id,
            to,
            name: name.clone(),
            size,
        });

        let this = self.clone();
        let in_use = InUse::new(self, conn.stable_id());
        tokio::spawn(async move {
            let _in_use = in_use;
            let header = FileHeader { name, size };
            let outcome = tokio::select! {
                outcome = this.send_inner(id, &conn, &path, header) => outcome,
                _ = cancel.notified() => Err("Cancelled.".to_owned()),
            };
            match outcome {
                Ok(()) => {
                    this.cancels.lock().unwrap().remove(&id);
                    this.emit(TransferEvent::Finished { id, path: None });
                }
                Err(reason) => this.fail(id, reason),
            }
        });
        Ok(id)
    }

    async fn send_inner(
        &self,
        id: u64,
        conn: &Connection,
        path: &Path,
        header: FileHeader,
    ) -> std::result::Result<(), String> {
        const LOST: &str = "The connection was lost.";
        let (mut send, mut recv) = conn.open_bi().await.map_err(|_| LOST.to_owned())?;
        let size = header.size;
        write_frame(&mut send, &StreamHeader::File(header), MAX_STREAM_HEADER)
            .await
            .map_err(|e| e.to_string())?;

        // Wait for the person on the other device.
        let answer: FileAnswer = read_small(&mut recv)
            .await
            .map_err(|_| "The other device did not answer.".to_owned())?;
        if !answer.accept {
            return Err("The other device declined the file.".into());
        }
        self.emit(TransferEvent::Accepted { id });

        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|e| format!("Could not read the file: {e}"))?;
        let mut buffer = vec![0u8; CHUNK];
        let mut sent = 0u64;
        let mut reported = Instant::now();
        while sent < size {
            let n = file
                .read(&mut buffer)
                .await
                .map_err(|e| format!("Could not read the file: {e}"))?;
            if n == 0 {
                return Err("The file changed while it was being sent.".into());
            }
            let n = n.min((size - sent) as usize);
            send.write_all(&buffer[..n])
                .await
                .map_err(|_| LOST.to_owned())?;
            sent += n as u64;
            if reported.elapsed() >= PROGRESS_INTERVAL {
                reported = Instant::now();
                self.emit(TransferEvent::Progress { id, bytes: sent });
            }
        }
        send.finish().map_err(|_| LOST.to_owned())?;

        let receipt: FileReceipt = read_small(&mut recv)
            .await
            .map_err(|_| "The other device did not confirm the file.".to_owned())?;
        if receipt.ok {
            Ok(())
        } else {
            Err("The other device could not save the file.".into())
        }
    }

    /// Handles a file offered by `from` on a freshly accepted stream.
    pub(crate) async fn receive(
        self: &Arc<Self>,
        from: DeviceId,
        conn_id: usize,
        header: FileHeader,
        mut send: SendStream,
        mut recv: RecvStream,
    ) {
        let _in_use = InUse::new(self, conn_id);
        let name = sanitize_file_name(&header.name);
        let size = header.size;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let (answer_tx, answer_rx) = oneshot::channel();
        let too_many = {
            let mut pending = self.pending.lock().unwrap();
            let waiting = pending
                .values()
                .filter(|(device, _)| *device == from)
                .count();
            if waiting >= MAX_PENDING_PER_DEVICE {
                true
            } else {
                pending.insert(id, (from, answer_tx));
                false
            }
        };
        if too_many {
            let _ = write_frame(&mut send, &FileAnswer { accept: false }, MAX_STREAM_HEADER).await;
            return;
        }
        self.emit(TransferEvent::Offered {
            id,
            from,
            name: name.clone(),
            size,
        });

        let decision = timeout(OFFER_TIMEOUT, answer_rx).await;
        self.pending.lock().unwrap().remove(&id);
        let folder = match decision {
            Ok(Ok(Decision::Accept(folder))) => folder,
            other => {
                let _ =
                    write_frame(&mut send, &FileAnswer { accept: false }, MAX_STREAM_HEADER).await;
                let _ = send.finish();
                let reason = if other.is_err() {
                    "No answer was given in time."
                } else {
                    "Declined."
                };
                self.emit(TransferEvent::Failed {
                    id,
                    reason: reason.into(),
                });
                return;
            }
        };

        let cancel = Arc::new(Notify::new());
        self.cancels.lock().unwrap().insert(id, cancel.clone());
        let outcome = tokio::select! {
            outcome = self.receive_inner(id, &folder, &name, size, &mut send, &mut recv) => outcome,
            _ = cancel.notified() => Err("Cancelled.".to_owned()),
        };
        self.cancels.lock().unwrap().remove(&id);
        match outcome {
            Ok(path) => {
                let _ = write_frame(&mut send, &FileReceipt { ok: true }, MAX_STREAM_HEADER).await;
                let _ = send.finish();
                self.emit(TransferEvent::Finished {
                    id,
                    path: Some(path),
                });
            }
            Err(reason) => {
                let _ = recv.stop(0u32.into());
                let _ = write_frame(&mut send, &FileReceipt { ok: false }, MAX_STREAM_HEADER).await;
                let _ = send.finish();
                self.emit(TransferEvent::Failed { id, reason });
            }
        }
    }

    async fn receive_inner(
        &self,
        id: u64,
        folder: &Path,
        name: &str,
        size: u64,
        send: &mut SendStream,
        recv: &mut RecvStream,
    ) -> std::result::Result<PathBuf, String> {
        let cannot_save = |e: std::io::Error| format!("Could not save the file: {e}");
        tokio::fs::create_dir_all(folder)
            .await
            .map_err(cannot_save)?;
        // Written under a temporary name, so a half-received file is never
        // mistaken for the real one.
        let partial = folder.join(format!(".{name}.{id}.part"));
        let mut file = tokio::fs::File::create(&partial)
            .await
            .map_err(cannot_save)?;

        write_frame(send, &FileAnswer { accept: true }, MAX_STREAM_HEADER)
            .await
            .map_err(|_| "The connection was lost.".to_owned())?;
        self.emit(TransferEvent::Accepted { id });

        let result = async {
            let mut buffer = vec![0u8; CHUNK];
            let mut received = 0u64;
            let mut reported = Instant::now();
            while received < size {
                let want = CHUNK.min((size - received) as usize);
                let n = recv
                    .read(&mut buffer[..want])
                    .await
                    .map_err(|_| "The connection was lost.".to_owned())?
                    .unwrap_or(0);
                if n == 0 {
                    return Err("The file arrived incomplete.".to_owned());
                }
                file.write_all(&buffer[..n]).await.map_err(cannot_save)?;
                received += n as u64;
                if reported.elapsed() >= PROGRESS_INTERVAL {
                    reported = Instant::now();
                    self.emit(TransferEvent::Progress {
                        id,
                        bytes: received,
                    });
                }
            }
            file.flush().await.map_err(cannot_save)?;
            file.sync_all().await.map_err(cannot_save)?;
            Ok(())
        }
        .await;
        drop(file);
        if let Err(reason) = result {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(reason);
        }

        let target = unique_path(folder, name);
        tokio::fs::rename(&partial, &target)
            .await
            .map_err(cannot_save)?;
        Ok(target)
    }

    /// Answers an offer. Returns false if it is no longer waiting.
    pub(crate) fn answer(&self, id: u64, save_into: Option<PathBuf>) -> bool {
        let Some((_, answer)) = self.pending.lock().unwrap().remove(&id) else {
            return false;
        };
        let decision = match save_into {
            Some(folder) => Decision::Accept(folder),
            None => Decision::Decline,
        };
        answer.send(decision).is_ok()
    }

    /// Stops a transfer that is under way.
    pub(crate) fn cancel(&self, id: u64) {
        if let Some(cancel) = self.cancels.lock().unwrap().get(&id) {
            cancel.notify_one();
        }
        // An offer still waiting for an answer is simply declined.
        self.answer(id, None);
    }
}

async fn read_small<T: serde::de::DeserializeOwned>(recv: &mut RecvStream) -> Result<T> {
    let body = read_frame(recv, MAX_STREAM_HEADER)
        .await?
        .ok_or_else(|| Error::Protocol("stream closed".into()))?;
    decode(&body)
}

/// Reduces a name received from another device to a plain, safe file name:
/// no folders, no control characters, nothing a file system refuses.
pub fn sanitize_file_name(name: &str) -> String {
    // Only the last path component, whichever separator was used.
    let last = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut clean = String::new();
    for c in last.chars() {
        let c = match c {
            c if c.is_control() => continue,
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c => c,
        };
        if clean.len() + c.len_utf8() > MAX_NAME_BYTES {
            break;
        }
        clean.push(c);
    }
    // No leading dots (hidden files, "..") and no trailing dots or spaces
    // (Windows drops them).
    let clean = clean
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .trim();
    if clean.is_empty() {
        "file".to_owned()
    } else {
        clean.to_owned()
    }
}

/// `folder/name`, or `folder/name (1).ext` and so on if that is taken.
fn unique_path(folder: &Path, name: &str) -> PathBuf {
    let first = folder.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, extension) = match name.rfind('.') {
        Some(dot) if dot > 0 => (&name[..dot], &name[dot..]),
        _ => (name, ""),
    };
    for n in 1..10_000 {
        let candidate = folder.join(format!("{stem} ({n}){extension}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    folder.join(format!("{stem} ({}){extension}", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cannot_escape_the_folder() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(
            sanitize_file_name("C:\\Windows\\System32\\evil.dll"),
            "evil.dll"
        );
        assert_eq!(sanitize_file_name(".."), "file");
        assert_eq!(sanitize_file_name("/"), "file");
        assert_eq!(sanitize_file_name(""), "file");
        assert_eq!(sanitize_file_name(".bashrc"), "bashrc");
        assert_eq!(sanitize_file_name("a\u{0}b\nc.txt"), "abc.txt");
        assert_eq!(sanitize_file_name("what?*.txt"), "what__.txt");
        assert_eq!(sanitize_file_name("trailing. . "), "trailing");
        assert_eq!(sanitize_file_name("Résumé 2026.pdf"), "Résumé 2026.pdf");
        assert!(sanitize_file_name(&"x".repeat(1000)).len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn existing_files_are_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unique_path(dir.path(), "a.txt"), dir.path().join("a.txt"));
        std::fs::write(dir.path().join("a.txt"), b"1").unwrap();
        assert_eq!(
            unique_path(dir.path(), "a.txt"),
            dir.path().join("a (1).txt")
        );
        std::fs::write(dir.path().join("a (1).txt"), b"2").unwrap();
        assert_eq!(
            unique_path(dir.path(), "a.txt"),
            dir.path().join("a (2).txt")
        );
        std::fs::write(dir.path().join("noext"), b"3").unwrap();
        assert_eq!(
            unique_path(dir.path(), "noext"),
            dir.path().join("noext (1)")
        );
    }
}
