use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rand::random;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;
use zbus::zvariant::{OwnedValue, Str};

use crate::yadisk::{ResourceEntry, ResourceKind, YandexDiskClient, YandexError};

pub const ROOT_PATH: &str = "disk:/";
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(500);
const WORKER_LEASE_SECONDS: i64 = 30;
const MAX_SYNC_ITEMS: usize = 32;

pub trait RemoteSyncClient: Send + Sync {
    fn fetch_resource_metadata(&self, path: &str) -> Result<ResourceEntry, YandexError>;
    fn list_directory(&self, path: &str) -> Result<Vec<ResourceEntry>, YandexError>;
    fn create_directory(&self, path: &str) -> Result<(), YandexError>;
    fn delete_resource(&self, path: &str, permanently: bool) -> Result<(), YandexError>;
    fn move_resource(&self, from: &str, to: &str, overwrite: bool) -> Result<(), YandexError>;
    fn resolve_download_url(&self, path: &str) -> Result<String, YandexError>;
    fn resolve_upload_url(&self, path: &str, overwrite: bool) -> Result<String, YandexError>;
    fn upload_file(&self, href: &str, local_path: &Path) -> Result<(), YandexError>;
    fn download_file(&self, href: &str) -> Result<Vec<u8>, YandexError>;
}

impl RemoteSyncClient for YandexDiskClient {
    fn fetch_resource_metadata(&self, path: &str) -> Result<ResourceEntry, YandexError> {
        YandexDiskClient::fetch_resource_metadata(self, path)
    }

    fn list_directory(&self, path: &str) -> Result<Vec<ResourceEntry>, YandexError> {
        YandexDiskClient::list_directory(self, path)
    }

    fn create_directory(&self, path: &str) -> Result<(), YandexError> {
        YandexDiskClient::create_directory(self, path)
    }

    fn delete_resource(&self, path: &str, permanently: bool) -> Result<(), YandexError> {
        YandexDiskClient::delete_resource(self, path, permanently)
    }

    fn move_resource(&self, from: &str, to: &str, overwrite: bool) -> Result<(), YandexError> {
        YandexDiskClient::move_resource(self, from, to, overwrite)
    }

    fn resolve_download_url(&self, path: &str) -> Result<String, YandexError> {
        YandexDiskClient::resolve_download_url(self, path)
    }

    fn resolve_upload_url(&self, path: &str, overwrite: bool) -> Result<String, YandexError> {
        YandexDiskClient::resolve_upload_url(self, path, overwrite)
    }

    fn upload_file(&self, href: &str, local_path: &Path) -> Result<(), YandexError> {
        YandexDiskClient::upload_file(self, href, local_path)
    }

