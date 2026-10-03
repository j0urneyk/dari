//! File transfer, shared by host and viewer sessions.
//!
//! Offers, answers, and cancellations travel on the control stream; an accepted offer's bytes
//! travel on its own low-priority stream: one file, or a folder's files one after another. [`Transfers`] is the per-session state machine: the
//! session task feeds it control messages, incoming file streams, local commands, and the
//! results of its send and receive tasks, and sends whatever control message it returns.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use dari_net::{FileReceiver, SessionStreams, StreamError};
use dari_proto::{
    ControlMessage, FileOffer, FolderFile, MAX_FOLDER_DEPTH, MAX_FOLDER_FILES,
    MAX_FOLDER_PATH_BYTES, TransferEnd, TransferId, sanitize_file_name, split_extension,
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
    /// The file's or folder's name as offered (already safe to display).
    pub name: String,
    /// For a folder, how many files it holds; `None` for a single file.
    pub files: Option<usize>,
    pub size: u64,
    pub transferred: u64,
    pub state: TransferState,
    /// Where a received file or folder was saved.
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
    payload: Payload,
    task: Option<JoinHandle<()>>,
}

/// A file to send, with the size that was offered for it.
type Source = (PathBuf, u64);

/// What a transfer reads from or writes to.
enum Payload {
    /// Sending: the source files in the order they are sent, with their offered sizes.
    Send(Vec<Source>),
    /// Receiving, not yet accepted: the offered folder's files, or `None` for a file.
    Offered(Option<Vec<FolderFile>>),
    /// Receiving a file: the reserved `.part` file, open until the peer's stream arrives.
    File {
        part: PathBuf,
        file: Option<tokio::fs::File>,
    },
    /// Receiving a folder: the `.part` folder and where each file goes inside it.
    Folder {
        part: PathBuf,
        files: Vec<(PathBuf, u64)>,
    },
}

