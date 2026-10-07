use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;
use std::time::Duration;

use connectrpc::{ConnectError, ErrorCode, ServiceStream};
use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc::{channel, unbounded_channel};
use tokio::time::{Instant, MissedTickBehavior, interval_at};
use tokio_stream::wrappers::ReceiverStream;

use super::entry::entry_info;
use crate::proto::filesystem::watch_dir_response::{KeepAlive, StartEvent};
use crate::proto::filesystem::{EntryInfo, EventType, FilesystemEvent, WatchDirResponse};
use crate::util::fs::os_error;

const ID_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz1234567890";

pub struct FileWatcher {
    pub watcher: RecommendedWatcher,
    pub pending: Arc<Mutex<PendingEvents>>,
}

#[derive(Default)]
pub struct PendingEvents {
    pub events: Vec<FilesystemEvent>,
    pub error: Option<String>,
}

pub fn watcher_id() -> String {
    let random: String = (0..20)
        .map(|_| ID_ALPHABET[rand::random_range(0..ID_ALPHABET.len())] as char)
        .collect();
    format!("w{random}")
}

pub fn check_watch_path(path: &Path, allow_network_mounts: bool) -> Result<(), ConnectError> {
    let shown = path.display();
    let meta = std::fs::metadata(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => {
            ConnectError::not_found(format!("path {shown} not found: {}", os_error("stat", path, &e)))
        }
        _ => ConnectError::internal(format!("error statting path {shown}: {}", os_error("stat", path, &e))),
    })?;

    if !meta.is_dir() {
        return Err(ConnectError::invalid_argument(format!(
            "path {shown} not a directory: %!w(<nil>)"
        )));
    }

    if allow_network_mounts {
        return Ok(());
    }
    let on_network =
        is_network_mount(path).map_err(|e| ConnectError::internal(format!("error checking mount status: {e}")))?;
    if on_network {
        return Err(ConnectError::invalid_argument(format!(
            "cannot watch path on network filesystem: {shown}"
        )));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn is_network_mount(path: &Path) -> Result<bool, String> {
    use nix::sys::statfs::{FUSE_SUPER_MAGIC, FsType, NFS_SUPER_MAGIC, SMB_SUPER_MAGIC, statfs};

    const NETWORK_FILESYSTEMS: [FsType; 5] = [
        NFS_SUPER_MAGIC,
        SMB_SUPER_MAGIC,
        FUSE_SUPER_MAGIC,
        FsType(0xFF53_4D42),
        FsType(0xFE53_4D42),
    ];

    let stat = statfs(path).map_err(|errno| {
        let reason = crate::util::fs::reason(&io::Error::from(errno));
        format!("failed to statfs {}: {reason}", path.display())
    })?;
    Ok(NETWORK_FILESYSTEMS.contains(&stat.filesystem_type()))
}

#[cfg(not(target_os = "linux"))]
fn is_network_mount(_path: &Path) -> Result<bool, String> {
    Ok(false)
}

pub fn watch(
    path: &Path,
    recursive: bool,
    handler: impl notify::EventHandler,
) -> Result<RecommendedWatcher, ConnectError> {
    let mut watcher = notify::recommended_watcher(handler)
        .map_err(|e| ConnectError::internal(format!("error creating watcher: {e}")))?;
    let mode = if recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    watcher
        .watch(path, mode)
        .map_err(|e| ConnectError::internal(format!("error adding path {} to watcher: {e}", path.display())))?;
    Ok(watcher)
}

pub fn record_events(pending: &Mutex<PendingEvents>, result: notify::Result<Event>, root: &Path, include_entry: bool) {
    let events = result.map(|event| filesystem_events(&event, root, include_entry));
    let mut pending = pending.lock();
    if pending.error.is_some() {
        return;
    }
    match events {
        Ok(events) => pending.events.extend(events),
        Err(e) => pending.error = Some(format!("watcher error: {e}")),
    }
}

pub fn stream_events(
    root: PathBuf,
    recursive: bool,
    include_entry: bool,
    keepalive_interval: Duration,
) -> Result<ServiceStream<WatchDirResponse>, ConnectError> {
    let (event_sender, mut events) = unbounded_channel();
    let watcher = watch(&root, recursive, move |result| {
        let _ = event_sender.send(result);
    })?;

    let (responses, stream) = channel(16);
    tokio::spawn(async move {
        let _watcher = watcher;
        let start = WatchDirResponse {
            event: StartEvent::default().into(),
            ..Default::default()
        };
        if responses.send(Ok(start)).await.is_err() {
            return;
        }

        let mut keepalive = interval_at(Instant::now() + keepalive_interval, keepalive_interval);
        keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = responses.closed() => return,
                _ = keepalive.tick() => {
                    let response = WatchDirResponse {
                        event: KeepAlive::default().into(),
                        ..Default::default()
                    };
                    if responses.send(Ok(response)).await.is_err() {
                        return;
                    }
                }
                event = events.recv() => {
                    let event = match event {
                        Some(Ok(event)) => event,
                        Some(Err(e)) => {
                            let _ = responses.send(Err(ConnectError::internal(format!("watcher error: {e}")))).await;
                            return;
                        }
                        None => return,
                    };
                    for filesystem_event in filesystem_events(&event, &root, include_entry) {
                        let response = WatchDirResponse {
                            event: filesystem_event.into(),
                            ..Default::default()
                        };
                        if responses.send(Ok(response)).await.is_err() {
                            return;
                        }
                        keepalive.reset();
                    }
                }
            }
        }
    });
    Ok(Box::pin(ReceiverStream::new(stream)))
}