    fn download_file(&self, href: &str) -> Result<Vec<u8>, YandexError> {
        YandexDiskClient::download_file(self, href)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum FileKind {
    Directory = 0,
    File = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum SyncState {
    Synced = 0,
    QueuedUpload = 1,
    Uploading = 2,
    Downloading = 3,
    Conflict = 4,
    QueuedDelete = 5,
    Error = 6,
    Placeholder = 7,
    QueuedMkdir = 8,
    QueuedMove = 9,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ContentStatus {
    Missing = 0,
    Cached = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum OperationType {
    Upload = 0,
    Delete = 1,
    Mkdir = 2,
    Move = 3,
    Rename = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum OperationStatus {
    Pending = 0,
    Leased = 1,
    Done = 2,
    RetryableError = 3,
    PermanentError = 4,
    Conflict = 5,
}

macro_rules! impl_try_from_i32 {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        impl TryFrom<i32> for $name {
            type Error = SyncError;

            fn try_from(value: i32) -> Result<Self, SyncError> {
                match value {
                    $(x if x == Self::$variant as i32 => Ok(Self::$variant),)+
                    other => Err(SyncError::InvalidEnum {
                        ty: stringify!($name),
                        value: other,
                    }),
                }
            }
        }
    };
}

impl_try_from_i32!(FileKind { Directory, File });
impl_try_from_i32!(SyncState {
    Synced,
    QueuedUpload,
    Uploading,
    Downloading,
    Conflict,
    QueuedDelete,
    Error,
    Placeholder,
    QueuedMkdir,
    QueuedMove,
});
impl_try_from_i32!(ContentStatus { Missing, Cached });
impl_try_from_i32!(OperationType {
    Upload,
    Delete,
    Mkdir,
    Move,
    Rename,
});
impl_try_from_i32!(OperationStatus {
    Pending,
    Leased,
    Done,
    RetryableError,
    PermanentError,
    Conflict,
});

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("entry not found")]
    NotFound,
    #[error("entry already exists")]
    AlreadyExists,
    #[error("entry is not a directory")]
    NotDir,
    #[error("entry is a directory")]
    IsDir,
    #[error("directory is not empty")]
    DirectoryNotEmpty,
    #[error("remote operation conflict: {0}")]
    Conflict(String),
    #[error("local I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("remote error: {0}")]
    Remote(#[from] YandexError),
    #[error("unknown enum value for {ty}: {value}")]
    InvalidEnum { ty: &'static str, value: i32 },
    #[error("invalid local state: {0}")]
    InvalidState(String),
}

#[derive(Debug, Clone)]
pub struct LocalNode {
    pub file_id: String,
    pub path: String,
    pub parent_path: Option<String>,
    pub name: String,
    pub kind: FileKind,
    pub size: u64,
    pub mtime: SystemTime,
    pub sync_state: SyncState,
    pub remote_version: Option<String>,
    pub remote_path: Option<String>,
    pub local_version: i64,
    pub synced_local_version: i64,
    pub content_status: ContentStatus,
    pub cache_rel_path: Option<String>,
    pub is_deleted: bool,
}

#[derive(Debug, Clone)]
pub struct SyncSummarySnapshot {
    pub active_count: u32,
    pub uploading_count: u32,
    pub downloading_count: u32,
    pub queued_count: u32,
    pub conflict_count: u32,
    pub error_count: u32,
    pub last_update_unix: i64,
    pub is_syncing: bool,
    pub attention_required: bool,
}

#[derive(Debug, Clone)]
pub struct SyncItemSnapshot {
    pub path: String,
    pub state: String,
    pub direction: String,
    pub progress: u32,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct SyncService {
    db: Arc<Mutex<Connection>>,
    client: Arc<dyn RemoteSyncClient>,
    cache_dir: PathBuf,
    runtime: Arc<Mutex<RuntimeState>>,
    change_tx: watch::Sender<u64>,
}

#[derive(Debug, Default)]
struct RuntimeState {
    downloads: HashMap<String, ActiveTransfer>,
    last_update_unix: i64,
}

#[derive(Debug, Clone)]
struct ActiveTransfer {
    path: String,
    bytes_total: u64,
    updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct OperationPayload {
    from_path: Option<String>,
    to_path: Option<String>,
}

#[derive(Debug, Clone)]
struct QueuedOperation {
    id: i64,
    file_id: String,
    op_type: OperationType,
    payload: OperationPayload,
}

#[derive(Debug)]
struct FileRow {
    file_id: String,
    path: String,
    parent_path: Option<String>,
    name: String,
    kind: FileKind,
    sync_state: SyncState,
    remote_version: Option<String>,
    local_version: i64,
    synced_local_version: i64,
    mtime: i64,
    size: u64,
    content_status: ContentStatus,
    cache_rel_path: Option<String>,
    is_deleted: bool,
    remote_path: Option<String>,
    children_cached: bool,
    updated_at: i64,
}

impl SyncService {
    pub fn new(client: YandexDiskClient) -> Result<Arc<Self>, SyncError> {
        Self::with_client(Arc::new(client), None)
    }

    pub fn with_client(
        client: Arc<dyn RemoteSyncClient>,
        state_root: Option<PathBuf>,
    ) -> Result<Arc<Self>, SyncError> {
        let state_root = state_root.unwrap_or_else(default_state_root);
        fs::create_dir_all(&state_root)?;
        let service_root = state_root.join("discohack-daemon");
        let cache_dir = service_root.join("cache");
        fs::create_dir_all(&cache_dir)?;
        let db_path = service_root.join("state.sqlite3");
        let mut db = Connection::open(db_path)?;
        apply_migrations(&mut db)?;
        let now = unix_now();
        let (change_tx, _) = watch::channel(0u64);
        let service = Arc::new(Self {
            db: Arc::new(Mutex::new(db)),
            client,
            cache_dir,
            runtime: Arc::new(Mutex::new(RuntimeState {
                downloads: HashMap::new(),
                last_update_unix: now,
            })),
            change_tx,
        });

        service.recover_queue_state()?;
        service.ensure_root_bootstrapped()?;
        Ok(service)
    }

    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.change_tx.subscribe()
    }

    pub fn ensure_root_available(&self) -> Result<(), SyncError> {
        self.ensure_root_bootstrapped()
    }

    pub fn has_local_state(&self) -> Result<bool, SyncError> {
        let db = self.db.lock().unwrap();
        let exists: Option<String> = db
            .query_row(
                "SELECT file_id FROM files WHERE path = ?1 AND is_deleted = 0 LIMIT 1",
                params![ROOT_PATH],
                |row| row.get(0),
            )
            .optional()?;
        Ok(exists.is_some())
    }

    pub fn start_worker(self: &Arc<Self>) -> thread::JoinHandle<()> {
        let service = Arc::clone(self);
        thread::spawn(move || loop {
            match service.process_one_operation() {
                Ok(true) => continue,
                Ok(false) => thread::sleep(WORKER_POLL_INTERVAL),
                Err(_) => thread::sleep(WORKER_POLL_INTERVAL),
            }
        })
    }

    pub fn root_node(&self) -> Result<LocalNode, SyncError> {
        self.get_entry(ROOT_PATH)
    }

    pub fn get_entry(&self, path: &str) -> Result<LocalNode, SyncError> {
        {
            let db = self.db.lock().unwrap();
            if let Some(row) = load_file_row_by_path(&db, path)? {
                return Ok(row.into_local_node());
            }
        }

        let remote = self.client.fetch_resource_metadata(path)?;
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let parent_hint = parent_path_of(path);
        upsert_remote_entry_tx(&tx, path, remote, parent_hint.as_deref())?;
        tx.commit()?;
        drop(db);
        self.notify_change();

        let db = self.db.lock().unwrap();
        let row = load_file_row_by_path(&db, path)?.ok_or(SyncError::NotFound)?;
        Ok(row.into_local_node())
    }

    pub fn get_entry_by_file_id(&self, file_id: &str) -> Result<LocalNode, SyncError> {
        let db = self.db.lock().unwrap();
        let row = load_file_row_by_id(&db, file_id)?.ok_or(SyncError::NotFound)?;
        Ok(row.into_local_node())
    }

    pub fn lookup_child(&self, parent_path: &str, name: &str) -> Result<LocalNode, SyncError> {
        let path = join_remote_path(parent_path, name);
        {
            let db = self.db.lock().unwrap();
            if let Some(row) = load_file_row_by_path(&db, &path)? {
                return Ok(row.into_local_node());
            }

            if let Some(parent) = load_file_row_by_path(&db, parent_path)? {
                if parent.kind != FileKind::Directory {
                    return Err(SyncError::NotDir);
                }
                if parent.children_cached || has_local_children(&db, parent_path)? {
                    return Err(SyncError::NotFound);
                }
            }
        }
        self.get_entry(&path)
    }

    pub fn list_directory(&self, path: &str) -> Result<Vec<LocalNode>, SyncError> {
        let mut parent = {
            let db = self.db.lock().unwrap();
            load_file_row_by_path(&db, path)?
        };
        if parent.is_none() {
            let _ = self.get_entry(path)?;
            let db = self.db.lock().unwrap();
            parent = load_file_row_by_path(&db, path)?;
        }

        let parent = parent.ok_or(SyncError::NotFound)?;
        if parent.kind != FileKind::Directory {
            return Err(SyncError::NotDir);
        }

        let local_rows = {
            let db = self.db.lock().unwrap();
            load_directory_rows(&db, path)?
        };
        if parent.children_cached || !local_rows.is_empty() {
            return Ok(local_rows
                .into_iter()
                .map(FileRow::into_local_node)
                .collect());
        }

        if let Ok(children) = self.client.list_directory(path) {
            let mut db = self.db.lock().unwrap();
            let tx = db.transaction()?;
            refresh_directory_from_remote(&tx, &parent.file_id, path, children)?;
            tx.commit()?;
            self.notify_change();
        }

        let db = self.db.lock().unwrap();
        let rows = load_directory_rows(&db, path)?;
        Ok(rows.into_iter().map(FileRow::into_local_node).collect())
    }

    pub fn read_file(&self, file_id: &str, offset: u64, size: u32) -> Result<Vec<u8>, SyncError> {
        let cache_path = self.ensure_cached(file_id)?;
        read_local_range(&cache_path, offset, size)
    }

    pub fn prepare_write(&self, file_id: &str, truncate: bool) -> Result<LocalNode, SyncError> {
        let node = self.get_entry_by_file_id(file_id)?;
        if node.kind != FileKind::File {
            return Err(SyncError::IsDir);
        }
        if truncate {
            let cache_path = self.materialize_local_cache(file_id, 0)?;
            fs::OpenOptions::new()
                .write(true)
                .open(cache_path)?
                .set_len(0)?;
            self.apply_local_file_change(file_id, 0)?;
            return self.get_entry_by_file_id(file_id);
        }

        if node.content_status == ContentStatus::Cached {
            let _ = self.materialize_local_cache(file_id, node.size)?;
            return Ok(node);
        }

        if node.size == 0 || node.remote_path.is_none() {
            let _ = self.materialize_local_cache(file_id, node.size)?;
            return self.get_entry_by_file_id(file_id);
        }

        let _ = self.ensure_cached(file_id)?;
        self.get_entry_by_file_id(file_id)
    }

    pub fn write_file(&self, file_id: &str, offset: u64, data: &[u8]) -> Result<u32, SyncError> {
        let cache_path = self.ensure_cached(file_id)?;
        write_local_range(&cache_path, offset, data)?;
        let size = fs::metadata(cache_path)?.len();
        self.apply_local_file_change(file_id, size)?;
        Ok(data.len() as u32)
    }

    pub fn truncate_file(&self, file_id: &str, size: u64) -> Result<LocalNode, SyncError> {
        let cache_path = self.materialize_local_cache(file_id, size)?;
        fs::OpenOptions::new()
            .write(true)
            .open(cache_path)?
            .set_len(size)?;
        self.apply_local_file_change(file_id, size)?;
        self.get_entry_by_file_id(file_id)
    }

    pub fn create_file(&self, parent_path: &str, name: &str) -> Result<LocalNode, SyncError> {
        let parent = self.get_entry(parent_path)?;
        if parent.kind != FileKind::Directory {
            return Err(SyncError::NotDir);
        }
        let path = join_remote_path(parent_path, name);
        let now = unix_now();
        let file_id = new_id("file");
        let cache_rel_path = relative_cache_path(&file_id);
        let cache_path = self.cache_dir.join(&cache_rel_path);
        ensure_parent_dir(&cache_path)?;
        fs::write(&cache_path, [])?;

        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        if load_file_row_by_path(&tx, &path)?.is_some() {
            return Err(SyncError::AlreadyExists);
        }
        tx.execute(
            "INSERT INTO files (
                file_id, path, parent_path, name, kind, sync_state, remote_version,
                local_version, synced_local_version, mtime, size, hash, content_status,
                cache_rel_path, is_deleted, remote_path, children_cached, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 1, 0, ?7, 0, NULL, ?8, ?9, 0, NULL, 0, ?7)",
            params![
                file_id,
                path,
                parent_path,
                name,
                FileKind::File as i32,
                SyncState::QueuedUpload as i32,
                now,
                ContentStatus::Cached as i32,
                cache_rel_path,
            ],
        )?;
        enqueue_upload_tx(&tx, &file_id)?;
        tx.commit()?;
        drop(db);
        self.notify_change();
        self.get_entry(&path)
    }

    pub fn mkdir(&self, parent_path: &str, name: &str) -> Result<LocalNode, SyncError> {
        let parent = self.get_entry(parent_path)?;
        if parent.kind != FileKind::Directory {
            return Err(SyncError::NotDir);
        }
        let path = join_remote_path(parent_path, name);
        let file_id = new_id("dir");
        let now = unix_now();

        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        if load_file_row_by_path(&tx, &path)?.is_some() {
            return Err(SyncError::AlreadyExists);
        }
        tx.execute(
            "INSERT INTO files (
                file_id, path, parent_path, name, kind, sync_state, remote_version,
                local_version, synced_local_version, mtime, size, hash, content_status,
                cache_rel_path, is_deleted, remote_path, children_cached, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 1, 0, ?7, 0, NULL, ?8, NULL, 0, NULL, 1, ?7)",
            params![
                file_id,
                path,
                parent_path,
                name,
                FileKind::Directory as i32,
                SyncState::QueuedMkdir as i32,
                now,
                ContentStatus::Cached as i32,
            ],
        )?;
        enqueue_operation_tx(
            &tx,
            &file_id,
            OperationType::Mkdir,
            OperationPayload::default(),
        )?;
        tx.commit()?;
        drop(db);
        self.notify_change();
        self.get_entry(&path)
    }

    pub fn delete(&self, path: &str, expect_dir: bool) -> Result<(), SyncError> {
        let node = self.get_entry(path)?;
        match (expect_dir, node.kind) {
            (true, FileKind::File) => return Err(SyncError::NotDir),
            (false, FileKind::Directory) => return Err(SyncError::IsDir),
            _ => {}
        }

        if expect_dir {
            let children = self.list_local_children(path)?;
            if !children.is_empty() {
                return Err(SyncError::DirectoryNotEmpty);
            }
        }

        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let node_row = load_file_row_by_path(&tx, path)?.ok_or(SyncError::NotFound)?;
        mark_subtree_deleted_tx(&tx, path)?;
        clear_pending_non_delete_ops_tx(&tx, &node_row.file_id)?;
        if node_row.remote_path.is_some() {
            tx.execute(
                "UPDATE files SET sync_state = ?2, updated_at = ?3 WHERE file_id = ?1",
                params![node_row.file_id, SyncState::QueuedDelete as i32, unix_now(),],
            )?;
            enqueue_operation_tx(
                &tx,
                &node_row.file_id,
                OperationType::Delete,
                OperationPayload::default(),
            )?;
        } else {
            tx.execute(
                "DELETE FROM operations_queue WHERE file_id = ?1",
                params![node_row.file_id],
            )?;
        }
        tx.commit()?;
        drop(db);
        self.notify_change();
        Ok(())
    }

    pub fn rename(&self, old_path: &str, new_path: &str) -> Result<(), SyncError> {
        if old_path == ROOT_PATH {
            return Err(SyncError::InvalidState(String::from("cannot rename root")));
        }
        if self.get_entry(new_path).is_ok() {
            return Err(SyncError::AlreadyExists);
        }
        let new_parent = parent_path_of(new_path).ok_or_else(|| {
            SyncError::InvalidState(format!("path {new_path} has no parent directory"))
        })?;
        let new_parent_node = self.get_entry(&new_parent)?;
        if new_parent_node.kind != FileKind::Directory {
            return Err(SyncError::NotDir);
        }

        let new_name = basename_of(new_path).to_owned();
        let now = unix_now();
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let row = load_file_row_by_path(&tx, old_path)?.ok_or(SyncError::NotFound)?;
        rename_subtree_tx(&tx, old_path, new_path, &new_parent, &new_name, now)?;

        if row.remote_path.is_some() {
            tx.execute(
                "UPDATE files SET sync_state = ?2, updated_at = ?3 WHERE file_id = ?1",
                params![row.file_id, SyncState::QueuedMove as i32, now],
            )?;
            enqueue_move_tx(
                &tx,
                &row.file_id,
                row.remote_path
                    .clone()
                    .unwrap_or_else(|| old_path.to_owned()),
                new_path.to_owned(),
            )?;
        } else if row.kind == FileKind::File {
            enqueue_upload_tx(&tx, &row.file_id)?;
        }

        tx.commit()?;
        drop(db);
        self.notify_change();
        Ok(())
    }

    pub fn sync_summary_snapshot(&self) -> Result<SyncSummarySnapshot, SyncError> {
        let db = self.db.lock().unwrap();
        let queued_count = count_states(
            &db,
            &[
                SyncState::QueuedUpload,
                SyncState::QueuedDelete,
                SyncState::QueuedMkdir,
                SyncState::QueuedMove,
            ],
        )?;
        let uploading_count = count_states(&db, &[SyncState::Uploading])?;
        let conflict_count = count_states(&db, &[SyncState::Conflict])?;
        let error_count = count_states(&db, &[SyncState::Error])?;
        let runtime = self.runtime.lock().unwrap();
        let downloading_count = runtime.downloads.len() as u32;
        let last_update_unix = latest_update_unix(&db)?.max(runtime.last_update_unix);
        let active_count = queued_count + uploading_count + downloading_count;

        Ok(SyncSummarySnapshot {
            active_count,
            uploading_count,
            downloading_count,
            queued_count,
            conflict_count,
            error_count,
            last_update_unix,
            is_syncing: active_count > 0,
            attention_required: conflict_count > 0 || error_count > 0,
        })
    }

    pub fn sync_items_snapshot(&self) -> Result<Vec<SyncItemSnapshot>, SyncError> {
        let db = self.db.lock().unwrap();
        let mut items = load_sync_items(&db)?;
        let runtime = self.runtime.lock().unwrap();
        for download in runtime.downloads.values() {
            items.push(SyncItemSnapshot {
                path: download.path.clone(),
                state: String::from("downloading"),
                direction: String::from("download"),
                progress: 0,
                bytes_done: 0,
                bytes_total: download.bytes_total,
                updated_at: download.updated_at,
            });
        }
        items.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        items.truncate(MAX_SYNC_ITEMS);
        Ok(items)
    }

    pub fn sync_summary_dbus(&self) -> Result<HashMap<String, OwnedValue>, SyncError> {
        let summary = self.sync_summary_snapshot()?;
        Ok(HashMap::from([
            (
                String::from("active_count"),
                OwnedValue::from(summary.active_count),
            ),
            (
                String::from("uploading_count"),
                OwnedValue::from(summary.uploading_count),
            ),
            (
                String::from("downloading_count"),
                OwnedValue::from(summary.downloading_count),
            ),
            (
                String::from("queued_count"),
                OwnedValue::from(summary.queued_count),
            ),
            (
                String::from("conflict_count"),
                OwnedValue::from(summary.conflict_count),
            ),
            (
                String::from("error_count"),
                OwnedValue::from(summary.error_count),
            ),
            (
                String::from("last_update_unix"),
                OwnedValue::from(summary.last_update_unix),
            ),
            (
                String::from("is_syncing"),
                OwnedValue::from(summary.is_syncing),
            ),
            (
                String::from("attention_required"),
                OwnedValue::from(summary.attention_required),
            ),
        ]))
    }

    pub fn sync_items_dbus(&self) -> Result<Vec<HashMap<String, OwnedValue>>, SyncError> {
        let items = self.sync_items_snapshot()?;
        Ok(items
            .into_iter()
            .map(|item| {
                HashMap::from([
                    (String::from("path"), OwnedValue::from(Str::from(item.path))),
                    (
                        String::from("state"),
                        OwnedValue::from(Str::from(item.state)),
                    ),
                    (
                        String::from("direction"),
                        OwnedValue::from(Str::from(item.direction)),
                    ),
                    (String::from("progress"), OwnedValue::from(item.progress)),
                    (
                        String::from("bytes_done"),
                        OwnedValue::from(item.bytes_done),
                    ),
                    (
                        String::from("bytes_total"),
                        OwnedValue::from(item.bytes_total),
                    ),
                    (
                        String::from("updated_at"),
                        OwnedValue::from(item.updated_at),
                    ),
                ])
            })
            .collect())
    }

    fn list_local_children(&self, path: &str) -> Result<Vec<LocalNode>, SyncError> {
        let db = self.db.lock().unwrap();
        let rows = load_directory_rows(&db, path)?;
        Ok(rows.into_iter().map(FileRow::into_local_node).collect())
    }

    fn apply_local_file_change(&self, file_id: &str, size: u64) -> Result<(), SyncError> {
        let now = unix_now();
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "UPDATE files
             SET size = ?2,
                 mtime = ?3,
                 local_version = local_version + 1,
                 content_status = ?4,
                 sync_state = ?5,
                 updated_at = ?3
             WHERE file_id = ?1",
            params![
                file_id,
                size,
                now,
                ContentStatus::Cached as i32,
                SyncState::QueuedUpload as i32,
            ],
        )?;
        enqueue_upload_tx(&tx, file_id)?;
        tx.commit()?;
        drop(db);
        self.notify_change();
        Ok(())
    }

    fn ensure_root_bootstrapped(&self) -> Result<(), SyncError> {
        if self.has_local_state()? {
            return Ok(());
        }

        let root = match self.client.fetch_resource_metadata(ROOT_PATH) {
            Ok(root) => root,
            Err(YandexError::Unauthorized)
            | Err(YandexError::Forbidden)
            | Err(YandexError::Auth(_))
            | Err(YandexError::NotFound) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let now = unix_now();
        let file_id = new_id("root");
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO files (
                file_id, path, parent_path, name, kind, sync_state, remote_version,
                local_version, synced_local_version, mtime, size, hash, content_status,
                cache_rel_path, is_deleted, remote_path, children_cached, updated_at
            ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, 0, 0, ?7, ?8, NULL, ?9, NULL, 0, ?2, 0, ?7)",
            params![
                file_id,
                root.path,
                root.name,
                FileKind::Directory as i32,
                SyncState::Synced as i32,
                root.remote_version,
                now,
                root.size,
                ContentStatus::Cached as i32,
            ],
        )?;
        self.notify_change();
        Ok(())
    }

    fn ensure_cached(&self, file_id: &str) -> Result<PathBuf, SyncError> {
        let row = {
            let db = self.db.lock().unwrap();
            load_file_row_by_id(&db, file_id)?.ok_or(SyncError::NotFound)?
        };
        if row.kind != FileKind::File {
            return Err(SyncError::IsDir);
        }

        let cache_rel_path = row
            .cache_rel_path
            .clone()
            .unwrap_or_else(|| relative_cache_path(file_id));
        let cache_path = self.cache_dir.join(&cache_rel_path);
        if row.content_status == ContentStatus::Cached && cache_path.exists() {
            return Ok(cache_path);
        }

        let remote_path = row.remote_path.clone().ok_or(SyncError::NotFound)?;
        self.start_download(file_id, &row.path, row.size);
        let download = (|| -> Result<(), SyncError> {
            let href = self.client.resolve_download_url(&remote_path)?;
            let bytes = self.client.download_file(&href)?;
            ensure_parent_dir(&cache_path)?;
            fs::write(&cache_path, &bytes)?;

            let now = unix_now();
            let db = self.db.lock().unwrap();
            db.execute(
                "UPDATE files
                 SET cache_rel_path = ?2,
                     content_status = ?3,
                     size = ?4,
                     sync_state = CASE WHEN sync_state = ?5 THEN ?6 ELSE sync_state END,
                     updated_at = ?7
                 WHERE file_id = ?1",
                params![
                    file_id,
                    cache_rel_path,
                    ContentStatus::Cached as i32,
                    bytes.len() as u64,
                    SyncState::Placeholder as i32,
                    SyncState::Synced as i32,
                    now,
                ],
            )?;
            Ok(())
        })();
        self.finish_download(file_id);
        download?;
        self.notify_change();
        Ok(cache_path)
    }

    fn materialize_local_cache(
        &self,
        file_id: &str,
        initial_size: u64,
    ) -> Result<PathBuf, SyncError> {
        let row = {
            let db = self.db.lock().unwrap();
            load_file_row_by_id(&db, file_id)?.ok_or(SyncError::NotFound)?
        };
        if row.kind != FileKind::File {
            return Err(SyncError::IsDir);
        }

        let cache_rel_path = row
            .cache_rel_path
            .clone()
            .unwrap_or_else(|| relative_cache_path(file_id));
        let cache_path = self.cache_dir.join(&cache_rel_path);
        if !cache_path.exists() {
            ensure_parent_dir(&cache_path)?;
            let file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .open(&cache_path)?;
            file.set_len(initial_size)?;
        }

        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE files
             SET cache_rel_path = ?2,
                 content_status = ?3,
                 updated_at = ?4
             WHERE file_id = ?1",
            params![
                file_id,
                cache_rel_path,
                ContentStatus::Cached as i32,
                unix_now(),
            ],
        )?;
        Ok(cache_path)
    }

    fn process_one_operation(&self) -> Result<bool, SyncError> {
        self.recover_queue_state()?;
        self.reconstruct_missing_jobs()?;
        let job = self.lease_next_operation()?;
        let Some(job) = job else {
            return Ok(false);
        };

        let result = match job.op_type {
            OperationType::Upload => self.execute_upload(&job),
            OperationType::Delete => self.execute_delete(&job),
            OperationType::Mkdir => self.execute_mkdir(&job),
            OperationType::Move | OperationType::Rename => self.execute_move(&job),
        };

        match result {
            Ok(()) => self.mark_operation_done(job.id, &job.file_id)?,
            Err(SyncError::Conflict(message)) => {
                self.mark_operation_conflict(job.id, &job.file_id, &message)?
            }
            Err(err) => self.mark_operation_failed(job.id, &job.file_id, &err)?,
        }

        Ok(true)
    }

    fn execute_upload(&self, job: &QueuedOperation) -> Result<(), SyncError> {
        let row = {
            let db = self.db.lock().unwrap();
            load_file_row_by_id(&db, &job.file_id)?.ok_or(SyncError::NotFound)?
        };
        let path = row.path.clone();
        let cache_path = self.ensure_cached(&row.file_id)?;

        if let Some(remote_path) = row.remote_path.clone() {
            if remote_path != path {
                self.client.move_resource(&remote_path, &path, true)?;
                let db = self.db.lock().unwrap();
                db.execute(
                    "UPDATE files SET remote_path = ?2, updated_at = ?3 WHERE file_id = ?1",
                    params![row.file_id, path, unix_now()],
                )?;
            }
        }

        match self.client.fetch_resource_metadata(&path) {
            Ok(remote) => {
                if row.remote_version.is_none() {
                    return self.handle_conflict(&row, remote.remote_version.clone());
                }
                if remote.remote_version.as_deref() != row.remote_version.as_deref() {
                    return self.handle_conflict(&row, remote.remote_version.clone());
                }
            }
            Err(YandexError::NotFound) => {
                if row.remote_version.is_some() {
                    return self.handle_conflict(&row, None);
                }
            }
            Err(err) => return Err(err.into()),
        }

        let href = self.client.resolve_upload_url(&path, true)?;
        self.client.upload_file(&href, &cache_path)?;
        let fresh = self.client.fetch_resource_metadata(&path)?;
        let now = unix_now();
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE files
             SET remote_version = ?2,
                 remote_path = ?3,
                 sync_state = ?4,
                 synced_local_version = local_version,
                 size = ?5,
                 mtime = ?6,
                 is_deleted = 0,
                 updated_at = ?6
             WHERE file_id = ?1",
            params![
                row.file_id,
                fresh.remote_version,
                fresh.path,
                SyncState::Synced as i32,
                fresh.size,
                now,
            ],
        )?;
        Ok(())
    }

    fn execute_delete(&self, job: &QueuedOperation) -> Result<(), SyncError> {
        let row = {
            let db = self.db.lock().unwrap();
            load_file_row_by_id(&db, &job.file_id)?.ok_or(SyncError::NotFound)?
        };
        if let Some(remote_path) = row.remote_path.as_deref() {
            match self.client.delete_resource(remote_path, true) {
                Ok(()) | Err(YandexError::NotFound) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }

    fn execute_mkdir(&self, job: &QueuedOperation) -> Result<(), SyncError> {
        let row = {
            let db = self.db.lock().unwrap();
            load_file_row_by_id(&db, &job.file_id)?.ok_or(SyncError::NotFound)?
        };
        self.client.create_directory(&row.path)?;
        let fresh = self.client.fetch_resource_metadata(&row.path)?;
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE files
             SET remote_version = ?2,
                 remote_path = ?3,
                 sync_state = ?4,
                 synced_local_version = local_version,
                 updated_at = ?5
             WHERE file_id = ?1",
            params![
                row.file_id,
                fresh.remote_version,
                fresh.path,
                SyncState::Synced as i32,
                unix_now(),
            ],
        )?;
        Ok(())
    }

    fn execute_move(&self, job: &QueuedOperation) -> Result<(), SyncError> {
        let row = {
            let db = self.db.lock().unwrap();
            load_file_row_by_id(&db, &job.file_id)?.ok_or(SyncError::NotFound)?
        };
        let from = job
            .payload
            .from_path
            .clone()
            .or_else(|| row.remote_path.clone())
            .ok_or_else(|| SyncError::InvalidState(String::from("move job missing from path")))?;
        let to = job
            .payload
            .to_path
            .clone()
            .unwrap_or_else(|| row.path.clone());

        self.client.move_resource(&from, &to, true)?;
        let fresh = self.client.fetch_resource_metadata(&to)?;
        let next_state =
            if row.kind == FileKind::File && row.local_version > row.synced_local_version {
                SyncState::QueuedUpload
            } else {
                SyncState::Synced
            };
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE files
             SET remote_path = ?2,
                 remote_version = ?3,
                 sync_state = ?4,
                 updated_at = ?5
             WHERE file_id = ?1",
            params![
                row.file_id,
                to,
                fresh.remote_version,
                next_state as i32,
                unix_now()
            ],
        )?;
        Ok(())
    }

    fn handle_conflict(
        &self,
        row: &FileRow,
        current_remote_version: Option<String>,
    ) -> Result<(), SyncError> {
        let conflict_path = self.next_conflict_path(&row.path)?;
        let conflict_parent = parent_path_of(&conflict_path)
            .ok_or_else(|| SyncError::InvalidState(String::from("conflict path missing parent")))?;
        let conflict_name = basename_of(&conflict_path).to_owned();
        let now = unix_now();
        let conflict_id = new_id("conflict");

        let current_remote = self.client.fetch_resource_metadata(&row.path)?;

        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO conflicts (
                conflict_id, file_id, original_path, conflict_path, created_at,
                base_remote_version, current_remote_version, origin_device
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
            params![
                conflict_id,
                row.file_id,
                row.path,
                conflict_path,
                now,
                row.remote_version,
                current_remote_version,
            ],
        )?;
        tx.execute(
            "UPDATE files
             SET path = ?2,
                 parent_path = ?3,
                 name = ?4,
                 remote_path = NULL,
                 remote_version = NULL,
                 synced_local_version = 0,
                 sync_state = ?5,
                 updated_at = ?6
             WHERE file_id = ?1",
            params![
                row.file_id,
                conflict_path,
                conflict_parent,
                conflict_name,
                SyncState::Conflict as i32,
                now,
            ],
        )?;
        let remote_path = current_remote.path.clone();
        upsert_remote_entry_tx(
            &tx,
            &remote_path,
            current_remote,
            Some(row.parent_path.as_deref().unwrap_or(ROOT_PATH)),
        )?;
        enqueue_upload_tx(&tx, &row.file_id)?;
        tx.commit()?;
        self.notify_change();
        Err(SyncError::Conflict(String::from(
            "remote version changed before upload",
        )))
    }

    fn next_conflict_path(&self, path: &str) -> Result<String, SyncError> {
        let db = self.db.lock().unwrap();
        let mut candidate = increment_conflict_name(path);
        while load_file_row_by_path(&db, &candidate)?.is_some() {
            candidate = increment_conflict_name(&candidate);
        }
        Ok(candidate)
    }

    fn recover_queue_state(&self) -> Result<(), SyncError> {
        let now = unix_now();
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE operations_queue
             SET op_status = ?1,
                 worker_id = NULL,
                 lease_expires_at = NULL,
                 updated_at = ?2
             WHERE op_status = ?3 AND lease_expires_at IS NOT NULL AND lease_expires_at <= ?2",
            params![
                OperationStatus::Pending as i32,
                now,
                OperationStatus::Leased as i32,
            ],
        )?;
        db.execute(
            "UPDATE files
             SET sync_state = CASE
                 WHEN sync_state = ?1 THEN ?2
                 WHEN sync_state = ?3 THEN ?4
                 ELSE sync_state
             END,
             updated_at = ?5",
            params![
                SyncState::Uploading as i32,
                SyncState::QueuedUpload as i32,
                SyncState::Downloading as i32,
                SyncState::Placeholder as i32,
                now,
            ],
        )?;
        Ok(())
    }

