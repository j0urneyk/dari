//! File transfer, shared by host and viewer sessions.
//!
//! Offers, answers, and cancellations travel on the control stream; an accepted file's bytes
//! travel on its own low-priority stream. [`Transfers`] is the per-session state machine: the
//! session task feeds it control messages, incoming file streams, local commands, and the
//! results of its send and receive tasks, and sends whatever control message it returns.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dari_net::{FileReceiver, SessionStreams, StreamError};
use dari_proto::{
    ControlMessage, FileOffer, TransferEnd, TransferId, sanitize_file_name, split_extension,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

/// Offers from the peer tracked at once; more are declined.
const MAX_INCOMING: usize = 32;
/// Files read and written in chunks of this size.
const CHUNK: usize = 64 * 1024;
/// Progress is reported at most this often per transfer.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
/// Unidirectional streams a host lets its viewer open once file transfer is allowed.
pub(crate) const PEER_FILE_STREAMS: u32 = 4;
/// Suffix of a file still being received.
const PART_SUFFIX: &str = ".part";
/// Numbered names tried before giving up on finding a free one.
const MAX_NAME_ATTEMPTS: u32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    Sending,
    Receiving,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferState {
    /// Waiting for the receiver to accept or decline.
    Offered,
    InProgress,
    /// Sent and confirmed by the receiver, or received and saved.
    Completed,
    Ended(TransferEnd),
}

impl TransferState {
    pub fn is_finished(self) -> bool {
        matches!(self, TransferState::Completed | TransferState::Ended(_))
    }
}

/// A snapshot of one transfer, reported whenever it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    pub id: TransferId,
    pub direction: TransferDirection,
    /// The file's name as offered (already safe to display).
    pub name: String,
    pub size: u64,
    pub transferred: u64,
    pub state: TransferState,
    /// Where a received file was saved.
    pub saved_to: Option<PathBuf>,
}

/// What the local user asks of a session's transfers.
#[derive(Debug)]
pub(crate) enum TransferCommand {
    Send(PathBuf),
    Accept(TransferId),
    /// Stops a transfer in either direction, or declines an offer.
    Cancel(TransferId),
}

/// Results from a transfer's send or receive task.
#[derive(Debug)]
pub(crate) enum TransferStep {
    Progress {
        id: TransferId,
        bytes: u64,
    },
    Sent {
        id: TransferId,
        result: Result<(), String>,
    },
    Received {
        id: TransferId,
        result: Result<PathBuf, String>,
    },
}

/// Reports transfer snapshots to the UI.
pub(crate) type TransferReport = Arc<dyn Fn(Transfer) + Send + Sync>;

/// What a session lets its transfers do.
#[derive(Debug, Clone)]
pub(crate) struct TransferPolicy {
    /// Whether this session may transfer files at all (the host allows it for this session).
    pub(crate) allowed: bool,
    /// Where received files are saved; `None` declines every offer.
    pub(crate) receive_dir: Option<PathBuf>,
    /// Accept offers without asking the local user.
    pub(crate) auto_accept: bool,
}

struct Entry {
    info: Transfer,
    /// Sending: the source file. Receiving: the `.part` file being written.
    path: PathBuf,
    /// Receiving: the opened `.part` file, until the peer's stream arrives.
    part_file: Option<tokio::fs::File>,
    task: Option<JoinHandle<()>>,
}

pub(crate) struct Transfers {
    by_host: bool,
    next_index: u64,
    streams: SessionStreams,
    policy: TransferPolicy,
    sending: HashMap<TransferId, Entry>,
    receiving: HashMap<TransferId, Entry>,
    steps: mpsc::UnboundedSender<TransferStep>,
    report: TransferReport,
}

/// A violation of the transfer protocol by the peer; it ends the session.
pub(crate) type ProtocolViolation = String;

impl Transfers {
    pub(crate) fn new(
        by_host: bool,
        streams: SessionStreams,
        policy: TransferPolicy,
        report: TransferReport,
    ) -> (Self, mpsc::UnboundedReceiver<TransferStep>) {
        let (steps, step_receiver) = mpsc::unbounded_channel();
        (
            Self {
                by_host,
                next_index: 0,
                streams,
                policy,
                sending: HashMap::new(),
                receiving: HashMap::new(),
                steps,
                report,
            },
            step_receiver,
        )
    }