pub fn filesystem_events(event: &Event, root: &Path, include_entry: bool) -> Vec<FilesystemEvent> {
    let Some(event_type) = event_type(&event.kind) else {
        return Vec::new();
    };
    let carries_entry = include_entry
        && matches!(
            event_type,
            EventType::EVENT_TYPE_CREATE | EventType::EVENT_TYPE_WRITE | EventType::EVENT_TYPE_CHMOD
        );

    event
        .paths
        .iter()
        .map(|path| FilesystemEvent {
            name: relative_name(root, path),
            r#type: event_type.into(),
            entry: if carries_entry {
                event_entry(path).into()
            } else {
                None.into()
            },
            ..Default::default()
        })
        .collect()
}

fn event_type(kind: &EventKind) -> Option<EventType> {
    match kind {
        EventKind::Create(_) | EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
            Some(EventType::EVENT_TYPE_CREATE)
        }
        EventKind::Modify(ModifyKind::Name(RenameMode::From)) => Some(EventType::EVENT_TYPE_RENAME),
        EventKind::Modify(ModifyKind::Data(_)) => Some(EventType::EVENT_TYPE_WRITE),
        EventKind::Modify(ModifyKind::Metadata(_)) => Some(EventType::EVENT_TYPE_CHMOD),
        EventKind::Remove(_) => Some(EventType::EVENT_TYPE_REMOVE),
        _ => None,
    }
}

fn event_entry(path: &Path) -> Option<EntryInfo> {
    entry_info(path)
        .inspect_err(|e| {
            if e.code != ErrorCode::NotFound {
                tracing::warn!(path = %path.display(), error = %e, "failed to get entry info for filesystem event");
            }
        })
        .ok()
}

fn relative_name(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".to_owned(),
        Ok(relative) => relative.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use notify::event::{AccessKind, CreateKind, DataChange, MetadataKind, RemoveKind};

    use super::*;

    fn events(kind: EventKind, paths: &[&str]) -> Vec<(String, EventType)> {
        let event = Event {
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: Default::default(),
        };
        filesystem_events(&event, Path::new("/root"), false)
            .into_iter()
            .map(|e| (e.name, e.r#type.as_known().unwrap()))
            .collect()
    }

    #[test]
    fn maps_notify_kinds_to_event_types() {
        let cases = [
            (EventKind::Create(CreateKind::File), EventType::EVENT_TYPE_CREATE),
            (
                EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                EventType::EVENT_TYPE_CREATE,
            ),
            (
                EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                EventType::EVENT_TYPE_RENAME,
            ),
            (
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                EventType::EVENT_TYPE_WRITE,
            ),
            (
                EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
                EventType::EVENT_TYPE_CHMOD,
            ),
            (EventKind::Remove(RemoveKind::File), EventType::EVENT_TYPE_REMOVE),
        ];
        for (kind, expected) in cases {
            assert_eq!(events(kind, &["/root/a.txt"]), vec![("a.txt".to_owned(), expected)]);
        }
    }

    #[test]
    fn ignores_access_and_paired_renames() {
        assert!(events(EventKind::Access(AccessKind::Any), &["/root/a"]).is_empty());
        assert!(
            events(
                EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                &["/root/a", "/root/b"]
            )
            .is_empty()
        );
    }

    #[test]
    fn names_are_relative_to_root() {
        let names: Vec<String> = events(EventKind::Remove(RemoveKind::Any), &["/root/x/y.txt", "/root"])
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec!["x/y.txt", "."]);
    }

    #[test]
    fn entry_only_for_events_about_existing_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let event = |kind| Event {
            kind,
            paths: vec![file.clone()],
            attrs: Default::default(),
        };

        let created = filesystem_events(&event(EventKind::Create(CreateKind::File)), tmp.path(), true);
        assert_eq!(created[0].entry.name, "a.txt");

        let removed = filesystem_events(&event(EventKind::Remove(RemoveKind::File)), tmp.path(), true);
        assert!(!removed[0].entry.is_set());

        let without_entry = filesystem_events(&event(EventKind::Create(CreateKind::File)), tmp.path(), false);
        assert!(!without_entry[0].entry.is_set());
    }

    #[test]
    fn watcher_ids_look_like_go() {
        let id = watcher_id();
        assert_eq!(id.len(), 21);
        assert!(id.starts_with('w'));
        assert!(id[1..].bytes().all(|b| ID_ALPHABET.contains(&b)));
    }
}