    fn reconstruct_missing_jobs(&self) -> Result<(), SyncError> {
        let db = self.db.lock().unwrap();
        let rows = load_unsynced_rows(&db)?;
        drop(db);

        for row in rows {
            let mut db = self.db.lock().unwrap();
            let tx = db.transaction()?;
            let exists = tx
                .query_row(
                    "SELECT 1 FROM operations_queue
                 WHERE file_id = ?1 AND op_status IN (?2, ?3, ?4)
                 LIMIT 1",
                    params![
                        row.file_id,
                        OperationStatus::Pending as i32,
                        OperationStatus::Leased as i32,
                        OperationStatus::RetryableError as i32,
                    ],
                    |_| Ok(()),
                )
                .optional()?;
            if exists.is_some() {
                continue;
            }

            match row.sync_state {
                SyncState::QueuedUpload => enqueue_upload_tx(&tx, &row.file_id)?,
                SyncState::QueuedDelete => enqueue_operation_tx(
                    &tx,
                    &row.file_id,
                    OperationType::Delete,
                    OperationPayload::default(),
                )?,
                SyncState::QueuedMkdir => enqueue_operation_tx(
                    &tx,
                    &row.file_id,
                    OperationType::Mkdir,
                    OperationPayload::default(),
                )?,
                SyncState::QueuedMove => {
                    if let Some(from_path) = row.remote_path.clone() {
                        enqueue_move_tx(&tx, &row.file_id, from_path, row.path.clone())?;
                    }
                }
                _ => {}
            }
            tx.commit()?;
        }
        Ok(())
    }