    pub(crate) fn set_allowed(&mut self, allowed: bool) {
        self.policy.allowed = allowed;
    }

    fn is_ours(&self, id: TransferId) -> bool {
        id.offered_by_host() == self.by_host
    }

    /// Handles a command from the local user.
    pub(crate) async fn command(&mut self, command: TransferCommand) -> Option<ControlMessage> {
        match command {
            TransferCommand::Send(path) => self.offer(path).await,
            TransferCommand::Accept(id) => self.accept(id).await,
            TransferCommand::Cancel(id) => self.cancel(id).await,
        }
    }

    async fn offer(&mut self, path: PathBuf) -> Option<ControlMessage> {
        let id = TransferId::new(self.next_index, self.by_host);
        self.next_index += 1;
        let name = path.file_name().map_or_else(
            || "file".into(),
            |name| sanitize_file_name(&name.to_string_lossy()),
        );
        let mut info = Transfer {
            id,
            direction: TransferDirection::Sending,
            name,
            size: 0,
            transferred: 0,
            state: TransferState::Offered,
            saved_to: None,
        };
        if !self.policy.allowed {
            info.state = TransferState::Ended(TransferEnd::Declined);
            (self.report)(info);
            return None;
        }
        match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => info.size = metadata.len(),
            Ok(_) => {
                debug!(path = %path.display(), "only regular files can be sent");
                info.state = TransferState::Ended(TransferEnd::Failed);
                (self.report)(info);
                return None;
            }
            Err(error) => {
                debug!(%error, path = %path.display(), "cannot read the file to send");
                info.state = TransferState::Ended(TransferEnd::Failed);
                (self.report)(info);
                return None;
            }
        }
        let offer = FileOffer {
            id,
            name: info.name.clone(),
            size: info.size,
        };
        (self.report)(info.clone());
        self.sending.insert(
            id,
            Entry {
                info,
                path,
                part_file: None,
                task: None,
            },
        );
        Some(ControlMessage::FileOffer(offer))
    }

    async fn accept(&mut self, id: TransferId) -> Option<ControlMessage> {
        let receive_dir = self.policy.receive_dir.clone();
        let entry = self.receiving.get_mut(&id)?;
        if entry.info.state != TransferState::Offered {
            return None;
        }
        let Some(receive_dir) = receive_dir.filter(|_| self.policy.allowed) else {
            return self.finish(id, TransferEnd::Declined, true).await;
        };
        match create_part_file(&receive_dir, &entry.info.name).await {
            Ok((path, file)) => {
                entry.path = path;
                entry.part_file = Some(file);
                entry.info.state = TransferState::InProgress;
                (self.report)(entry.info.clone());
                Some(ControlMessage::FileAccept(id))
            }
            Err(error) => {
                warn!(%error, dir = %receive_dir.display(), "cannot create the received file");
                self.finish(id, TransferEnd::Failed, true).await
            }
        }
    }

    async fn cancel(&mut self, id: TransferId) -> Option<ControlMessage> {
        let declining = !self.is_ours(id)
            && self
                .receiving
                .get(&id)
                .is_some_and(|entry| entry.info.state == TransferState::Offered);
        let reason = if declining {
            TransferEnd::Declined
        } else {
            TransferEnd::Cancelled
        };
        self.finish(id, reason, true).await
    }

    /// Handles a transfer message from the peer. Returns the answer to send, or a protocol
    /// violation that ends the session.
    pub(crate) async fn message(
        &mut self,
        message: ControlMessage,
    ) -> Result<Option<ControlMessage>, ProtocolViolation> {
        match message {
            ControlMessage::FileOffer(offer) => self.offered(offer).await,
            ControlMessage::FileAccept(id) => {
                if !self.is_ours(id) {
                    return Err(format!("peer accepted its own transfer {id}"));
                }
                self.start_sending(id);
                Ok(None)
            }
            ControlMessage::FileDone(id) => {
                if !self.is_ours(id) {
                    return Err(format!("peer confirmed its own transfer {id}"));
                }
                if let Some(mut entry) = self.sending.remove(&id) {
                    entry.info.state = TransferState::Completed;
                    entry.info.transferred = entry.info.size;
                    info!(name = %entry.info.name, size = entry.info.size, "file sent");
                    (self.report)(entry.info);
                }
                Ok(None)
            }
            ControlMessage::FileCancel { id, reason } => Ok(self.finish(id, reason, false).await),
            _ => Ok(None),
        }
    }

    async fn offered(
        &mut self,
        offer: FileOffer,
    ) -> Result<Option<ControlMessage>, ProtocolViolation> {
        let id = offer.id;
        if self.is_ours(id) {
            return Err(format!("peer offered transfer {id} with our id"));
        }
        if self.receiving.contains_key(&id) {
            return Err(format!("peer offered transfer {id} twice"));
        }
        let decline = Some(ControlMessage::FileCancel {
            id,
            reason: TransferEnd::Declined,
        });
        if !self.policy.allowed || self.policy.receive_dir.is_none() {
            debug!(%id, "declining a file offer: file transfer is off");
            return Ok(decline);
        }
        if self.receiving.len() >= MAX_INCOMING {
            debug!(%id, "declining a file offer: too many transfers");
            return Ok(decline);
        }
        let info = Transfer {
            id,
            direction: TransferDirection::Receiving,
            name: sanitize_file_name(&offer.name),
            size: offer.size,
            transferred: 0,
            state: TransferState::Offered,
            saved_to: None,
        };
        (self.report)(info.clone());
        self.receiving.insert(
            id,
            Entry {
                info,
                path: PathBuf::new(),
                part_file: None,
                task: None,
            },
        );
        if self.policy.auto_accept {
            Ok(self.accept(id).await)
        } else {
            Ok(None)
        }
    }

    fn start_sending(&mut self, id: TransferId) {
        let Some(entry) = self.sending.get_mut(&id) else {
            return;
        };
        if entry.info.state != TransferState::Offered {
            return;
        }
        entry.info.state = TransferState::InProgress;
        (self.report)(entry.info.clone());
        let streams = self.streams.clone();
        let path = entry.path.clone();
        let size = entry.info.size;
        let steps = self.steps.clone();
        entry.task = Some(tokio::spawn(async move {
            let result = send_file(&streams, id, &path, size, &steps).await;
            let _sent = steps.send(TransferStep::Sent { id, result });
        }));
    }

    /// Hands an accepted transfer the peer's stream of file bytes.
    pub(crate) fn incoming_stream(&mut self, id: TransferId, stream: FileReceiver) {
        let Some(entry) = self.receiving.get_mut(&id) else {
            // Cancelled meanwhile, or never offered; dropping the stream stops it.
            debug!(%id, "ignoring a file stream for no accepted transfer");
            return;
        };
        let Some(file) = entry.part_file.take() else {
            debug!(%id, "ignoring a second file stream");
            return;
        };
        let destination = Destination {
            part: entry.path.clone(),
            name: entry.info.name.clone(),
        };
        let size = entry.info.size;
        let steps = self.steps.clone();
        entry.task = Some(tokio::spawn(async move {
            let result = receive_file(id, stream, file, &destination, size, &steps).await;
            let _sent = steps.send(TransferStep::Received { id, result });
        }));
    }

    /// Handles a result from a send or receive task.
    pub(crate) async fn step(&mut self, step: TransferStep) -> Option<ControlMessage> {
        match step {
            TransferStep::Progress { id, bytes } => {
                let entry = self
                    .sending
                    .get_mut(&id)
                    .or_else(|| self.receiving.get_mut(&id))?;
                if !entry.info.state.is_finished() {
                    entry.info.transferred = bytes;
                    (self.report)(entry.info.clone());
                }
                None
            }
            TransferStep::Sent { id, result } => {
                let entry = self.sending.get_mut(&id)?;
                entry.task = None;
                match result {
                    // Completed once the receiver confirms it saved the file.
                    Ok(()) => {
                        entry.info.transferred = entry.info.size;
                        (self.report)(entry.info.clone());
                        None
                    }
                    Err(error) => {
                        warn!(%error, name = %entry.info.name, "sending a file failed");
                        self.finish(id, TransferEnd::Failed, true).await
                    }
                }
            }
            TransferStep::Received { id, result } => {
                let entry = self.receiving.get_mut(&id)?;
                entry.task = None;
                match result {
                    Ok(saved) => {
                        let mut entry = self.receiving.remove(&id)?;
                        info!(path = %saved.display(), size = entry.info.size, "file received");
                        entry.info.state = TransferState::Completed;
                        entry.info.transferred = entry.info.size;
                        entry.info.saved_to = Some(saved);
                        (self.report)(entry.info);
                        Some(ControlMessage::FileDone(id))
                    }
                    Err(error) => {
                        warn!(%error, name = %entry.info.name, "receiving a file failed");
                        self.finish(id, TransferEnd::Failed, true).await
                    }
                }
            }
        }
    }

    /// Ends a transfer: stops its task, removes a partial file, and reports it. Returns the
    /// cancellation to send when the local side ended it (`tell_peer`).
    async fn finish(
        &mut self,
        id: TransferId,
        reason: TransferEnd,
        tell_peer: bool,
    ) -> Option<ControlMessage> {
        let mut entry = if self.is_ours(id) {
            self.sending.remove(&id)
        } else {
            self.receiving.remove(&id)
        }?;
        stop(&mut entry).await;
        entry.info.state = TransferState::Ended(reason);
        (self.report)(entry.info);
        tell_peer.then_some(ControlMessage::FileCancel { id, reason })
    }

    /// Stops every transfer when the session ends.
    pub(crate) async fn shut_down(&mut self) {
        let entries: Vec<Entry> = self
            .sending
            .drain()
            .chain(self.receiving.drain())
            .map(|(_, entry)| entry)
            .collect();
        for mut entry in entries {
            stop(&mut entry).await;
            entry.info.state = TransferState::Ended(TransferEnd::Cancelled);
            (self.report)(entry.info);
        }
    }
}