impl Payload {
    /// The partial file or folder to delete if the transfer stops early.
    fn partial(&self) -> Option<(&Path, bool)> {
        match self {
            Payload::File { part, .. } => Some((part, false)),
            Payload::Folder { part, .. } => Some((part, true)),
            Payload::Send(_) | Payload::Offered(_) => None,
        }
    }
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
            files: None,
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
        let listed = match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => Ok((vec![(path.clone(), metadata.len())], None)),
            Ok(metadata) if metadata.is_dir() => {
                let root = path.clone();
                tokio::task::spawn_blocking(move || list_folder(&root))
                    .await
                    .unwrap_or_else(|error| Err(error.to_string()))
                    .map(|listed| {
                        let (sources, contents) = listed.into_iter().unzip();
                        (sources, Some(contents))
                    })
            }
            Ok(_) => Err("only files and folders can be sent".into()),
            Err(error) => Err(format!("cannot read it: {error}")),
        };
        let (sources, contents): (Vec<Source>, Option<Vec<FolderFile>>) = match listed {
            Ok(listed) => listed,
            Err(error) => {
                debug!(%error, path = %path.display(), "cannot send");
                info.state = TransferState::Ended(TransferEnd::Failed);
                (self.report)(info);
                return None;
            }
        };
        info.size = sources.iter().map(|(_, size)| size).sum();
        info.files = contents.as_ref().map(Vec::len);
        let offer = FileOffer {
            id,
            name: info.name.clone(),
            size: info.size,
            contents,
        };
        (self.report)(info.clone());
        self.sending.insert(
            id,
            Entry {
                info,
                payload: Payload::Send(sources),
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
        let Payload::Offered(contents) =
            std::mem::replace(&mut entry.payload, Payload::Offered(None))
        else {
            return None;
        };
        let reserved = match contents {
            None => create_part_file(&receive_dir, &entry.info.name)
                .await
                .map(|(part, file)| Payload::File {
                    part,
                    file: Some(file),
                }),
            Some(contents) => create_part_folder(&receive_dir, &entry.info.name)
                .await
                .map(|part| Payload::Folder {
                    part,
                    files: plan_folder(&contents),
                }),
        };
        match reserved {
            Ok(payload) => {
                entry.payload = payload;
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
            files: offer.contents.as_ref().map(Vec::len),
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
                payload: Payload::Offered(offer.contents),
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
        let Payload::Send(sources) = &entry.payload else {
            return;
        };
        let sources = sources.clone();
        let streams = self.streams.clone();
        let steps = self.steps.clone();
        entry.task = Some(tokio::spawn(async move {
            let result = send_files(&streams, id, &sources, &steps).await;
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
        let (targets, finish) = match &mut entry.payload {
            Payload::File { part, file } => {
                let Some(file) = file.take() else {
                    debug!(%id, "ignoring a second file stream");
                    return;
                };
                let target = Target {
                    path: part.clone(),
                    size: entry.info.size,
                    file: Some(file),
                };
                (vec![target], Finish::File(part.clone()))
            }
            // A second stream for a folder already being received is ignored below.
            Payload::Folder { part, files } if entry.task.is_none() => {
                let targets = std::mem::take(files)
                    .into_iter()
                    .map(|(path, size)| Target {
                        path: part.join(path),
                        size,
                        file: None,
                    })
                    .collect();
                (targets, Finish::Folder(part.clone()))
            }
            Payload::Folder { .. } | Payload::Send(_) | Payload::Offered(_) => {
                debug!(%id, "ignoring a file stream for a transfer not waiting for one");
                return;
            }
        };
        let name = entry.info.name.clone();
        let steps = self.steps.clone();
        entry.task = Some(tokio::spawn(async move {
            let result = receive_files(id, stream, targets, &steps).await;
            let result = match result {
                Ok(()) => finish.complete(&name).await,
                Err(error) => Err(error),
            };
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

/// Stops an entry's task and, for a file or folder being received, deletes what was written
/// so far.
async fn stop(entry: &mut Entry) {
    if let Some(task) = entry.task.take() {
        task.abort();
        // Wait until the task has dropped its file handles; Windows can't delete open files.
        let _stopped = task.await;
    }
    if entry.info.state == TransferState::Completed {
        return;
    }
    if let Payload::File { file, .. } = &mut entry.payload {
        drop(file.take());
    }
    if let Some((part, folder)) = entry.payload.partial() {
        let removed = if folder {
            tokio::fs::remove_dir_all(part).await
        } else {
            tokio::fs::remove_file(part).await
        };
        if let Err(error) = removed
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(%error, path = %part.display(), "cannot remove a partial transfer");
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

/// Sends `sources` on one stream, each file exactly its offered size.
async fn send_files(
    streams: &SessionStreams,
    id: TransferId,
    sources: &[Source],
    steps: &mpsc::UnboundedSender<TransferStep>,
) -> Result<(), String> {
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
    for (path, size) in sources {
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
        let mut remaining = *size;
        while remaining > 0 {
            let want = usize::try_from(remaining).map_or(CHUNK, |remaining| remaining.min(CHUNK));
            let read = file
                .read(&mut buffer[..want])
                .await
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            if read == 0 {
                return Err(format!("{} shrank while it was being sent", path.display()));
            }
            sender
                .write_all(&buffer[..read])
                .await
                .map_err(|error| error.to_string())?;
            remaining -= read as u64;
            sent += read as u64;
            progress.update(sent);
        }
        let past_end = file
            .read(&mut buffer[..1])
            .await
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        if past_end > 0 {
            return Err(format!("{} grew while it was being sent", path.display()));
        }
    }
    sender.finish().map_err(|error| error.to_string())
}

/// One file to write from the stream: where, how many bytes, and its handle if already open.
struct Target {
    path: PathBuf,
    size: u64,
    file: Option<tokio::fs::File>,
}

/// Writes the stream's bytes into `targets` in order, each exactly its size, and checks that
/// the stream then ends.
async fn receive_files(
    id: TransferId,
    mut stream: FileReceiver,
    targets: Vec<Target>,
    steps: &mpsc::UnboundedSender<TransferStep>,
) -> Result<(), String> {
    let mut buffer = vec![0; CHUNK];
    let mut received = 0u64;
    let mut progress = Progress {
        id,
        steps,
        last: Instant::now(),
    };
    let write_error = |error: std::io::Error| format!("cannot write the file: {error}");
    for target in targets {
        let mut file = match target.file {
            Some(file) => file,
            None => create_received_file(&target.path)
                .await
                .map_err(write_error)?,
        };
        let mut remaining = target.size;
        while remaining > 0 {
            let want = usize::try_from(remaining).map_or(CHUNK, |remaining| remaining.min(CHUNK));
            let read = stream
                .read(&mut buffer[..want])
                .await
                .map_err(|error| format!("the transfer stopped: {error}"))?
                .ok_or("the peer sent less than it offered")?;
            file.write_all(&buffer[..read]).await.map_err(write_error)?;
            remaining -= read as u64;
            received += read as u64;
            progress.update(received);
        }
        file.flush().await.map_err(write_error)?;
    }
    let extra = stream
        .read(&mut buffer[..1])
        .await
        .map_err(|error| format!("the transfer stopped: {error}"))?;
    if extra.is_some_and(|read| read > 0) {
        return Err("the peer sent more than it offered".into());
    }
    Ok(())
}

/// Creates a file inside a folder being received, and the folders above it.
async fn create_received_file(path: &Path) -> std::io::Result<tokio::fs::File> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
}

/// How a fully received transfer gets its final name.
enum Finish {
    File(PathBuf),
    Folder(PathBuf),
}

impl Finish {
    async fn complete(self, name: &str) -> Result<PathBuf, String> {
        match self {
            Finish::File(part) => finish_part(&part, name, numbered).await,
            Finish::Folder(part) => finish_part(&part, name, numbered_folder).await,
        }
        .map_err(|error| format!("cannot save it: {error}"))
    }
}

/// Lists a folder's files for sending, depth first in name order, without following symbolic
/// links (which could loop or reach outside the folder). Fails past the protocol's limits.
fn list_folder(root: &Path) -> Result<Vec<(Source, FolderFile)>, String> {
    let mut files = Vec::new();
    let mut path_bytes = 0;
    let mut pending = vec![(root.to_path_buf(), Vec::<String>::new())];
    while let Some((dir, below)) = pending.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .map_err(|error| format!("cannot list {}: {error}", dir.display()))?
            .filter_map(Result::ok)
            .collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        // Pushed in reverse so folders are walked in name order.
        let mut subfolders = Vec::new();
        for entry in entries {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = sanitize_file_name(&entry.file_name().to_string_lossy());
            let mut path = below.clone();
            path.push(name);
            if path.len() > MAX_FOLDER_DEPTH {
                return Err("the folder is nested too deeply".into());
            }
            if kind.is_dir() {
                subfolders.push((entry.path(), path));
            } else if kind.is_file() {
                let size = entry
                    .metadata()
                    .map_err(|error| format!("cannot read {}: {error}", entry.path().display()))?
                    .len();
                path_bytes += path.iter().map(String::len).sum::<usize>();
                if files.len() == MAX_FOLDER_FILES || path_bytes > MAX_FOLDER_PATH_BYTES {
                    return Err("the folder holds too many files".into());
                }
                files.push(((entry.path(), size), FolderFile { path, size }));
            }
        }
        pending.extend(subfolders.into_iter().rev());
    }
    Ok(files)
}

/// Where each file of an offered folder goes, relative to the folder: names made safe for this
/// file system, and numbered when two would collide (also by case, which macOS and Windows
/// ignore) or when a file would take the name of a folder.
fn plan_folder(contents: &[FolderFile]) -> Vec<(PathBuf, u64)> {
    let key = |components: &[String]| components.join("/").to_lowercase();
    let paths: Vec<Vec<String>> = contents
        .iter()
        .map(|file| {
            file.path
                .iter()
                .map(|name| sanitize_file_name(name))
                .collect()
        })
        .collect();
    let folders: HashSet<String> = paths
        .iter()
        .flat_map(|path| (1..path.len()).map(|depth| key(&path[..depth])))
        .collect();
    let mut taken = HashSet::new();
    let mut planned = Vec::with_capacity(paths.len());
    for (mut path, file) in paths.into_iter().zip(contents) {
        let Some(name) = path.pop() else { continue };
        let mut attempt = 0;
        let unique = loop {
            let candidate = numbered(&name, attempt);
            path.push(candidate);
            let key = key(&path);
            if !folders.contains(&key) && taken.insert(key) {
                break path;
            }
            path.pop();
            attempt += 1;
        };
        planned.push((unique.iter().collect(), file.size));
    }
    planned
}

/// `name` with ` (n)` before its extension, for the `n`-th attempt.
fn numbered(name: &str, attempt: u32) -> String {
    if attempt == 0 {
        return name.into();
    }
    let (stem, extension) = split_extension(name);
    format!("{stem} ({attempt}){extension}")
}

/// `name` with ` (n)` after it, for the `n`-th attempt at naming a folder.
fn numbered_folder(name: &str, attempt: u32) -> String {
    if attempt == 0 {
        name.into()
    } else {
        format!("{name} ({attempt})")
    }
}

/// Creates the folder `<name>.part` in `dir` for a name that is free both as itself and as a
/// part folder.
async fn create_part_folder(dir: &Path, name: &str) -> std::io::Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;
    for attempt in 0..MAX_NAME_ATTEMPTS {
        let candidate = numbered_folder(name, attempt);
        if tokio::fs::try_exists(dir.join(&candidate)).await? {
            continue;
        }
        let part = dir.join(format!("{candidate}{PART_SUFFIX}"));
        match tokio::fs::create_dir(&part).await {
            Ok(()) => return Ok(part),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(std::io::ErrorKind::AlreadyExists.into())
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

/// Renames a finished part file or folder to `name`, or the first free numbered variant of it
/// if something with those names appeared meanwhile. Never replaces anything.
async fn finish_part(
    part: &Path,
    name: &str,
    numbered: fn(&str, u32) -> String,
) -> std::io::Result<PathBuf> {
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
        let saved = finish_part(&part, "photo.jpg", numbered).await.unwrap();
        assert_eq!(saved, dir.path().join("photo (2).jpg"));
        assert_eq!(std::fs::read(&saved).unwrap(), b"theirs");
        assert_eq!(
            std::fs::read(dir.path().join("photo.jpg")).unwrap(),
            b"mine"
        );
        assert!(!part.exists());
    }

    fn offered(paths: &[&str]) -> Vec<FolderFile> {
        paths
            .iter()
            .map(|path| FolderFile {
                path: path.split('/').map(String::from).collect(),
                size: 1,
            })
            .collect()
    }

    fn planned(paths: &[&str]) -> Vec<String> {
        plan_folder(&offered(paths))
            .into_iter()
            .map(|(path, _)| {
                path.iter()
                    .map(|component| component.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/")
            })
            .collect()
    }

    #[test]
    fn folder_files_never_collide() {
        assert_eq!(
            planned(&["a.txt", "A.TXT", "a:b", "a_b", "sub/x", "sub"]),
            ["a.txt", "A (1).TXT", "a_b", "a_b (1)", "sub/x", "sub (1)"]
        );
        // Decomposed Hangul from macOS and Windows-reserved names are made safe per component.
        assert_eq!(planned(&["\u{1112}\u{1161}\u{11AB}/CON"]), ["한/_CON"]);
    }

    #[test]
    fn folders_are_listed_in_order_without_following_links() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("b/c")).unwrap();
        std::fs::write(root.path().join("z.txt"), b"z").unwrap();
        std::fs::write(root.path().join("a.txt"), b"aa").unwrap();
        std::fs::write(root.path().join("b/c/d.txt"), b"ddd").unwrap();
        std::fs::create_dir(root.path().join("empty")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.path(), root.path().join("b/loop")).unwrap();
        let listed = list_folder(root.path()).unwrap();
        let paths: Vec<_> = listed
            .iter()
            .map(|(_, file)| (file.path.join("/"), file.size))
            .collect();
        assert_eq!(
            paths,
            [
                ("a.txt".to_owned(), 2),
                ("z.txt".to_owned(), 1),
                ("b/c/d.txt".to_owned(), 3)
            ]
        );
    }
}