    fn lease_next_operation(&self) -> Result<Option<QueuedOperation>, SyncError> {
        let now = unix_now();
        let worker_id = new_id("worker");
        let lease_expires = now + WORKER_LEASE_SECONDS;
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        let op = tx
            .query_row(
                "SELECT id, file_id, op_type, payload_json
                 FROM operations_queue
                 WHERE op_status IN (?1, ?2)
                   AND (next_retry_at IS NULL OR next_retry_at <= ?3)
                 ORDER BY created_at ASC, id ASC
                 LIMIT 1",
                params![
                    OperationStatus::Pending as i32,
                    OperationStatus::RetryableError as i32,
                    now,
                ],
                |row| {
                    let payload = row
                        .get::<_, Option<String>>(3)?
                        .map(|raw| serde_json::from_str::<OperationPayload>(&raw))
                        .transpose()
                        .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?
                        .unwrap_or_default();
                    Ok(QueuedOperation {
                        id: row.get(0)?,
                        file_id: row.get(1)?,
                        op_type: OperationType::try_from(row.get::<_, i32>(2)?).map_err(|err| {
                            rusqlite::Error::ToSqlConversionFailure(Box::new(err))
                        })?,
                        payload,
                    })
                },
            )
            .optional()?;

        let Some(op) = op else {
            tx.commit()?;
            return Ok(None);
        };

        tx.execute(
            "UPDATE operations_queue
             SET op_status = ?2,
                 worker_id = ?3,
                 lease_expires_at = ?4,
                 updated_at = ?5
             WHERE id = ?1",
            params![
                op.id,
                OperationStatus::Leased as i32,
                worker_id,
                lease_expires,
                now,
            ],
        )?;
        let sync_state = match op.op_type {
            OperationType::Upload => SyncState::Uploading,
            OperationType::Delete => SyncState::QueuedDelete,
            OperationType::Mkdir => SyncState::QueuedMkdir,
            OperationType::Move | OperationType::Rename => SyncState::QueuedMove,
        };
        tx.execute(
            "UPDATE files SET sync_state = ?2, updated_at = ?3 WHERE file_id = ?1 AND is_deleted = 0",
            params![op.file_id, sync_state as i32, now],
        )?;
        tx.commit()?;
        self.notify_change();
        Ok(Some(op))
    }