/// Stops an entry's task and, for a file being received, deletes what was written so far.
async fn stop(entry: &mut Entry) {
    if let Some(task) = entry.task.take() {
        task.abort();
        // Wait until the task has dropped its file handle; Windows can't delete open files.
        let _stopped = task.await;
    }
    if entry.info.direction == TransferDirection::Receiving
        && entry.info.state != TransferState::Completed
        && !entry.path.as_os_str().is_empty()
    {
        drop(entry.part_file.take());
        if let Err(error) = tokio::fs::remove_file(&entry.path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(%error, path = %entry.path.display(), "cannot remove a partial file");
        }
    }
}

/// Reports progress at most every [`PROGRESS_INTERVAL`].
struct Progress<'a> {
    id: TransferId,
    steps: &'a mpsc::UnboundedSender<TransferStep>,
    last: Instant,
}

impl Progress<'_> {
    fn update(&mut self, bytes: u64) {
        if self.last.elapsed() >= PROGRESS_INTERVAL {
            self.last = Instant::now();
            let _sent = self
                .steps
                .send(TransferStep::Progress { id: self.id, bytes });
        }
    }
}

async fn send_file(
    streams: &SessionStreams,
    id: TransferId,
    path: &Path,
    size: u64,
    steps: &mpsc::UnboundedSender<TransferStep>,
) -> Result<(), String> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|error| format!("cannot open the file: {error}"))?;
    let mut sender = streams
        .open_file_sender(id)
        .await
        .map_err(|error: StreamError| error.to_string())?;
    let mut buffer = vec![0; CHUNK];
    let mut sent = 0u64;
    let mut progress = Progress {
        id,
        steps,
        last: Instant::now(),
    };
    loop {
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|error| format!("cannot read the file: {error}"))?;
        if read == 0 {
            break;
        }
        sent += read as u64;
        if sent > size {
            return Err("the file grew while it was being sent".into());
        }
        sender
            .write_all(&buffer[..read])
            .await
            .map_err(|error| error.to_string())?;
        progress.update(sent);
    }
    if sent != size {
        return Err("the file shrank while it was being sent".into());
    }
    sender.finish().map_err(|error| error.to_string())
}