    fn mark_operation_done(&self, op_id: i64, file_id: &str) -> Result<(), SyncError> {
        let now = unix_now();
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE operations_queue
             SET op_status = ?2,
                 worker_id = NULL,
                 lease_expires_at = NULL,
                 updated_at = ?3
             WHERE id = ?1",
            params![op_id, OperationStatus::Done as i32, now],
        )?;
        db.execute(
            "UPDATE files
             SET sync_state = CASE WHEN is_deleted = 0 THEN ?2 ELSE sync_state END,
                 updated_at = ?3
             WHERE file_id = ?1",
            params![file_id, SyncState::Synced as i32, now],
        )?;
        self.notify_change();
        Ok(())
    }

    fn mark_operation_conflict(
        &self,
        op_id: i64,
        file_id: &str,
        _message: &str,
    ) -> Result<(), SyncError> {
        let now = unix_now();
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE operations_queue
             SET op_status = ?2,
                 worker_id = NULL,
                 lease_expires_at = NULL,
                 updated_at = ?3
             WHERE id = ?1",
            params![op_id, OperationStatus::Conflict as i32, now],
        )?;
        db.execute(
            "UPDATE files SET sync_state = ?2, updated_at = ?3 WHERE file_id = ?1",
            params![file_id, SyncState::Conflict as i32, now],
        )?;
        self.notify_change();
        Ok(())
    }

    fn mark_operation_failed(
        &self,
        op_id: i64,
        file_id: &str,
        err: &SyncError,
    ) -> Result<(), SyncError> {
        let now = unix_now();
        let retryable = is_retryable(err);
        let next_retry_at = if retryable { Some(now + 2) } else { None };
        let status = if retryable {
            OperationStatus::RetryableError
        } else {
            OperationStatus::PermanentError
        };
        let sync_state = if retryable {
            SyncState::QueuedUpload
        } else {
            SyncState::Error
        };
        let db = self.db.lock().unwrap();
        db.execute(
            "UPDATE operations_queue
             SET op_status = ?2,
                 retry_count = retry_count + 1,
                 next_retry_at = ?3,
                 worker_id = NULL,
                 lease_expires_at = NULL,
                 updated_at = ?4
             WHERE id = ?1",
            params![op_id, status as i32, next_retry_at, now],
        )?;
        db.execute(
            "UPDATE files SET sync_state = ?2, updated_at = ?3 WHERE file_id = ?1",
            params![file_id, sync_state as i32, now],
        )?;
        self.notify_change();
        Ok(())
    }

    fn start_download(&self, file_id: &str, path: &str, bytes_total: u64) {
        let mut runtime = self.runtime.lock().unwrap();
        runtime.downloads.insert(
            file_id.to_owned(),
            ActiveTransfer {
                path: path.to_owned(),
                bytes_total,
                updated_at: unix_now(),
            },
        );
        runtime.last_update_unix = unix_now();
        drop(runtime);
        self.notify_change();
    }

    fn finish_download(&self, file_id: &str) {
        let mut runtime = self.runtime.lock().unwrap();
        runtime.downloads.remove(file_id);
        runtime.last_update_unix = unix_now();
        drop(runtime);
        self.notify_change();
    }

    fn notify_change(&self) {
        let _ = self.change_tx.send_modify(|value| *value += 1);
    }
}

fn default_state_root() -> PathBuf {
    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(path);
    }
    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home).join(".local/state");
    }
    env::temp_dir()
}

fn apply_migrations(db: &mut Connection) -> Result<(), SyncError> {
    db.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS files (
             file_id TEXT PRIMARY KEY,
             path TEXT NOT NULL UNIQUE,
             parent_path TEXT,
             name TEXT NOT NULL,
             kind INTEGER NOT NULL CHECK (kind IN (0, 1)),
             sync_state INTEGER NOT NULL CHECK (sync_state IN (0, 1, 2, 3, 4, 5, 6, 7, 8, 9)),
             remote_version TEXT,
             local_version INTEGER NOT NULL DEFAULT 0,
             synced_local_version INTEGER NOT NULL DEFAULT 0,
             mtime INTEGER,
             size INTEGER NOT NULL DEFAULT 0,
             hash BLOB,
             content_status INTEGER NOT NULL CHECK (content_status IN (0, 1)),
             cache_rel_path TEXT,
             is_deleted INTEGER NOT NULL DEFAULT 0 CHECK (is_deleted IN (0, 1)),
             remote_path TEXT,
             children_cached INTEGER NOT NULL DEFAULT 0 CHECK (children_cached IN (0, 1)),
             updated_at INTEGER NOT NULL DEFAULT 0
         );
         CREATE TABLE IF NOT EXISTS operations_queue (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             file_id TEXT NOT NULL,
             op_type INTEGER NOT NULL CHECK (op_type IN (0, 1, 2, 3, 4)),
             op_status INTEGER NOT NULL CHECK (op_status IN (0, 1, 2, 3, 4, 5)),
             payload_json TEXT,
             retry_count INTEGER NOT NULL DEFAULT 0,
             next_retry_at INTEGER,
             worker_id TEXT,
             lease_expires_at INTEGER,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             FOREIGN KEY(file_id) REFERENCES files(file_id)
         );
         CREATE TABLE IF NOT EXISTS conflicts (
             conflict_id TEXT PRIMARY KEY,
             file_id TEXT NOT NULL,
             original_path TEXT NOT NULL,
             conflict_path TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             base_remote_version TEXT,
             current_remote_version TEXT,
             origin_device TEXT,
             FOREIGN KEY(file_id) REFERENCES files(file_id)
         );
         CREATE INDEX IF NOT EXISTS idx_files_parent_path ON files(parent_path, is_deleted, name);
         CREATE INDEX IF NOT EXISTS idx_files_sync_state ON files(sync_state, is_deleted);
         CREATE INDEX IF NOT EXISTS idx_queue_pending ON operations_queue(op_status, next_retry_at, created_at);
         CREATE INDEX IF NOT EXISTS idx_queue_file ON operations_queue(file_id, op_status);
         CREATE INDEX IF NOT EXISTS idx_conflicts_file ON conflicts(file_id, created_at);",
    )?;
    Ok(())
}

fn load_file_row_by_path(conn: &Connection, path: &str) -> Result<Option<FileRow>, SyncError> {
    conn.query_row(
        "SELECT file_id, path, parent_path, name, kind, sync_state, remote_version,
                local_version, synced_local_version, COALESCE(mtime, 0), size,
                content_status, cache_rel_path, is_deleted, remote_path,
                children_cached, updated_at
         FROM files
         WHERE path = ?1 AND is_deleted = 0",
        params![path],
        file_row_from_row,
    )
    .optional()
    .map_err(SyncError::from)
}

fn load_file_row_by_id(conn: &Connection, file_id: &str) -> Result<Option<FileRow>, SyncError> {
    conn.query_row(
        "SELECT file_id, path, parent_path, name, kind, sync_state, remote_version,
                local_version, synced_local_version, COALESCE(mtime, 0), size,
                content_status, cache_rel_path, is_deleted, remote_path,
                children_cached, updated_at
         FROM files
         WHERE file_id = ?1",
        params![file_id],
        file_row_from_row,
    )
    .optional()
    .map_err(SyncError::from)
}

fn load_directory_rows(conn: &Connection, parent_path: &str) -> Result<Vec<FileRow>, SyncError> {
    let mut stmt = conn.prepare(
        "SELECT file_id, path, parent_path, name, kind, sync_state, remote_version,
                local_version, synced_local_version, COALESCE(mtime, 0), size,
                content_status, cache_rel_path, is_deleted, remote_path,
                children_cached, updated_at
         FROM files
         WHERE parent_path = ?1 AND is_deleted = 0
         ORDER BY name ASC",
    )?;
    let rows = stmt.query_map(params![parent_path], file_row_from_row)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(SyncError::from)
}

fn has_local_children(conn: &Connection, parent_path: &str) -> Result<bool, SyncError> {
    let exists: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM files WHERE parent_path = ?1 AND is_deleted = 0 LIMIT 1",
            params![parent_path],
            |row| row.get(0),
        )
        .optional()?;
    Ok(exists.is_some())
}

fn load_unsynced_rows(conn: &Connection) -> Result<Vec<FileRow>, SyncError> {
    let mut stmt = conn.prepare(
        "SELECT file_id, path, parent_path, name, kind, sync_state, remote_version,
                local_version, synced_local_version, COALESCE(mtime, 0), size,
                content_status, cache_rel_path, is_deleted, remote_path,
                children_cached, updated_at
         FROM files
         WHERE sync_state IN (?1, ?2, ?3, ?4)",
    )?;
    let rows = stmt.query_map(
        params![
            SyncState::QueuedUpload as i32,
            SyncState::QueuedDelete as i32,
            SyncState::QueuedMkdir as i32,
            SyncState::QueuedMove as i32,
        ],
        file_row_from_row,
    )?;
    rows.collect::<Result<Vec<_>, _>>().map_err(SyncError::from)
}