/// Where a received file is written, and the name it should end up with.
struct Destination {
    part: PathBuf,
    name: String,
}

async fn receive_file(
    id: TransferId,
    mut stream: FileReceiver,
    mut file: tokio::fs::File,
    destination: &Destination,
    size: u64,
    steps: &mpsc::UnboundedSender<TransferStep>,
) -> Result<PathBuf, String> {
    let mut buffer = vec![0; CHUNK];
    let mut received = 0u64;
    let mut progress = Progress {
        id,
        steps,
        last: Instant::now(),
    };
    loop {
        let Some(read) = stream
            .read(&mut buffer)
            .await
            .map_err(|error| format!("the transfer stopped: {error}"))?
        else {
            break;
        };
        received += read as u64;
        if received > size {
            return Err("the peer sent more than it offered".into());
        }
        file.write_all(&buffer[..read])
            .await
            .map_err(|error| format!("cannot write the file: {error}"))?;
        progress.update(received);
    }
    if received != size {
        return Err("the peer sent less than it offered".into());
    }
    file.flush()
        .await
        .map_err(|error| format!("cannot write the file: {error}"))?;
    drop(file);
    finish_part_file(&destination.part, &destination.name)
        .await
        .map_err(|error| format!("cannot save the file: {error}"))
}

/// `name` with ` (n)` before its extension, for the `n`-th attempt.
fn numbered(name: &str, attempt: u32) -> String {
    if attempt == 0 {
        return name.into();
    }
    let (stem, extension) = split_extension(name);
    format!("{stem} ({attempt}){extension}")
}