fn file_row_from_row(row: &rusqlite::Row<'_>) -> Result<FileRow, rusqlite::Error> {
    Ok(FileRow {
        file_id: row.get(0)?,
        path: row.get(1)?,
        parent_path: row.get(2)?,
        name: row.get(3)?,
        kind: FileKind::try_from(row.get::<_, i32>(4)?)
            .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?,
        sync_state: SyncState::try_from(row.get::<_, i32>(5)?)
            .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?,
        remote_version: row.get(6)?,
        local_version: row.get(7)?,
        synced_local_version: row.get(8)?,
        mtime: row.get(9)?,
        size: row.get::<_, u64>(10)?,
        content_status: ContentStatus::try_from(row.get::<_, i32>(11)?)
            .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?,
        cache_rel_path: row.get(12)?,
        is_deleted: row.get::<_, i64>(13)? != 0,
        remote_path: row.get(14)?,
        children_cached: row.get::<_, i64>(15)? != 0,
        updated_at: row.get(16)?,
    })
}

impl FileRow {
    fn into_local_node(self) -> LocalNode {
        LocalNode {
            file_id: self.file_id,
            path: self.path,
            parent_path: self.parent_path,
            name: self.name,
            kind: self.kind,
            size: self.size,
            mtime: unix_to_system_time(self.mtime),
            sync_state: self.sync_state,
            remote_version: self.remote_version,
            remote_path: self.remote_path,
            local_version: self.local_version,
            synced_local_version: self.synced_local_version,
            content_status: self.content_status,
            cache_rel_path: self.cache_rel_path,
            is_deleted: self.is_deleted,
        }
    }
}

fn refresh_directory_from_remote(
    tx: &Transaction<'_>,
    _parent_id: &str,
    parent_path: &str,
    children: Vec<ResourceEntry>,
) -> Result<(), SyncError> {
    let mut seen_paths = HashSet::new();
    for child in children {
        let child_path = child.path.clone();
        seen_paths.insert(child_path.clone());
        upsert_remote_entry_tx(tx, &child_path, child, Some(parent_path))?;
    }

    let existing = load_directory_rows(tx, parent_path)?;
    for row in existing {
        if seen_paths.contains(&row.path) {
            continue;
        }
        if row.sync_state == SyncState::Synced {
            tx.execute(
                "UPDATE files SET is_deleted = 1, updated_at = ?2 WHERE file_id = ?1",
                params![row.file_id, unix_now()],
            )?;
        }
    }
    tx.execute(
        "UPDATE files SET children_cached = 1, updated_at = ?2 WHERE path = ?1",
        params![parent_path, unix_now()],
    )?;
    Ok(())
}

fn upsert_remote_entry_tx(
    tx: &Transaction<'_>,
    path: &str,
    entry: ResourceEntry,
    parent_hint: Option<&str>,
) -> Result<(), SyncError> {
    let now = unix_now();
    if let Some(existing) = load_file_row_by_path(tx, path)? {
        tx.execute(
            "UPDATE files
             SET parent_path = ?2,
                 name = ?3,
                 kind = ?4,
                 sync_state = CASE WHEN sync_state IN (?5, ?6, ?7, ?8) THEN sync_state ELSE ?9 END,
                 remote_version = ?10,
                 size = ?11,
                 mtime = ?12,
                 remote_path = ?1,
                 is_deleted = 0,
                 updated_at = ?12
             WHERE file_id = ?13",
            params![
                path,
                parent_hint,
                entry.name,
                match entry.kind {
                    ResourceKind::Directory => FileKind::Directory as i32,
                    ResourceKind::File => FileKind::File as i32,
                },
                SyncState::QueuedUpload as i32,
                SyncState::QueuedDelete as i32,
                SyncState::QueuedMkdir as i32,
                SyncState::QueuedMove as i32,
                SyncState::Synced as i32,
                entry.remote_version,
                entry.size,
                now,
                existing.file_id,
            ],
        )?;
    } else {
        let file_id = new_id("remote");
        tx.execute(
            "INSERT INTO files (
                file_id, path, parent_path, name, kind, sync_state, remote_version,
                local_version, synced_local_version, mtime, size, hash, content_status,
                cache_rel_path, is_deleted, remote_path, children_cached, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0, ?8, ?9, NULL, ?10, NULL, 0, ?2, 0, ?8)",
            params![
                file_id,
                path,
                parent_hint,
                entry.name,
                match entry.kind {
                    ResourceKind::Directory => FileKind::Directory as i32,
                    ResourceKind::File => FileKind::File as i32,
                },
                if entry.kind == ResourceKind::Directory {
                    SyncState::Synced as i32
                } else {
                    SyncState::Placeholder as i32
                },
                entry.remote_version,
                now,
                entry.size,
                if entry.kind == ResourceKind::Directory {
                    ContentStatus::Cached as i32
                } else {
                    ContentStatus::Missing as i32
                },
            ],
        )?;
    }
    Ok(())
}

fn enqueue_upload_tx(tx: &Transaction<'_>, file_id: &str) -> Result<(), SyncError> {
    let existing = find_pending_operation(tx, file_id, OperationType::Upload)?;
    if existing.is_some() {
        tx.execute(
            "UPDATE operations_queue
             SET op_status = ?2,
                 next_retry_at = NULL,
                 updated_at = ?3
             WHERE file_id = ?1 AND op_type = ?4 AND op_status IN (?2, ?5)",
            params![
                file_id,
                OperationStatus::Pending as i32,
                unix_now(),
                OperationType::Upload as i32,
                OperationStatus::RetryableError as i32,
            ],
        )?;
        return Ok(());
    }
    enqueue_operation_tx(
        tx,
        file_id,
        OperationType::Upload,
        OperationPayload::default(),
    )
}

fn enqueue_move_tx(
    tx: &Transaction<'_>,
    file_id: &str,
    from_path: String,
    to_path: String,
) -> Result<(), SyncError> {
    let payload = OperationPayload {
        from_path: Some(from_path.clone()),
        to_path: Some(to_path),
    };
    let existing = find_pending_operation(tx, file_id, OperationType::Move)?;
    if let Some(id) = existing {
        tx.execute(
            "UPDATE operations_queue
             SET payload_json = ?2,
                 op_status = ?3,
                 next_retry_at = NULL,
                 updated_at = ?4
             WHERE id = ?1",
            params![
                id,
                serde_json::to_string(&payload).unwrap_or_default(),
                OperationStatus::Pending as i32,
                unix_now(),
            ],
        )?;
        return Ok(());
    }
    enqueue_operation_tx(tx, file_id, OperationType::Move, payload)
}

fn enqueue_operation_tx(
    tx: &Transaction<'_>,
    file_id: &str,
    op_type: OperationType,
    payload: OperationPayload,
) -> Result<(), SyncError> {
    let now = unix_now();
    tx.execute(
        "INSERT INTO operations_queue (
            file_id, op_type, op_status, payload_json, retry_count,
            next_retry_at, worker_id, lease_expires_at, created_at, updated_at
        ) VALUES (?1, ?2, ?3, ?4, 0, NULL, NULL, NULL, ?5, ?5)",
        params![
            file_id,
            op_type as i32,
            OperationStatus::Pending as i32,
            serde_json::to_string(&payload).unwrap_or_default(),
            now,
        ],
    )?;
    Ok(())
}

fn clear_pending_non_delete_ops_tx(tx: &Transaction<'_>, file_id: &str) -> Result<(), SyncError> {
    tx.execute(
        "DELETE FROM operations_queue
         WHERE file_id = ?1
           AND op_type != ?2
           AND op_status IN (?3, ?4)",
        params![
            file_id,
            OperationType::Delete as i32,
            OperationStatus::Pending as i32,
            OperationStatus::RetryableError as i32,
        ],
    )?;
    Ok(())
}

fn find_pending_operation(
    tx: &Transaction<'_>,
    file_id: &str,
    op_type: OperationType,
) -> Result<Option<i64>, SyncError> {
    tx.query_row(
        "SELECT id FROM operations_queue
         WHERE file_id = ?1 AND op_type = ?2 AND op_status IN (?3, ?4)
         ORDER BY created_at ASC LIMIT 1",
        params![
            file_id,
            op_type as i32,
            OperationStatus::Pending as i32,
            OperationStatus::RetryableError as i32,
        ],
        |row| row.get(0),
    )
    .optional()
    .map_err(SyncError::from)
}

fn rename_subtree_tx(
    tx: &Transaction<'_>,
    old_path: &str,
    new_path: &str,
    new_parent: &str,
    new_name: &str,
    now: i64,
) -> Result<(), SyncError> {
    let mut stmt = tx.prepare(
        "SELECT file_id, path, parent_path FROM files
         WHERE (path = ?1 OR path LIKE ?2) AND is_deleted = 0",
    )?;
    let rows = stmt.query_map(params![old_path, format!("{old_path}/%")], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
        ))
    })?;

    let rows = rows.collect::<Result<Vec<_>, _>>()?;
    for (file_id, path, parent_path) in rows {
        let suffix = path.strip_prefix(old_path).unwrap_or("");
        let next_path = format!("{new_path}{suffix}");
        let next_parent = if path == old_path {
            Some(new_parent.to_owned())
        } else {
            parent_path.map(|parent| parent.replacen(old_path, new_path, 1))
        };
        let next_name = if path == old_path {
            new_name.to_owned()
        } else {
            basename_of(&next_path).to_owned()
        };
        tx.execute(
            "UPDATE files
             SET path = ?2,
                 parent_path = ?3,
                 name = ?4,
                 updated_at = ?5
             WHERE file_id = ?1",
            params![file_id, next_path, next_parent, next_name, now],
        )?;
    }
    Ok(())
}

fn mark_subtree_deleted_tx(tx: &Transaction<'_>, path: &str) -> Result<(), SyncError> {
    tx.execute(
        "UPDATE files SET is_deleted = 1, updated_at = ?3 WHERE path = ?1 OR path LIKE ?2",
        params![path, format!("{path}/%"), unix_now()],
    )?;
    Ok(())
}