/// Creates `<name>.part` in `dir` for a name that is free both as itself and as a part file.
async fn create_part_file(dir: &Path, name: &str) -> std::io::Result<(PathBuf, tokio::fs::File)> {
    tokio::fs::create_dir_all(dir).await?;
    for attempt in 0..MAX_NAME_ATTEMPTS {
        let candidate = numbered(name, attempt);
        if tokio::fs::try_exists(dir.join(&candidate)).await? {
            continue;
        }
        let part = dir.join(format!("{candidate}{PART_SUFFIX}"));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)
            .await
        {
            Ok(file) => return Ok((part, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::ErrorKind::AlreadyExists.into())
}

/// Renames a finished part file to `name`, or the first free numbered variant of it if files
/// with those names appeared meanwhile. Never replaces an existing file.
async fn finish_part_file(part: &Path, name: &str) -> std::io::Result<PathBuf> {
    let dir = part.parent().unwrap_or(Path::new("."));
    for attempt in 0..MAX_NAME_ATTEMPTS {
        let destination = dir.join(numbered(name, attempt));
        if !tokio::fs::try_exists(&destination).await? {
            tokio::fs::rename(part, &destination).await?;
            return Ok(destination);
        }
    }
    Err(std::io::ErrorKind::AlreadyExists.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_names_keep_the_extension() {
        assert_eq!(numbered("a.txt", 0), "a.txt");
        assert_eq!(numbered("a.txt", 2), "a (2).txt");
        assert_eq!(numbered("archive.tar.gz", 1), "archive.tar (1).gz");
        assert_eq!(numbered(".env", 1), ".env (1)");
    }

    #[tokio::test]
    async fn received_files_never_replace_existing_ones() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("photo.jpg"), b"mine").unwrap();
        let (part, mut file) = create_part_file(dir.path(), "photo.jpg").await.unwrap();
        assert_eq!(part, dir.path().join("photo (1).jpg.part"));
        file.write_all(b"theirs").await.unwrap();
        drop(file);
        // Someone created the planned name while the file was arriving.
        std::fs::write(dir.path().join("photo (1).jpg"), b"other").unwrap();
        let saved = finish_part_file(&part, "photo.jpg").await.unwrap();
        assert_eq!(saved, dir.path().join("photo (2).jpg"));
        assert_eq!(std::fs::read(&saved).unwrap(), b"theirs");
        assert_eq!(
            std::fs::read(dir.path().join("photo.jpg")).unwrap(),
            b"mine"
        );
        assert!(!part.exists());
    }
}