fn count_states(conn: &Connection, states: &[SyncState]) -> Result<u32, SyncError> {
    let values: Vec<i32> = states.iter().map(|state| *state as i32).collect();
    let mut sql =
        String::from("SELECT COUNT(*) FROM files WHERE is_deleted = 0 AND sync_state IN (");
    for (index, _) in values.iter().enumerate() {
        if index > 0 {
            sql.push(',');
        }
        sql.push('?');
        sql.push_str(&(index + 1).to_string());
    }
    sql.push(')');
    let mut stmt = conn.prepare(&sql)?;
    stmt.query_row(rusqlite::params_from_iter(values), |row| row.get(0))
        .map_err(SyncError::from)
}

fn latest_update_unix(conn: &Connection) -> Result<i64, SyncError> {
    let files_update: Option<i64> =
        conn.query_row("SELECT MAX(updated_at) FROM files", [], |row| row.get(0))?;
    let queue_update: Option<i64> =
        conn.query_row("SELECT MAX(updated_at) FROM operations_queue", [], |row| {
            row.get(0)
        })?;
    Ok(files_update.unwrap_or(0).max(queue_update.unwrap_or(0)))
}

fn load_sync_items(conn: &Connection) -> Result<Vec<SyncItemSnapshot>, SyncError> {
    let mut stmt = conn.prepare(
        "SELECT path, sync_state, size, updated_at
         FROM files
         WHERE (is_deleted = 0 AND sync_state IN (?1, ?2, ?3, ?4, ?5, ?6))
            OR sync_state IN (?4, ?5)
         ORDER BY updated_at DESC
         LIMIT ?7",
    )?;
    let rows = stmt.query_map(
        params![
            SyncState::QueuedUpload as i32,
            SyncState::QueuedDelete as i32,
            SyncState::QueuedMkdir as i32,
            SyncState::Uploading as i32,
            SyncState::Conflict as i32,
            SyncState::Error as i32,
            MAX_SYNC_ITEMS as i64,
        ],
        |row| {
            let sync_state = SyncState::try_from(row.get::<_, i32>(1)?)
                .map_err(|err| rusqlite::Error::ToSqlConversionFailure(Box::new(err)))?;
            let (state, direction) = match sync_state {
                SyncState::Uploading => ("uploading", "upload"),
                SyncState::Conflict => ("conflict", "upload"),
                SyncState::Error => ("error", "upload"),
                _ => ("queued", "upload"),
            };
            let size = row.get::<_, u64>(2)?;
            Ok(SyncItemSnapshot {
                path: row.get(0)?,
                state: state.to_owned(),
                direction: direction.to_owned(),
                progress: if sync_state == SyncState::Uploading {
                    0
                } else {
                    100
                },
                bytes_done: if sync_state == SyncState::Uploading {
                    0
                } else {
                    size
                },
                bytes_total: size,
                updated_at: row.get(3)?,
            })
        },
    )?;
    rows.collect::<Result<Vec<_>, _>>().map_err(SyncError::from)
}

fn ensure_parent_dir(path: &Path) -> Result<(), SyncError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn relative_cache_path(file_id: &str) -> String {
    format!("{file_id}.bin")
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{:032x}", random::<u128>())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn unix_to_system_time(value: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(value.max(0) as u64)
}

fn join_remote_path(parent: &str, name: &str) -> String {
    if parent == ROOT_PATH {
        format!("{ROOT_PATH}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn parent_path_of(path: &str) -> Option<String> {
    if path == ROOT_PATH {
        return None;
    }
    let trimmed = path.strip_prefix(ROOT_PATH)?;
    if let Some((parent, _)) = trimmed.rsplit_once('/') {
        Some(if parent.is_empty() {
            ROOT_PATH.to_owned()
        } else {
            format!("{ROOT_PATH}{parent}")
        })
    } else {
        Some(ROOT_PATH.to_owned())
    }
}

fn basename_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn split_name_and_extension(name: &str) -> (&str, &str) {
    if let Some((stem, ext)) = name.rsplit_once('.') {
        if stem.is_empty() {
            return (name, "");
        }
        return (stem, ext);
    }
    (name, "")
}

pub fn increment_conflict_name(path: &str) -> String {
    let parent = parent_path_of(path).unwrap_or_else(|| ROOT_PATH.to_owned());
    let name = basename_of(path);
    let (stem, ext) = split_name_and_extension(name);
    let (base, next_index) = parse_conflict_suffix(stem);
    let next_name = if ext.is_empty() {
        format!("{base} ({next_index})")
    } else {
        format!("{base} ({next_index}).{ext}")
    };
    join_remote_path(&parent, &next_name)
}

fn parse_conflict_suffix(stem: &str) -> (&str, u32) {
    if let Some(prefix) = stem.strip_suffix(')') {
        if let Some((base, number)) = prefix.rsplit_once(" (") {
            if let Ok(parsed) = number.parse::<u32>() {
                return (base, parsed + 1);
            }
        }
    }
    (stem, 2)
}

fn read_local_range(path: &Path, offset: u64, size: u32) -> Result<Vec<u8>, SyncError> {
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut file = fs::OpenOptions::new().read(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; size as usize];
    let read = file.read(&mut buf)?;
    buf.truncate(read);
    Ok(buf)
}

fn write_local_range(path: &Path, offset: u64, data: &[u8]) -> Result<(), SyncError> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)?;
    file.sync_data()?;
    Ok(())
}

fn is_retryable(err: &SyncError) -> bool {
    match err {
        SyncError::Remote(YandexError::Unauthorized)
        | SyncError::Remote(YandexError::Forbidden)
        | SyncError::Remote(YandexError::Auth(_))
        | SyncError::Remote(YandexError::Http(_)) => true,
        SyncError::Remote(YandexError::Status { status, .. }) => status.is_server_error(),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, sync::Mutex, thread};

    #[derive(Default)]
    struct FakeRemote {
        resources: Mutex<HashMap<String, ResourceEntry>>,
        contents: Mutex<HashMap<String, Vec<u8>>>,
        fail_downloads: Mutex<bool>,
        offline: Mutex<bool>,
    }

    impl FakeRemote {
        fn with_fixture() -> Arc<Self> {
            let remote = Arc::new(Self::default());
            remote.insert(dir(ROOT_PATH, "disk", None));
            remote.insert(file(
                "disk:/report.txt",
                "report.txt",
                b"hello",
                Some("rev-1"),
            ));
            remote
        }

        fn insert(&self, entry: ResourceEntry) {
            if entry.kind == ResourceKind::File {
                self.contents
                    .lock()
                    .unwrap()
                    .insert(entry.path.clone(), vec![0; entry.size as usize]);
            }
            self.resources
                .lock()
                .unwrap()
                .insert(entry.path.clone(), entry);
        }

        fn set_bytes(&self, path: &str, bytes: &[u8]) {
            self.contents
                .lock()
                .unwrap()
                .insert(path.to_owned(), bytes.to_vec());
            if let Some(entry) = self.resources.lock().unwrap().get_mut(path) {
                entry.size = bytes.len() as u64;
            }
        }

        fn set_offline(&self, offline: bool) {
            *self.offline.lock().unwrap() = offline;
        }

        fn ensure_online(&self) -> Result<(), YandexError> {
            if *self.offline.lock().unwrap() {
                return Err(YandexError::Forbidden);
            }
            Ok(())
        }
    }

    impl RemoteSyncClient for FakeRemote {
        fn fetch_resource_metadata(&self, path: &str) -> Result<ResourceEntry, YandexError> {
            self.ensure_online()?;
            self.resources
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or(YandexError::NotFound)
        }

        fn list_directory(&self, path: &str) -> Result<Vec<ResourceEntry>, YandexError> {
            self.ensure_online()?;
            let resources = self.resources.lock().unwrap();
            let prefix = if path == ROOT_PATH {
                ROOT_PATH.to_owned()
            } else {
                format!("{path}/")
            };
            let mut out = Vec::new();
            for entry in resources.values() {
                if entry.path == path || !entry.path.starts_with(&prefix) {
                    continue;
                }
                let rest = &entry.path[prefix.len()..];
                if !rest.is_empty() && !rest.contains('/') {
                    out.push(entry.clone());
                }
            }
            out.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(out)
        }

        fn create_directory(&self, path: &str) -> Result<(), YandexError> {
            self.ensure_online()?;
            let name = basename_of(path).to_owned();
            self.resources
                .lock()
                .unwrap()
                .insert(path.to_owned(), dir(path, &name, Some("dir-rev")));
            Ok(())
        }

        fn delete_resource(&self, path: &str, _permanently: bool) -> Result<(), YandexError> {
            self.ensure_online()?;
            self.resources.lock().unwrap().remove(path);
            self.contents.lock().unwrap().remove(path);
            Ok(())
        }

        fn move_resource(&self, from: &str, to: &str, _overwrite: bool) -> Result<(), YandexError> {
            self.ensure_online()?;
            let mut resources = self.resources.lock().unwrap();
            let mut entry = resources.remove(from).ok_or(YandexError::NotFound)?;
            entry.path = to.to_owned();
            entry.name = basename_of(to).to_owned();
            resources.insert(to.to_owned(), entry);
            let bytes = self
                .contents
                .lock()
                .unwrap()
                .remove(from)
                .unwrap_or_default();
            self.contents.lock().unwrap().insert(to.to_owned(), bytes);
            Ok(())
        }

        fn resolve_download_url(&self, path: &str) -> Result<String, YandexError> {
            self.ensure_online()?;
            Ok(path.to_owned())
        }

        fn resolve_upload_url(&self, path: &str, _overwrite: bool) -> Result<String, YandexError> {
            self.ensure_online()?;
            Ok(path.to_owned())
        }

        fn upload_file(&self, href: &str, local_path: &Path) -> Result<(), YandexError> {
            self.ensure_online()?;
            let bytes = fs::read(local_path)?;
            self.set_bytes(href, &bytes);
            let name = basename_of(href).to_owned();
            self.resources.lock().unwrap().insert(
                href.to_owned(),
                ResourceEntry {
                    path: href.to_owned(),
                    name,
                    kind: ResourceKind::File,
                    size: bytes.len() as u64,
                    created: Some(SystemTime::UNIX_EPOCH),
                    modified: Some(SystemTime::UNIX_EPOCH),
                    remote_version: Some(format!("rev-{}", bytes.len())),
                },
            );
            Ok(())
        }

        fn download_file(&self, href: &str) -> Result<Vec<u8>, YandexError> {
            self.ensure_online()?;
            if *self.fail_downloads.lock().unwrap() {
                return Err(YandexError::Forbidden);
            }
            self.contents
                .lock()
                .unwrap()
                .get(href)
                .cloned()
                .ok_or(YandexError::NotFound)
        }
    }

    fn dir(path: &str, name: &str, remote_version: Option<&str>) -> ResourceEntry {
        ResourceEntry {
            path: path.to_owned(),
            name: name.to_owned(),
            kind: ResourceKind::Directory,
            size: 0,
            created: Some(SystemTime::UNIX_EPOCH),
            modified: Some(SystemTime::UNIX_EPOCH),
            remote_version: remote_version.map(str::to_owned),
        }
    }

    fn file(path: &str, name: &str, bytes: &[u8], remote_version: Option<&str>) -> ResourceEntry {
        ResourceEntry {
            path: path.to_owned(),
            name: name.to_owned(),
            kind: ResourceKind::File,
            size: bytes.len() as u64,
            created: Some(SystemTime::UNIX_EPOCH),
            modified: Some(SystemTime::UNIX_EPOCH),
            remote_version: remote_version.map(str::to_owned),
        }
    }

    fn temp_state_root() -> PathBuf {
        let path = env::temp_dir().join(format!("discohack-sync-test-{:032x}", random::<u128>()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn make_service(remote: Arc<FakeRemote>, state_root: Option<PathBuf>) -> Arc<SyncService> {
        SyncService::with_client(remote, state_root).unwrap()
    }

    #[test]
    fn filename_helper_increments_numeric_suffixes() {
        assert_eq!(
            increment_conflict_name("disk:/file.txt"),
            "disk:/file (2).txt"
        );
        assert_eq!(
            increment_conflict_name("disk:/file (2).txt"),
            "disk:/file (3).txt"
        );
        assert_eq!(increment_conflict_name("disk:/file"), "disk:/file (2)");
        assert_eq!(
            increment_conflict_name("disk:/archive.tar.gz"),
            "disk:/archive.tar (2).gz"
        );
    }

    #[test]
    fn local_write_without_network_persists_queue_and_cache() {
        let remote = FakeRemote::with_fixture();
        let root = temp_state_root();
        let service = make_service(Arc::clone(&remote), Some(root));

        let created = service.create_file(ROOT_PATH, "offline.txt").unwrap();
        service.write_file(&created.file_id, 0, b"offline").unwrap();
        *remote.fail_downloads.lock().unwrap() = true;

        let data = service.read_file(&created.file_id, 0, 7).unwrap();
        assert_eq!(data, b"offline");

        let summary = service.sync_summary_snapshot().unwrap();
        assert!(summary.queued_count >= 1);
    }

    #[test]
    fn summary_snapshot_handles_empty_queue() {
        let remote = FakeRemote::with_fixture();
        let service = make_service(remote, Some(temp_state_root()));

        let summary = service.sync_summary_snapshot().unwrap();
        assert_eq!(summary.queued_count, 0);
        assert_eq!(summary.uploading_count, 0);
        assert_eq!(summary.downloading_count, 0);
    }

    #[test]
    fn cached_directory_listing_still_works_offline() {
        let remote = FakeRemote::with_fixture();
        let service = make_service(Arc::clone(&remote), Some(temp_state_root()));

        let online = service.list_directory(ROOT_PATH).unwrap();
        assert!(online.iter().any(|node| node.path == "disk:/report.txt"));

        remote.set_offline(true);

        let offline = service.list_directory(ROOT_PATH).unwrap();
        assert!(offline.iter().any(|node| node.path == "disk:/report.txt"));
        let child = service.lookup_child(ROOT_PATH, "report.txt").unwrap();
        assert_eq!(child.path, "disk:/report.txt");
    }

    #[test]
    fn truncate_write_works_offline_without_downloading_remote_bytes() {
        let remote = FakeRemote::with_fixture();
        let service = make_service(Arc::clone(&remote), Some(temp_state_root()));

        let report = service.get_entry("disk:/report.txt").unwrap();
        remote.set_offline(true);

        let prepared = service.prepare_write(&report.file_id, true).unwrap();
        service
            .write_file(&prepared.file_id, 0, b"offline rewrite")
            .unwrap();

        let bytes = service.read_file(&prepared.file_id, 0, 15).unwrap();
        assert_eq!(bytes, b"offline rewrite");
        let refreshed = service.get_entry("disk:/report.txt").unwrap();
        assert_eq!(refreshed.sync_state, SyncState::QueuedUpload);
    }

    #[test]
    fn restart_keeps_pending_jobs() {
        let remote = FakeRemote::with_fixture();
        let root = temp_state_root();
        let service = make_service(Arc::clone(&remote), Some(root.clone()));
        let created = service.create_file(ROOT_PATH, "pending.txt").unwrap();
        service.write_file(&created.file_id, 0, b"pending").unwrap();

        let restarted = make_service(remote, Some(root));
        let summary = restarted.sync_summary_snapshot().unwrap();
        assert!(summary.queued_count >= 1);
        let restored = restarted.get_entry("disk:/pending.txt").unwrap();
        assert_eq!(
            restarted.read_file(&restored.file_id, 0, 7).unwrap(),
            b"pending"
        );
    }

    #[test]
    fn worker_completes_upload_and_updates_sync_state() {
        let remote = FakeRemote::with_fixture();
        let service = make_service(Arc::clone(&remote), Some(temp_state_root()));
        let created = service.create_file(ROOT_PATH, "upload.txt").unwrap();
        service.write_file(&created.file_id, 0, b"payload").unwrap();

        service.process_one_operation().unwrap();
        service.process_one_operation().unwrap_or(false);

        let node = service.get_entry("disk:/upload.txt").unwrap();
        assert_eq!(node.sync_state, SyncState::Synced);
        assert_eq!(
            remote.download_file("disk:/upload.txt").unwrap(),
            b"payload"
        );
    }

    #[test]
    fn repeated_writes_coalesce_to_single_upload_job() {
        let remote = FakeRemote::with_fixture();
        let service = make_service(remote, Some(temp_state_root()));
        let created = service.create_file(ROOT_PATH, "coalesce.txt").unwrap();
        service.write_file(&created.file_id, 0, b"one").unwrap();
        service.write_file(&created.file_id, 3, b"two").unwrap();

        let db = service.db.lock().unwrap();
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM operations_queue WHERE file_id = ?1 AND op_type = ?2 AND op_status = ?3",
                params![created.file_id, OperationType::Upload as i32, OperationStatus::Pending as i32],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn conflict_detection_creates_conflict_copy() {
        let remote = FakeRemote::with_fixture();
        remote.set_bytes("disk:/report.txt", b"hello");
        let service = make_service(Arc::clone(&remote), Some(temp_state_root()));
        let report = service.get_entry("disk:/report.txt").unwrap();
        let _ = service.read_file(&report.file_id, 0, 5).unwrap();
        service.write_file(&report.file_id, 0, b"HELLO").unwrap();
        remote
            .resources
            .lock()
            .unwrap()
            .get_mut("disk:/report.txt")
            .unwrap()
            .remote_version = Some(String::from("rev-2"));

        let _ = service.process_one_operation();

        let summary = service.sync_summary_snapshot().unwrap();
        assert_eq!(summary.conflict_count, 1);
        assert!(service.get_entry("disk:/report (2).txt").is_ok());
    }

    #[test]
    fn summary_and_items_reflect_attention_states() {
        let remote = FakeRemote::with_fixture();
        let service = make_service(remote, Some(temp_state_root()));
        let created = service.create_file(ROOT_PATH, "dbus.txt").unwrap();
        service.write_file(&created.file_id, 0, b"dbus").unwrap();

        let summary = service.sync_summary_snapshot().unwrap();
        assert!(summary.active_count >= 1);
        assert_eq!(summary.conflict_count, 0);
        let items = service.sync_items_snapshot().unwrap();
        assert!(items
            .iter()
            .any(|item| item.path == "disk:/dbus.txt" && item.state == "queued"));
    }

    #[test]
    fn leased_job_is_recovered_after_restart() {
        let remote = FakeRemote::with_fixture();
        let root = temp_state_root();
        let service = make_service(Arc::clone(&remote), Some(root.clone()));
        let created = service.create_file(ROOT_PATH, "leased.txt").unwrap();
        service.write_file(&created.file_id, 0, b"leased").unwrap();

        {
            let db = service.db.lock().unwrap();
            db.execute(
                "UPDATE operations_queue SET op_status = ?1, lease_expires_at = ?2 WHERE file_id = ?3",
                params![OperationStatus::Leased as i32, unix_now() - 1, created.file_id],
            ).unwrap();
        }

        let restarted = make_service(remote, Some(root));
        let summary = restarted.sync_summary_snapshot().unwrap();
        assert!(summary.queued_count >= 1);
    }

    #[test]
    fn local_write_path_is_thread_safe_enough_for_restart_tests() {
        let remote = FakeRemote::with_fixture();
        let service = make_service(remote, Some(temp_state_root()));
        let created = service.create_file(ROOT_PATH, "threaded.txt").unwrap();

        let svc = Arc::clone(&service);
        let file_id = created.file_id.clone();
        let handle = thread::spawn(move || svc.write_file(&file_id, 0, b"thread").unwrap());
        handle.join().unwrap();
        assert_eq!(
            service.read_file(&created.file_id, 0, 6).unwrap(),
            b"thread"
        );
    }
}
