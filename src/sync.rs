use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use rand::random;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use thiserror::Error;
use tracing::warn;
use zbus::zvariant::{OwnedValue, Str};

use crate::yadisk::{ResourceEntry, ResourceKind, YandexDiskClient, YandexError};

pub const ROOT_PATH: &str = "disk:/";

const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(300);
const LEASE_TIMEOUT_SECS: i64 = 30;
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
        Self::fetch_resource_metadata(self, path)
    }

    fn list_directory(&self, path: &str) -> Result<Vec<ResourceEntry>, YandexError> {
        Self::list_directory(self, path)
    }

    fn create_directory(&self, path: &str) -> Result<(), YandexError> {
        Self::create_directory(self, path)
    }

    fn delete_resource(&self, path: &str, permanently: bool) -> Result<(), YandexError> {
        Self::delete_resource(self, path, permanently)
    }

    fn move_resource(&self, from: &str, to: &str, overwrite: bool) -> Result<(), YandexError> {
        Self::move_resource(self, from, to, overwrite)
    }

    fn resolve_download_url(&self, path: &str) -> Result<String, YandexError> {
        Self::resolve_download_url(self, path)
    }

    fn resolve_upload_url(&self, path: &str, overwrite: bool) -> Result<String, YandexError> {
        Self::resolve_upload_url(self, path, overwrite)
    }

    fn upload_file(&self, href: &str, local_path: &Path) -> Result<(), YandexError> {
        Self::upload_file(self, href, local_path)
    }

    fn download_file(&self, href: &str) -> Result<Vec<u8>, YandexError> {
        Self::download_file(self, href)
    }
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error("entry not found")]
    NotFound,
    #[error("entry already exists")]
    AlreadyExists,
    #[error("path is not a directory")]
    NotDir,
    #[error("path is a directory")]
    IsDir,
    #[error("directory is not empty")]
    DirectoryNotEmpty,
    #[error("sync conflict: {0}")]
    Conflict(String),
    #[error("invalid enum value {value} for {name}")]
    InvalidEnum { name: &'static str, value: i32 },
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("local I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("remote sync failed: {0}")]
    Remote(#[from] YandexError),
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    File = 0,
    Directory = 1,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SyncState {
    Synced = 0,
    QueuedUpload = 1,
    Uploading = 2,
    Downloading = 3,
    Conflict = 4,
    QueuedDelete = 5,
    Error = 6,
    Placeholder = 7,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentState {
    Placeholder = 0,
    Hydrated = 1,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueOpType {
    Upload = 0,
    Delete = 1,
    Mkdir = 2,
    Move = 3,
    Rename = 4,
    RefreshTree = 5,
    RefreshDir = 6,
    Download = 7,
    ReconcileRemoteDelete = 8,
}

#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueOpStatus {
    Pending = 0,
    Leased = 1,
    Done = 2,
    RetryableError = 3,
    PermanentError = 4,
    Conflict = 5,
}

macro_rules! impl_int_enum {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        impl TryFrom<i32> for $name {
            type Error = SyncError;

            fn try_from(value: i32) -> Result<Self, SyncError> {
                match value {
                    $(x if x == $name::$variant as i32 => Ok($name::$variant),)+
                    other => Err(SyncError::InvalidEnum { name: stringify!($name), value: other }),
                }
            }
        }
    };
}

impl_int_enum!(NodeKind { File, Directory });
impl_int_enum!(SyncState {
    Synced,
    QueuedUpload,
    Uploading,
    Downloading,
    Conflict,
    QueuedDelete,
    Error,
    Placeholder,
});
impl_int_enum!(ContentState {
    Placeholder,
    Hydrated
});
impl_int_enum!(QueueOpType {
    Upload,
    Delete,
    Mkdir,
    Move,
    Rename,
    RefreshTree,
    RefreshDir,
    Download,
    ReconcileRemoteDelete,
});
impl_int_enum!(QueueOpStatus {
    Pending,
    Leased,
    Done,
    RetryableError,
    PermanentError,
    Conflict,
});

#[derive(Clone, Debug)]
pub struct StoredEntry {
    pub file_id: String,
    pub path: String,
    pub parent_path: Option<String>,
    pub name: String,
    pub kind: NodeKind,
    pub sync_state: SyncState,
    pub content_state: ContentState,
    pub remote_version: Option<String>,
    pub local_version: i64,
    pub mtime: Option<i64>,
    pub size: u64,
    pub cache_path: Option<PathBuf>,
    pub last_remote_check_at: Option<i64>,
    pub remote_deleted: bool,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug)]
struct QueuedOperation {
    id: i64,
    file_id: String,
    op_type: QueueOpType,
    op_status: QueueOpStatus,
    payload_json: Option<String>,
    retry_count: i64,
}

#[derive(Clone, Debug)]
pub struct SyncSummary {
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

#[derive(Clone, Debug)]
pub struct SyncItem {
    pub path: String,
    pub state: String,
    pub direction: String,
    pub progress: u32,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub updated_at: i64,
}

#[derive(Clone, Debug)]
pub struct StatusEntry {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub state: String,
    pub direction: String,
    pub is_conflicted: bool,
    pub is_placeholder: bool,
    pub progress: u32,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub updated_at: i64,
    pub known: bool,
}

#[derive(Clone, Debug)]
struct StatePaths {
    root_dir: PathBuf,
    db_path: PathBuf,
    cache_dir: PathBuf,
}

pub struct SyncService {
    client: Arc<dyn RemoteSyncClient>,
    mountpoint: PathBuf,
    paths: StatePaths,
    db: Mutex<Connection>,
    stop: AtomicBool,
    status_version: AtomicU64,
    worker_handles: Mutex<Vec<thread::JoinHandle<()>>>,
}

impl SyncService {
    pub fn open_default(
        client: Arc<dyn RemoteSyncClient>,
        mountpoint: PathBuf,
    ) -> Result<Arc<Self>, SyncError> {
        let state_home = env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
            .unwrap_or_else(|| env::temp_dir().join("discohack-daemon-state"));
        let root_dir = state_home.join("discohack-daemon");
        Self::open_at(root_dir, client, mountpoint)
    }

    pub fn open_at(
        root_dir: PathBuf,
        client: Arc<dyn RemoteSyncClient>,
        mountpoint: PathBuf,
    ) -> Result<Arc<Self>, SyncError> {
        let paths = StatePaths {
            db_path: root_dir.join("metadata.db"),
            cache_dir: root_dir.join("cache"),
            root_dir,
        };
        fs::create_dir_all(&paths.cache_dir)?;
        let mut conn = Connection::open(&paths.db_path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        run_migrations(&mut conn)?;

        let service = Arc::new(Self {
            client,
            mountpoint,
            paths,
            db: Mutex::new(conn),
            stop: AtomicBool::new(false),
            status_version: AtomicU64::new(1),
            worker_handles: Mutex::new(Vec::new()),
        });
        service.ensure_root_entry()?;
        service.recover_expired_leases()?;
        service.request_refresh(ROOT_PATH)?;
        Ok(service)
    }

    pub fn start_background(self: &Arc<Self>) {
        let mut handles = self.worker_handles.lock().unwrap();
        if !handles.is_empty() {
            return;
        }

        let worker = Arc::clone(self);
        handles.push(thread::spawn(move || worker.worker_loop()));

        let refresher = Arc::clone(self);
        handles.push(thread::spawn(move || refresher.refresh_loop()));
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let mut handles = self.worker_handles.lock().unwrap();
        while let Some(handle) = handles.pop() {
            let _ = handle.join();
        }
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    pub fn status_version(&self) -> u64 {
        self.status_version.load(Ordering::SeqCst)
    }

    pub fn request_refresh(&self, path: &str) -> Result<(), SyncError> {
        let entry = self
            .get_entry(path)?
            .ok_or(SyncError::NotFound)
            .or_else(|_| self.get_entry(ROOT_PATH)?.ok_or(SyncError::NotFound))?;
        let op_type = if path == ROOT_PATH {
            QueueOpType::RefreshTree
        } else {
            QueueOpType::RefreshDir
        };
        let payload = json!({ "path": path });
        let mut conn = self.db.lock().unwrap();
        upsert_operation(&mut conn, &entry.file_id, op_type, &payload, false, false)?;
        self.bump_status_version();
        Ok(())
    }

    pub fn get_entry(&self, path: &str) -> Result<Option<StoredEntry>, SyncError> {
        let conn = self.db.lock().unwrap();
        query_entry_by_path(&conn, path)
    }

    pub fn list_children(&self, parent_path: &str) -> Result<Vec<StoredEntry>, SyncError> {
        let conn = self.db.lock().unwrap();
        query_children(&conn, parent_path)
    }

    pub fn create_file(&self, parent_path: &str, name: &str) -> Result<StoredEntry, SyncError> {
        let parent = self.get_entry(parent_path)?.ok_or(SyncError::NotFound)?;
        if parent.kind != NodeKind::Directory {
            return Err(SyncError::NotDir);
        }
        let path = join_remote_path(parent_path, name);
        if self.get_entry(&path)?.is_some() {
            return Err(SyncError::AlreadyExists);
        }

        let file_id = new_id("file");
        let cache_path = self.cache_path_for(&file_id);
        if let Some(parent_dir) = cache_path.parent() {
            fs::create_dir_all(parent_dir)?;
        }
        fs::write(&cache_path, [])?;

        let now = now_unix();
        let mut conn = self.db.lock().unwrap();
        conn.execute(
            "INSERT INTO files (
                file_id, path, parent_path, name, kind, sync_state, content_state,
                remote_version, local_version, mtime, size, cache_path,
                last_remote_check_at, remote_deleted, last_error
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 1, ?8, 0, ?9, NULL, 0, NULL)",
            params![
                file_id,
                path,
                parent_path,
                name,
                NodeKind::File as i32,
                SyncState::QueuedUpload as i32,
                ContentState::Hydrated as i32,
                now,
                cache_path.to_string_lossy().to_string(),
            ],
        )?;
        upsert_operation(
            &mut conn,
            &file_id,
            QueueOpType::Upload,
            &json!({ "path": path }),
            false,
            false,
        )?;
        self.bump_status_version();
        drop(conn);
        self.get_entry(&join_remote_path(parent_path, name))?
            .ok_or(SyncError::NotFound)
    }

    pub fn apply_local_file_from_staging(
        &self,
        path: &str,
        staging_path: &Path,
    ) -> Result<StoredEntry, SyncError> {
        let entry = self.get_entry(path)?.ok_or(SyncError::NotFound)?;
        if entry.kind != NodeKind::File {
            return Err(SyncError::IsDir);
        }

        let cache_path = entry
            .cache_path
            .clone()
            .unwrap_or_else(|| self.cache_path_for(&entry.file_id));
        if let Some(parent_dir) = cache_path.parent() {
            fs::create_dir_all(parent_dir)?;
        }
        fs::copy(staging_path, &cache_path)?;
        let size = fs::metadata(&cache_path)?.len() as i64;
        let now = now_unix();

        let mut conn = self.db.lock().unwrap();
        conn.execute(
            "UPDATE files SET
                sync_state = ?2,
                content_state = ?3,
                local_version = local_version + 1,
                mtime = ?4,
                size = ?5,
                cache_path = ?6,
                remote_deleted = 0,
                last_error = NULL
             WHERE file_id = ?1",
            params![
                entry.file_id,
                SyncState::QueuedUpload as i32,
                ContentState::Hydrated as i32,
                now,
                size,
                cache_path.to_string_lossy().to_string(),
            ],
        )?;
        upsert_operation(
            &mut conn,
            &entry.file_id,
            QueueOpType::Upload,
            &json!({ "path": path }),
            false,
            false,
        )?;
        self.bump_status_version();
        drop(conn);
        self.get_entry(path)?.ok_or(SyncError::NotFound)
    }

    pub fn mkdir(&self, parent_path: &str, name: &str) -> Result<StoredEntry, SyncError> {
        let parent = self.get_entry(parent_path)?.ok_or(SyncError::NotFound)?;
        if parent.kind != NodeKind::Directory {
            return Err(SyncError::NotDir);
        }
        let path = join_remote_path(parent_path, name);
        if self.get_entry(&path)?.is_some() {
            return Err(SyncError::AlreadyExists);
        }
        let file_id = new_id("dir");
        let now = now_unix();

        let mut conn = self.db.lock().unwrap();
        conn.execute(
            "INSERT INTO files (
                file_id, path, parent_path, name, kind, sync_state, content_state,
                remote_version, local_version, mtime, size, cache_path,
                last_remote_check_at, remote_deleted, last_error
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, 1, ?8, 0, NULL, NULL, 0, NULL)",
            params![
                file_id,
                path,
                parent_path,
                name,
                NodeKind::Directory as i32,
                SyncState::QueuedUpload as i32,
                ContentState::Placeholder as i32,
                now,
            ],
        )?;
        upsert_operation(
            &mut conn,
            &file_id,
            QueueOpType::Mkdir,
            &json!({ "path": path }),
            false,
            false,
        )?;
        self.bump_status_version();
        drop(conn);
        self.get_entry(&join_remote_path(parent_path, name))?
            .ok_or(SyncError::NotFound)
    }

    pub fn unlink(&self, path: &str, expect_dir: bool) -> Result<(), SyncError> {
        let entry = self.get_entry(path)?.ok_or(SyncError::NotFound)?;
        match (expect_dir, entry.kind) {
            (true, NodeKind::File) => return Err(SyncError::NotDir),
            (false, NodeKind::Directory) => return Err(SyncError::IsDir),
            _ => {}
        }
        if entry.kind == NodeKind::Directory && !self.list_children(path)?.is_empty() {
            return Err(SyncError::DirectoryNotEmpty);
        }

        let mut conn = self.db.lock().unwrap();
        if entry.remote_version.is_none() {
            conn.execute(
                "DELETE FROM operations_queue WHERE file_id = ?1 AND op_status != ?2",
                params![entry.file_id, QueueOpStatus::Done as i32],
            )?;
            conn.execute(
                "DELETE FROM files WHERE file_id = ?1",
                params![entry.file_id],
            )?;
            if let Some(cache_path) = entry.cache_path {
                let _ = fs::remove_file(cache_path);
            }
        } else {
            conn.execute(
                "UPDATE files SET sync_state = ?2, last_error = NULL WHERE file_id = ?1",
                params![entry.file_id, SyncState::QueuedDelete as i32],
            )?;
            upsert_operation(
                &mut conn,
                &entry.file_id,
                QueueOpType::Delete,
                &json!({ "path": path }),
                true,
                true,
            )?;
            if let Some(cache_path) = entry.cache_path {
                let _ = fs::remove_file(cache_path);
            }
        }
        self.bump_status_version();
        Ok(())
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<(), SyncError> {
        let entry = self.get_entry(from)?.ok_or(SyncError::NotFound)?;
        if self.get_entry(to)?.is_some() {
            return Err(SyncError::AlreadyExists);
        }
        let new_parent = parent_path_of(to).ok_or(SyncError::NotFound)?;
        let parent = self.get_entry(&new_parent)?.ok_or(SyncError::NotFound)?;
        if parent.kind != NodeKind::Directory {
            return Err(SyncError::NotDir);
        }
        let new_name = name_of(to).to_owned();
        let now = now_unix();

        let mut conn = self.db.lock().unwrap();
        rename_subtree(&conn, from, to, &new_parent, &new_name, now)?;
        if entry.remote_version.is_some() {
            upsert_operation(
                &mut conn,
                &entry.file_id,
                QueueOpType::Rename,
                &json!({ "old_path": from, "new_path": to }),
                false,
                false,
            )?;
        }
        self.bump_status_version();
        Ok(())
    }

    pub fn read_range(&self, path: &str, offset: u64, size: u32) -> Result<Vec<u8>, SyncError> {
        let entry = self.ensure_hydrated(path)?;
        let cache_path = entry.cache_path.ok_or(SyncError::NotFound)?;
        if size == 0 {
            return Ok(Vec::new());
        }
        let data = fs::read(cache_path)?;
        let start = offset as usize;
        if start >= data.len() {
            return Ok(Vec::new());
        }
        let end = (start + size as usize).min(data.len());
        Ok(data[start..end].to_vec())
    }

    pub fn load_local_bytes(&self, path: &str) -> Result<Vec<u8>, SyncError> {
        let entry = self.ensure_hydrated(path)?;
        let cache_path = entry.cache_path.ok_or(SyncError::NotFound)?;
        Ok(fs::read(cache_path)?)
    }

    pub fn sync_summary(&self) -> Result<SyncSummary, SyncError> {
        let conn = self.db.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT sync_state, COUNT(*) FROM files GROUP BY sync_state")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, i32>(0)?, row.get::<_, i64>(1)?)))?;

        let mut counts = HashMap::new();
        for row in rows {
            let (state_raw, count) = row?;
            counts.insert(SyncState::try_from(state_raw)?, count as u32);
        }

        let uploading_count = counts.get(&SyncState::Uploading).copied().unwrap_or(0);
        let downloading_count = counts.get(&SyncState::Downloading).copied().unwrap_or(0);
        let queued_count = counts.get(&SyncState::QueuedUpload).copied().unwrap_or(0)
            + counts.get(&SyncState::QueuedDelete).copied().unwrap_or(0);
        let conflict_count = counts.get(&SyncState::Conflict).copied().unwrap_or(0);
        let error_count = counts.get(&SyncState::Error).copied().unwrap_or(0);
        let active_count = queued_count + uploading_count + downloading_count;
        let last_update_unix = now_unix();

        Ok(SyncSummary {
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

    pub fn sync_items(&self) -> Result<Vec<SyncItem>, SyncError> {
        let conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT * FROM files
             WHERE sync_state IN (?1, ?2, ?3, ?4, ?5)
             ORDER BY COALESCE(mtime, 0) DESC
             LIMIT ?6",
        )?;
        let rows = stmt.query_map(
            params![
                SyncState::QueuedUpload as i32,
                SyncState::Uploading as i32,
                SyncState::Downloading as i32,
                SyncState::Conflict as i32,
                SyncState::Error as i32,
                MAX_SYNC_ITEMS as i64,
            ],
            row_to_entry,
        )?;
        let mut items = Vec::new();
        for row in rows {
            let entry = row?;
            items.push(SyncItem {
                path: entry.path,
                state: sync_state_label(entry.sync_state).to_owned(),
                direction: sync_direction(entry.sync_state).to_owned(),
                progress: if matches!(entry.sync_state, SyncState::Synced) {
                    100
                } else {
                    0
                },
                bytes_done: 0,
                bytes_total: entry.size,
                updated_at: entry.mtime.unwrap_or(0),
            });
        }
        Ok(items)
    }

    pub fn sync_status(&self, path: &str) -> Result<StatusEntry, SyncError> {
        match self.get_entry(path)? {
            Some(entry) => Ok(status_from_entry(entry)),
            None => Ok(StatusEntry {
                path: path.to_owned(),
                name: name_of(path).to_owned(),
                kind: String::from("unknown"),
                state: String::from("unknown"),
                direction: String::from("none"),
                is_conflicted: false,
                is_placeholder: false,
                progress: 0,
                bytes_done: 0,
                bytes_total: 0,
                updated_at: 0,
                known: false,
            }),
        }
    }

    pub fn list_directory_statuses(&self, path: &str) -> Result<Vec<StatusEntry>, SyncError> {
        let entry = self.get_entry(path)?.ok_or(SyncError::NotFound)?;
        if entry.kind != NodeKind::Directory {
            return Err(SyncError::NotDir);
        }
        let children = self.list_children(path)?;
        Ok(children.into_iter().map(status_from_entry).collect())
    }

    pub fn sync_summary_dict(&self) -> Result<HashMap<String, OwnedValue>, SyncError> {
        let summary = self.sync_summary()?;
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

    pub fn sync_items_dict(&self) -> Result<Vec<HashMap<String, OwnedValue>>, SyncError> {
        let items = self.sync_items()?;
        Ok(items
            .into_iter()
            .map(|item| {
                HashMap::from([
                    (String::from("path"), ov_string(item.path)),
                    (String::from("state"), ov_string(item.state)),
                    (String::from("direction"), ov_string(item.direction)),
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

    pub fn status_dict(&self, path: &str) -> Result<HashMap<String, OwnedValue>, SyncError> {
        Ok(status_to_dict(self.sync_status(path)?))
    }

    pub fn directory_statuses_dict(
        &self,
        path: &str,
    ) -> Result<Vec<HashMap<String, OwnedValue>>, SyncError> {
        Ok(self
            .list_directory_statuses(path)?
            .into_iter()
            .map(status_to_dict)
            .collect())
    }

    fn ensure_root_entry(&self) -> Result<(), SyncError> {
        let mut conn = self.db.lock().unwrap();
        if query_entry_by_path(&conn, ROOT_PATH)?.is_some() {
            return Ok(());
        }
        let now = now_unix();
        conn.execute(
            "INSERT INTO files (
                file_id, path, parent_path, name, kind, sync_state, content_state,
                remote_version, local_version, mtime, size, cache_path,
                last_remote_check_at, remote_deleted, last_error
            ) VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6, NULL, 0, ?7, 0, NULL, NULL, 0, NULL)",
            params![
                String::from("root"),
                ROOT_PATH,
                ROOT_PATH,
                NodeKind::Directory as i32,
                SyncState::Placeholder as i32,
                ContentState::Placeholder as i32,
                now,
            ],
        )?;
        self.bump_status_version();
        Ok(())
    }

    fn ensure_hydrated(&self, path: &str) -> Result<StoredEntry, SyncError> {
        let entry = self.get_entry(path)?.ok_or(SyncError::NotFound)?;
        if entry.kind != NodeKind::File {
            return Err(SyncError::IsDir);
        }
        if entry.content_state == ContentState::Hydrated {
            if let Some(cache_path) = &entry.cache_path {
                if cache_path.exists() {
                    return Ok(entry);
                }
            }
        }

        let download_url = self.client.resolve_download_url(path)?;
        let bytes = self.client.download_file(&download_url)?;
        let cache_path = entry
            .cache_path
            .clone()
            .unwrap_or_else(|| self.cache_path_for(&entry.file_id));
        if let Some(parent_dir) = cache_path.parent() {
            fs::create_dir_all(parent_dir)?;
        }
        fs::write(&cache_path, &bytes)?;

        let mut conn = self.db.lock().unwrap();
        conn.execute(
            "UPDATE files SET
                content_state = ?2,
                cache_path = ?3,
                size = ?4,
                mtime = ?5,
                sync_state = CASE WHEN sync_state = ?6 THEN ?7 ELSE sync_state END
             WHERE file_id = ?1",
            params![
                entry.file_id,
                ContentState::Hydrated as i32,
                cache_path.to_string_lossy().to_string(),
                bytes.len() as i64,
                now_unix(),
                SyncState::Placeholder as i32,
                SyncState::Synced as i32,
            ],
        )?;
        self.bump_status_version();
        drop(conn);
        self.get_entry(path)?.ok_or(SyncError::NotFound)
    }

    fn recover_expired_leases(&self) -> Result<(), SyncError> {
        let mut conn = self.db.lock().unwrap();
        conn.execute(
            "UPDATE operations_queue
             SET op_status = ?1, worker_id = NULL, lease_expires_at = NULL, updated_at = ?2
             WHERE op_status = ?3 AND COALESCE(lease_expires_at, 0) <= ?2",
            params![
                QueueOpStatus::Pending as i32,
                now_unix(),
                QueueOpStatus::Leased as i32,
            ],
        )?;
        Ok(())
    }

    fn worker_loop(self: Arc<Self>) {
        while !self.stop.load(Ordering::SeqCst) {
            if let Err(err) = self.recover_expired_leases() {
                warn!(error = %err, "failed to recover expired sync leases");
            }

            match self.lease_next_operation() {
                Ok(Some(job)) => {
                    if let Err(err) = self.process_operation(job) {
                        warn!(error = %err, "sync operation processing failed");
                    }
                }
                Ok(None) => thread::sleep(WORKER_POLL_INTERVAL),
                Err(err) => {
                    warn!(error = %err, "failed to lease sync operation");
                    thread::sleep(WORKER_POLL_INTERVAL);
                }
            }
        }
    }

    fn refresh_loop(self: Arc<Self>) {
        while !self.stop.load(Ordering::SeqCst) {
            thread::sleep(REFRESH_INTERVAL);
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            if let Err(err) = self.request_refresh(ROOT_PATH) {
                warn!(error = %err, "failed to schedule periodic refresh");
            }
        }
    }

    fn lease_next_operation(&self) -> Result<Option<QueuedOperation>, SyncError> {
        let now = now_unix();
        let worker_id = new_id("worker");
        let mut conn = self.db.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, file_id, op_type, op_status, payload_json, retry_count
             FROM operations_queue
             WHERE op_status IN (?1, ?2)
               AND (next_retry_at IS NULL OR next_retry_at <= ?3)
             ORDER BY id ASC
             LIMIT 1",
        )?;
        let job = stmt
            .query_row(
                params![
                    QueueOpStatus::Pending as i32,
                    QueueOpStatus::RetryableError as i32,
                    now,
                ],
                row_to_job,
            )
            .optional()?;
        let Some(job) = job else {
            return Ok(None);
        };
        conn.execute(
            "UPDATE operations_queue
             SET op_status = ?2, worker_id = ?3, lease_expires_at = ?4, updated_at = ?5
             WHERE id = ?1",
            params![
                job.id,
                QueueOpStatus::Leased as i32,
                worker_id,
                now + LEASE_TIMEOUT_SECS,
                now,
            ],
        )?;
        let mut leased = job;
        leased.op_status = QueueOpStatus::Leased;
        Ok(Some(leased))
    }

    fn process_operation(&self, job: QueuedOperation) -> Result<(), SyncError> {
        match job.op_type {
            QueueOpType::Upload => self.process_upload(job),
            QueueOpType::Delete => self.process_delete(job),
            QueueOpType::Mkdir => self.process_mkdir(job),
            QueueOpType::Rename | QueueOpType::Move => self.process_rename(job),
            QueueOpType::RefreshTree => self.process_refresh_tree(job),
            QueueOpType::RefreshDir => self.process_refresh_dir(job),
            QueueOpType::Download => self.process_download(job),
            QueueOpType::ReconcileRemoteDelete => self.process_delete(job),
        }
    }

    fn process_upload(&self, job: QueuedOperation) -> Result<(), SyncError> {
        let entry = self
            .entry_by_file_id(&job.file_id)?
            .ok_or(SyncError::NotFound)?;
        if entry.sync_state == SyncState::Conflict {
            return self.finish_job(job.id, QueueOpStatus::Conflict, Some("file is conflicted"));
        }
        let cache_path = entry.cache_path.clone().ok_or(SyncError::NotFound)?;
        self.set_file_state(&entry.file_id, SyncState::Uploading, None)?;

        let remote_meta = match self.client.fetch_resource_metadata(&entry.path) {
            Ok(resource) => Some(resource),
            Err(YandexError::NotFound) => None,
            Err(err) => return self.retry_or_fail(job, err),
        };
        let current_remote_version = remote_meta
            .as_ref()
            .and_then(|resource| resource.remote_version.clone());
        if entry.remote_version != current_remote_version {
            self.handle_conflict(&entry, current_remote_version, remote_meta.as_ref())?;
            return self.finish_job(
                job.id,
                QueueOpStatus::Conflict,
                Some("remote version conflict"),
            );
        }

        let href = match self.client.resolve_upload_url(&entry.path, true) {
            Ok(href) => href,
            Err(err) => return self.retry_or_fail(job, err),
        };
        if let Err(err) = self.client.upload_file(&href, &cache_path) {
            return self.retry_or_fail(job, err);
        }
        let fresh = match self.client.fetch_resource_metadata(&entry.path) {
            Ok(fresh) => fresh,
            Err(err) => return self.retry_or_fail(job, err),
        };

        let mut conn = self.db.lock().unwrap();
        conn.execute(
            "UPDATE files SET
                sync_state = ?2,
                remote_version = ?3,
                size = ?4,
                mtime = ?5,
                remote_deleted = 0,
                last_error = NULL,
                last_remote_check_at = ?5
             WHERE file_id = ?1",
            params![
                entry.file_id,
                SyncState::Synced as i32,
                fresh.remote_version,
                fresh.size as i64,
                system_time_to_unix(fresh.modified),
            ],
        )?;
        self.mark_job_done_locked(&mut conn, job.id)?;
        self.bump_status_version();
        Ok(())
    }

    fn process_delete(&self, job: QueuedOperation) -> Result<(), SyncError> {
        let payload = parse_payload(&job.payload_json);
        let path = payload
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(ROOT_PATH)
            .to_owned();
        match self.client.delete_resource(&path, true) {
            Ok(()) | Err(YandexError::NotFound) => {}
            Err(err) => return self.retry_or_fail(job, err),
        }

        let mut conn = self.db.lock().unwrap();
        conn.execute(
            "DELETE FROM operations_queue WHERE id = ?1",
            params![job.id],
        )?;
        conn.execute("DELETE FROM files WHERE file_id = ?1", params![job.file_id])?;
        self.bump_status_version();
        Ok(())
    }

    fn process_mkdir(&self, job: QueuedOperation) -> Result<(), SyncError> {
        let entry = self
            .entry_by_file_id(&job.file_id)?
            .ok_or(SyncError::NotFound)?;
        match self.client.create_directory(&entry.path) {
            Ok(()) => {}
            Err(YandexError::Conflict(_)) => {}
            Err(err) => return self.retry_or_fail(job, err),
        }
        let fresh = self.client.fetch_resource_metadata(&entry.path)?;

        let mut conn = self.db.lock().unwrap();
        conn.execute(
            "UPDATE files SET sync_state = ?2, remote_version = ?3, mtime = ?4, last_error = NULL WHERE file_id = ?1",
            params![
                entry.file_id,
                SyncState::Synced as i32,
                fresh.remote_version,
                system_time_to_unix(fresh.modified),
            ],
        )?;
        self.mark_job_done_locked(&mut conn, job.id)?;
        self.bump_status_version();
        Ok(())
    }

    fn process_rename(&self, job: QueuedOperation) -> Result<(), SyncError> {
        let payload = parse_payload(&job.payload_json);
        let old_path = payload
            .get("old_path")
            .and_then(Value::as_str)
            .ok_or(SyncError::NotFound)?;
        let new_path = payload
            .get("new_path")
            .and_then(Value::as_str)
            .ok_or(SyncError::NotFound)?;

        let current_exists = self.client.fetch_resource_metadata(new_path).is_ok();
        if current_exists {
            match self.client.delete_resource(old_path, true) {
                Ok(()) | Err(YandexError::NotFound) => {}
                Err(err) => return self.retry_or_fail(job, err),
            }
        } else {
            match self.client.move_resource(old_path, new_path, true) {
                Ok(()) | Err(YandexError::NotFound) => {}
                Err(err) => return self.retry_or_fail(job, err),
            }
        }

        let fresh = self
            .client
            .fetch_resource_metadata(new_path)
            .optional()
            .map_err(SyncError::Remote)?;
        let mut conn = self.db.lock().unwrap();
        if let Some(fresh) = fresh {
            conn.execute(
                "UPDATE files SET sync_state = ?2, remote_version = ?3, mtime = ?4, last_error = NULL WHERE file_id = ?1",
                params![
                    job.file_id,
                    SyncState::Synced as i32,
                    fresh.remote_version,
                    system_time_to_unix(fresh.modified),
                ],
            )?;
        }
        self.mark_job_done_locked(&mut conn, job.id)?;
        self.bump_status_version();
        Ok(())
    }

    fn process_refresh_tree(&self, job: QueuedOperation) -> Result<(), SyncError> {
        let remote = self.collect_remote_tree(ROOT_PATH)?;
        self.reconcile_remote_tree(remote)?;
        let mut conn = self.db.lock().unwrap();
        self.mark_job_done_locked(&mut conn, job.id)?;
        self.bump_status_version();
        Ok(())
    }

    fn process_refresh_dir(&self, job: QueuedOperation) -> Result<(), SyncError> {
        let payload = parse_payload(&job.payload_json);
        let path = payload
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(ROOT_PATH);
        let children = self.client.list_directory(path)?;
        self.reconcile_remote_tree(children)?;
        let mut conn = self.db.lock().unwrap();
        self.mark_job_done_locked(&mut conn, job.id)?;
        self.bump_status_version();
        Ok(())
    }

    fn process_download(&self, job: QueuedOperation) -> Result<(), SyncError> {
        let entry = self
            .entry_by_file_id(&job.file_id)?
            .ok_or(SyncError::NotFound)?;
        let _ = self.ensure_hydrated(&entry.path)?;
        let mut conn = self.db.lock().unwrap();
        self.mark_job_done_locked(&mut conn, job.id)?;
        self.bump_status_version();
        Ok(())
    }

    fn retry_or_fail(&self, job: QueuedOperation, err: YandexError) -> Result<(), SyncError> {
        let retryable = matches!(
            err,
            YandexError::Unauthorized
                | YandexError::Forbidden
                | YandexError::Http(_)
                | YandexError::Status { .. }
        );
        let mut conn = self.db.lock().unwrap();
        if retryable {
            let next_retry = now_unix() + (1_i64 << (job.retry_count.min(5) as u32));
            conn.execute(
                "UPDATE operations_queue
                 SET op_status = ?2,
                     retry_count = retry_count + 1,
                     next_retry_at = ?3,
                     worker_id = NULL,
                     lease_expires_at = NULL,
                     updated_at = ?4
                 WHERE id = ?1",
                params![
                    job.id,
                    QueueOpStatus::RetryableError as i32,
                    next_retry,
                    now_unix(),
                ],
            )?;
            conn.execute(
                "UPDATE files SET sync_state = ?2, last_error = ?3 WHERE file_id = ?1",
                params![job.file_id, SyncState::Error as i32, err.to_string()],
            )?;
        } else {
            conn.execute(
                "UPDATE operations_queue
                 SET op_status = ?2, worker_id = NULL, lease_expires_at = NULL, updated_at = ?3
                 WHERE id = ?1",
                params![job.id, QueueOpStatus::PermanentError as i32, now_unix()],
            )?;
            conn.execute(
                "UPDATE files SET sync_state = ?2, last_error = ?3 WHERE file_id = ?1",
                params![job.file_id, SyncState::Error as i32, err.to_string()],
            )?;
        }
        self.bump_status_version();
        Ok(())
    }

    fn collect_remote_tree(&self, root: &str) -> Result<Vec<ResourceEntry>, SyncError> {
        let mut all = Vec::new();
        let root_meta = self.client.fetch_resource_metadata(root)?;
        all.push(root_meta.clone());
        if root_meta.kind == ResourceKind::Directory {
            self.collect_remote_children(root, &mut all)?;
        }
        Ok(all)
    }

    fn collect_remote_children(
        &self,
        path: &str,
        all: &mut Vec<ResourceEntry>,
    ) -> Result<(), SyncError> {
        let children = self.client.list_directory(path)?;
        for child in children {
            let child_path = child.path.clone();
            let is_dir = child.kind == ResourceKind::Directory;
            all.push(child);
            if is_dir {
                self.collect_remote_children(&child_path, all)?;
            }
        }
        Ok(())
    }

    fn reconcile_remote_tree(&self, remote_entries: Vec<ResourceEntry>) -> Result<(), SyncError> {
        let mut remote_map = HashMap::new();
        for entry in remote_entries {
            remote_map.insert(entry.path.clone(), entry);
        }

        let local_entries = {
            let conn = self.db.lock().unwrap();
            let mut stmt = conn.prepare("SELECT * FROM files")?;
            let rows = stmt.query_map([], row_to_entry)?;
            let mut entries = Vec::new();
            for row in rows {
                entries.push(row?);
            }
            entries
        };

        let mut invalidated_cache = Vec::new();
        let mut conn = self.db.lock().unwrap();
        let tx = conn.transaction()?;
        for remote in remote_map.values() {
            if remote.path == ROOT_PATH {
                ensure_remote_row(&tx, remote, SyncState::Synced)?;
                continue;
            }

            if let Some(local) = local_entries.iter().find(|entry| entry.path == remote.path) {
                if has_unsynced_local_changes(local)
                    && local.remote_version != remote.remote_version
                {
                    apply_conflict_locked(
                        &tx,
                        local,
                        local.remote_version.clone(),
                        remote.remote_version.clone(),
                        Some(remote),
                    )?;
                    continue;
                }

                if local.remote_version != remote.remote_version {
                    if let Some(cache_path) = &local.cache_path {
                        invalidated_cache.push(cache_path.clone());
                    }
                }
                ensure_remote_row(&tx, remote, SyncState::Synced)?;
            } else {
                ensure_remote_row(&tx, remote, SyncState::Synced)?;
            }
        }

        let remote_paths: HashSet<&str> = remote_map.keys().map(String::as_str).collect();
        for local in &local_entries {
            if local.path == ROOT_PATH || remote_paths.contains(local.path.as_str()) {
                continue;
            }
            if local.sync_state == SyncState::QueuedDelete {
                tx.execute(
                    "DELETE FROM files WHERE file_id = ?1",
                    params![local.file_id],
                )?;
                continue;
            }
            if has_unsynced_local_changes(local) {
                apply_conflict_locked(&tx, local, local.remote_version.clone(), None, None)?;
                continue;
            }
            tx.execute(
                "DELETE FROM files WHERE file_id = ?1",
                params![local.file_id],
            )?;
            if let Some(cache_path) = &local.cache_path {
                invalidated_cache.push(cache_path.clone());
            }
        }

        tx.commit()?;
        drop(conn);
        for path in invalidated_cache {
            let _ = fs::remove_file(path);
        }
        self.bump_status_version();
        Ok(())
    }

    fn entry_by_file_id(&self, file_id: &str) -> Result<Option<StoredEntry>, SyncError> {
        let conn = self.db.lock().unwrap();
        query_entry_by_file_id(&conn, file_id)
    }

    fn set_file_state(
        &self,
        file_id: &str,
        sync_state: SyncState,
        last_error: Option<&str>,
    ) -> Result<(), SyncError> {
        let mut conn = self.db.lock().unwrap();
        conn.execute(
            "UPDATE files SET sync_state = ?2, last_error = ?3 WHERE file_id = ?1",
            params![file_id, sync_state as i32, last_error],
        )?;
        self.bump_status_version();
        Ok(())
    }

    fn finish_job(
        &self,
        job_id: i64,
        status: QueueOpStatus,
        last_error: Option<&str>,
    ) -> Result<(), SyncError> {
        let mut conn = self.db.lock().unwrap();
        conn.execute(
            "UPDATE operations_queue
             SET op_status = ?2, worker_id = NULL, lease_expires_at = NULL, updated_at = ?3
             WHERE id = ?1",
            params![job_id, status as i32, now_unix()],
        )?;
        if let Some(last_error) = last_error {
            let file_id: Option<String> = conn
                .query_row(
                    "SELECT file_id FROM operations_queue WHERE id = ?1",
                    params![job_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(file_id) = file_id {
                conn.execute(
                    "UPDATE files SET last_error = ?2, sync_state = ?3 WHERE file_id = ?1",
                    params![file_id, last_error, SyncState::Conflict as i32],
                )?;
            }
        }
        self.bump_status_version();
        Ok(())
    }

    fn mark_job_done_locked(&self, conn: &mut Connection, job_id: i64) -> Result<(), SyncError> {
        conn.execute(
            "UPDATE operations_queue
             SET op_status = ?2, worker_id = NULL, lease_expires_at = NULL, updated_at = ?3
             WHERE id = ?1",
            params![job_id, QueueOpStatus::Done as i32, now_unix()],
        )?;
        Ok(())
    }

    fn handle_conflict(
        &self,
        entry: &StoredEntry,
        current_remote_version: Option<String>,
        remote_resource: Option<&ResourceEntry>,
    ) -> Result<(), SyncError> {
        let mut conn = self.db.lock().unwrap();
        let tx = conn.transaction()?;
        apply_conflict_locked(
            &tx,
            entry,
            entry.remote_version.clone(),
            current_remote_version,
            remote_resource,
        )?;
        tx.commit()?;
        self.bump_status_version();
        Ok(())
    }

    fn cache_path_for(&self, file_id: &str) -> PathBuf {
        self.paths.cache_dir.join(format!("{file_id}.bin"))
    }

    fn bump_status_version(&self) {
        self.status_version.fetch_add(1, Ordering::SeqCst);
    }
}

fn run_migrations(conn: &mut Connection) -> Result<(), SyncError> {
    let user_version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if user_version >= 1 {
        return Ok(());
    }

    conn.execute_batch(
        "BEGIN;
         CREATE TABLE IF NOT EXISTS files (
             file_id TEXT PRIMARY KEY,
             path TEXT NOT NULL UNIQUE,
             parent_path TEXT,
             name TEXT NOT NULL,
             kind INTEGER NOT NULL CHECK (kind IN (0, 1)),
             sync_state INTEGER NOT NULL CHECK (sync_state IN (0, 1, 2, 3, 4, 5, 6, 7)),
             content_state INTEGER NOT NULL CHECK (content_state IN (0, 1)),
             remote_version TEXT,
             local_version INTEGER NOT NULL DEFAULT 0,
             mtime INTEGER,
             size INTEGER NOT NULL DEFAULT 0,
             hash BLOB,
             cache_path TEXT,
             last_remote_check_at INTEGER,
             remote_deleted INTEGER NOT NULL DEFAULT 0 CHECK (remote_deleted IN (0, 1)),
             last_error TEXT
         );
         CREATE TABLE IF NOT EXISTS operations_queue (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             file_id TEXT NOT NULL,
             op_type INTEGER NOT NULL CHECK (op_type IN (0, 1, 2, 3, 4, 5, 6, 7, 8)),
             op_status INTEGER NOT NULL CHECK (op_status IN (0, 1, 2, 3, 4, 5)),
             payload_json TEXT,
             retry_count INTEGER NOT NULL DEFAULT 0,
             next_retry_at INTEGER,
             worker_id TEXT,
             lease_expires_at INTEGER,
             created_at INTEGER NOT NULL,
             updated_at INTEGER NOT NULL,
             FOREIGN KEY(file_id) REFERENCES files(file_id) ON DELETE CASCADE
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
             FOREIGN KEY(file_id) REFERENCES files(file_id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_files_parent_path ON files(parent_path);
         CREATE INDEX IF NOT EXISTS idx_queue_leases ON operations_queue(op_status, next_retry_at, lease_expires_at);
         PRAGMA user_version = 1;
         COMMIT;",
    )?;
    Ok(())
}

fn query_entry_by_path(conn: &Connection, path: &str) -> Result<Option<StoredEntry>, SyncError> {
    conn.query_row(
        "SELECT * FROM files WHERE path = ?1",
        params![path],
        row_to_entry,
    )
    .optional()
    .map_err(SyncError::Db)
}

fn query_entry_by_file_id(
    conn: &Connection,
    file_id: &str,
) -> Result<Option<StoredEntry>, SyncError> {
    conn.query_row(
        "SELECT * FROM files WHERE file_id = ?1",
        params![file_id],
        row_to_entry,
    )
    .optional()
    .map_err(SyncError::Db)
}

fn query_children(conn: &Connection, parent_path: &str) -> Result<Vec<StoredEntry>, SyncError> {
    let mut stmt = conn.prepare(
        "SELECT * FROM files
         WHERE parent_path = ?1
           AND sync_state != ?2
           AND remote_deleted = 0
         ORDER BY kind DESC, name ASC",
    )?;
    let rows = stmt.query_map(
        params![parent_path, SyncState::QueuedDelete as i32],
        row_to_entry,
    )?;
    let mut entries = Vec::new();
    for row in rows {
        entries.push(row?);
    }
    Ok(entries)
}

fn row_to_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredEntry> {
    let cache_path: Option<String> = row.get("cache_path")?;
    let remote_deleted: i64 = row.get("remote_deleted")?;
    let kind_raw: i32 = row.get("kind")?;
    let sync_state_raw: i32 = row.get("sync_state")?;
    let content_state_raw: i32 = row.get("content_state")?;

    let kind = NodeKind::try_from(kind_raw).map_err(to_sql_error)?;
    let sync_state = SyncState::try_from(sync_state_raw).map_err(to_sql_error)?;
    let content_state = ContentState::try_from(content_state_raw).map_err(to_sql_error)?;

    Ok(StoredEntry {
        file_id: row.get("file_id")?,
        path: row.get("path")?,
        parent_path: row.get("parent_path")?,
        name: row.get("name")?,
        kind,
        sync_state,
        content_state,
        remote_version: row.get("remote_version")?,
        local_version: row.get("local_version")?,
        mtime: row.get("mtime")?,
        size: row.get::<_, i64>("size")? as u64,
        cache_path: cache_path.map(PathBuf::from),
        last_remote_check_at: row.get("last_remote_check_at")?,
        remote_deleted: remote_deleted != 0,
        last_error: row.get("last_error")?,
    })
}

fn row_to_job(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueuedOperation> {
    let op_type_raw: i32 = row.get("op_type")?;
    let op_status_raw: i32 = row.get("op_status")?;
    Ok(QueuedOperation {
        id: row.get("id")?,
        file_id: row.get("file_id")?,
        op_type: QueueOpType::try_from(op_type_raw).map_err(to_sql_error)?,
        op_status: QueueOpStatus::try_from(op_status_raw).map_err(to_sql_error)?,
        payload_json: row.get("payload_json")?,
        retry_count: row.get("retry_count")?,
    })
}

fn upsert_operation(
    conn: &mut Connection,
    file_id: &str,
    op_type: QueueOpType,
    payload: &Value,
    replace_uploads: bool,
    replace_all: bool,
) -> Result<(), SyncError> {
    let now = now_unix();
    if replace_all {
        conn.execute(
            "DELETE FROM operations_queue
             WHERE file_id = ?1 AND op_status IN (?2, ?3, ?4)",
            params![
                file_id,
                QueueOpStatus::Pending as i32,
                QueueOpStatus::RetryableError as i32,
                QueueOpStatus::Leased as i32,
            ],
        )?;
    } else if replace_uploads {
        conn.execute(
            "DELETE FROM operations_queue
             WHERE file_id = ?1 AND op_type = ?2 AND op_status IN (?3, ?4, ?5)",
            params![
                file_id,
                QueueOpType::Upload as i32,
                QueueOpStatus::Pending as i32,
                QueueOpStatus::RetryableError as i32,
                QueueOpStatus::Leased as i32,
            ],
        )?;
    }

    let existing_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM operations_queue
             WHERE file_id = ?1 AND op_type = ?2 AND op_status IN (?3, ?4)
             ORDER BY id ASC LIMIT 1",
            params![
                file_id,
                op_type as i32,
                QueueOpStatus::Pending as i32,
                QueueOpStatus::RetryableError as i32,
            ],
            |row| row.get(0),
        )
        .optional()?;

    let payload_json = serde_json::to_string(payload).unwrap_or_else(|_| String::from("{}"));
    if let Some(id) = existing_id {
        conn.execute(
            "UPDATE operations_queue
             SET payload_json = ?2, op_status = ?3, next_retry_at = NULL, updated_at = ?4
             WHERE id = ?1",
            params![id, payload_json, QueueOpStatus::Pending as i32, now],
        )?;
    } else {
        conn.execute(
            "INSERT INTO operations_queue (
                 file_id, op_type, op_status, payload_json, retry_count,
                 next_retry_at, worker_id, lease_expires_at, created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, 0, NULL, NULL, NULL, ?5, ?5)",
            params![
                file_id,
                op_type as i32,
                QueueOpStatus::Pending as i32,
                payload_json,
                now,
            ],
        )?;
    }
    Ok(())
}

fn ensure_remote_row(
    conn: &Connection,
    remote: &ResourceEntry,
    sync_state: SyncState,
) -> Result<(), SyncError> {
    let existing = query_entry_by_path(conn, &remote.path)?;
    let file_id = existing
        .as_ref()
        .map(|entry| entry.file_id.clone())
        .unwrap_or_else(|| new_id("remote"));
    let cache_path = existing
        .as_ref()
        .and_then(|entry| entry.cache_path.as_ref())
        .map(|path| path.to_string_lossy().to_string());
    let content_state = if existing
        .as_ref()
        .map(|entry| {
            entry.content_state == ContentState::Hydrated
                && entry.remote_version == remote.remote_version
        })
        .unwrap_or(false)
    {
        ContentState::Hydrated
    } else {
        ContentState::Placeholder
    };
    conn.execute(
        "INSERT INTO files (
             file_id, path, parent_path, name, kind, sync_state, content_state,
             remote_version, local_version, mtime, size, cache_path,
             last_remote_check_at, remote_deleted, last_error
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, COALESCE(?9, 0), ?10, ?11, ?12, ?13, 0, NULL)
         ON CONFLICT(path) DO UPDATE SET
             parent_path = excluded.parent_path,
             name = excluded.name,
             kind = excluded.kind,
             sync_state = CASE WHEN files.sync_state IN (?14, ?15, ?16) THEN files.sync_state ELSE excluded.sync_state END,
             content_state = excluded.content_state,
             remote_version = excluded.remote_version,
             mtime = excluded.mtime,
             size = excluded.size,
             cache_path = excluded.cache_path,
             last_remote_check_at = excluded.last_remote_check_at,
             remote_deleted = 0,
             last_error = CASE WHEN files.sync_state = ?16 THEN files.last_error ELSE NULL END",
        params![
            file_id,
            remote.path,
            parent_path_of(&remote.path),
            remote.name,
            match remote.kind {
                ResourceKind::File => NodeKind::File as i32,
                ResourceKind::Directory => NodeKind::Directory as i32,
            },
            sync_state as i32,
            content_state as i32,
            remote.remote_version,
            existing.as_ref().map(|entry| entry.local_version),
            system_time_to_unix(remote.modified.or(remote.created)),
            remote.size as i64,
            cache_path,
            now_unix(),
            SyncState::QueuedUpload as i32,
            SyncState::QueuedDelete as i32,
            SyncState::Conflict as i32,
        ],
    )?;
    Ok(())
}

fn apply_conflict_locked(
    conn: &Connection,
    entry: &StoredEntry,
    base_remote_version: Option<String>,
    current_remote_version: Option<String>,
    remote_resource: Option<&ResourceEntry>,
) -> Result<(), SyncError> {
    let conflict_path = next_conflict_path(conn, &entry.path)?;
    let now = now_unix();
    let conflict_parent = parent_path_of(&conflict_path);
    let conflict_name = name_of(&conflict_path).to_owned();

    rename_subtree(
        conn,
        &entry.path,
        &conflict_path,
        conflict_parent.as_deref().unwrap_or(ROOT_PATH),
        &conflict_name,
        now,
    )?;
    conn.execute(
        "UPDATE files SET sync_state = ?2, last_error = ?3 WHERE file_id = ?1",
        params![entry.file_id, SyncState::Conflict as i32, "conflict"],
    )?;
    conn.execute(
        "UPDATE operations_queue SET op_status = ?2, updated_at = ?3 WHERE file_id = ?1 AND op_status != ?4",
        params![
            entry.file_id,
            QueueOpStatus::Conflict as i32,
            now,
            QueueOpStatus::Done as i32,
        ],
    )?;
    conn.execute(
        "INSERT INTO conflicts (
            conflict_id, file_id, original_path, conflict_path, created_at,
            base_remote_version, current_remote_version, origin_device
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
        params![
            new_id("conflict"),
            entry.file_id,
            entry.path,
            conflict_path,
            now,
            base_remote_version,
            current_remote_version,
        ],
    )?;

    if let Some(remote) = remote_resource {
        ensure_remote_row(conn, remote, SyncState::Synced)?;
    }
    Ok(())
}

fn rename_subtree(
    conn: &Connection,
    from: &str,
    to: &str,
    new_parent: &str,
    new_name: &str,
    now: i64,
) -> Result<(), SyncError> {
    let mut stmt = conn.prepare(
        "SELECT file_id, path FROM files WHERE path = ?1 OR path LIKE ?2 ORDER BY LENGTH(path) ASC",
    )?;
    let descendants = stmt.query_map(params![from, format!("{from}/%")], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in descendants {
        let (file_id, old_path) = row?;
        let suffix = old_path.strip_prefix(from).unwrap_or("");
        let new_path = format!("{to}{suffix}");
        let parent_path = if new_path == to {
            Some(new_parent.to_owned())
        } else {
            parent_path_of(&new_path)
        };
        let name = if new_path == to {
            new_name.to_owned()
        } else {
            name_of(&new_path).to_owned()
        };
        conn.execute(
            "UPDATE files SET path = ?2, parent_path = ?3, name = ?4, mtime = ?5 WHERE file_id = ?1",
            params![file_id, new_path, parent_path, name, now],
        )?;
    }
    Ok(())
}

fn status_from_entry(entry: StoredEntry) -> StatusEntry {
    let updated_at = entry.mtime.unwrap_or(0);
    StatusEntry {
        path: entry.path.clone(),
        name: entry.name.clone(),
        kind: match entry.kind {
            NodeKind::File => String::from("file"),
            NodeKind::Directory => String::from("directory"),
        },
        state: sync_state_label(entry.sync_state).to_owned(),
        direction: sync_direction(entry.sync_state).to_owned(),
        is_conflicted: entry.sync_state == SyncState::Conflict,
        is_placeholder: entry.content_state == ContentState::Placeholder,
        progress: if entry.sync_state == SyncState::Synced {
            100
        } else {
            0
        },
        bytes_done: 0,
        bytes_total: entry.size,
        updated_at,
        known: true,
    }
}

fn status_to_dict(entry: StatusEntry) -> HashMap<String, OwnedValue> {
    HashMap::from([
        (String::from("path"), ov_string(entry.path)),
        (String::from("name"), ov_string(entry.name)),
        (String::from("kind"), ov_string(entry.kind)),
        (String::from("state"), ov_string(entry.state)),
        (String::from("direction"), ov_string(entry.direction)),
        (
            String::from("is_conflicted"),
            OwnedValue::from(entry.is_conflicted),
        ),
        (
            String::from("is_placeholder"),
            OwnedValue::from(entry.is_placeholder),
        ),
        (String::from("progress"), OwnedValue::from(entry.progress)),
        (
            String::from("bytes_done"),
            OwnedValue::from(entry.bytes_done),
        ),
        (
            String::from("bytes_total"),
            OwnedValue::from(entry.bytes_total),
        ),
        (
            String::from("updated_at"),
            OwnedValue::from(entry.updated_at),
        ),
        (String::from("known"), OwnedValue::from(entry.known)),
    ])
}

fn sync_state_label(state: SyncState) -> &'static str {
    match state {
        SyncState::Synced => "synced",
        SyncState::QueuedUpload | SyncState::QueuedDelete => "queued",
        SyncState::Uploading => "uploading",
        SyncState::Downloading | SyncState::Placeholder => "downloading",
        SyncState::Conflict => "conflict",
        SyncState::Error => "error",
    }
}

fn sync_direction(state: SyncState) -> &'static str {
    match state {
        SyncState::Uploading | SyncState::QueuedUpload | SyncState::QueuedDelete => "upload",
        SyncState::Downloading | SyncState::Placeholder => "download",
        SyncState::Synced | SyncState::Conflict | SyncState::Error => "none",
    }
}

fn has_unsynced_local_changes(entry: &StoredEntry) -> bool {
    matches!(
        entry.sync_state,
        SyncState::QueuedUpload | SyncState::Uploading | SyncState::Error
    )
}

fn next_conflict_path(conn: &Connection, original_path: &str) -> Result<String, SyncError> {
    let parent = parent_path_of(original_path).unwrap_or_else(|| ROOT_PATH.to_owned());
    let mut used_names = HashSet::new();
    for child in query_children(conn, &parent)? {
        used_names.insert(child.name);
    }
    let name = name_of(original_path);
    Ok(join_remote_path(
        &parent,
        &next_conflict_name(name, |candidate| used_names.contains(candidate)),
    ))
}

pub fn next_conflict_name<F>(name: &str, exists: F) -> String
where
    F: Fn(&str) -> bool,
{
    let (base, ext, current_index) = split_conflict_name(name);
    let mut index = current_index.unwrap_or(1) + 1;
    loop {
        let candidate = format!("{base} ({index}){ext}");
        if !exists(&candidate) {
            return candidate;
        }
        index += 1;
    }
}

fn split_conflict_name(name: &str) -> (String, String, Option<u32>) {
    let (stem, ext) = split_extension(name);
    if let Some((base, index)) = strip_numeric_suffix(&stem) {
        return (base.to_owned(), ext, Some(index));
    }
    (stem, ext, None)
}

fn split_extension(name: &str) -> (String, String) {
    for compound in [".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst"] {
        if let Some(base) = name.strip_suffix(compound) {
            return (base.to_owned(), compound.to_owned());
        }
    }

    match name.rsplit_once('.') {
        Some((base, ext)) if !base.is_empty() => (base.to_owned(), format!(".{ext}")),
        _ => (name.to_owned(), String::new()),
    }
}

fn strip_numeric_suffix(stem: &str) -> Option<(&str, u32)> {
    let open = stem.rfind(" (")?;
    let suffix = stem.get(open + 2..stem.len() - 1)?;
    if !stem.ends_with(')') || suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let index = suffix.parse().ok()?;
    Some((&stem[..open], index))
}

pub fn join_remote_path(parent: &str, name: &str) -> String {
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
    path.rsplit_once('/').map(|(parent, _)| {
        if parent.is_empty() || parent == "disk:" {
            ROOT_PATH.to_owned()
        } else {
            parent.to_owned()
        }
    })
}

fn name_of(path: &str) -> &str {
    if path == ROOT_PATH {
        return ROOT_PATH;
    }
    path.rsplit('/').next().unwrap_or(path)
}

fn new_id(prefix: &str) -> String {
    format!("{prefix}-{:016x}", random::<u64>())
}

fn now_unix() -> i64 {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_secs() as i64,
        Err(_) => 0,
    }
}

fn system_time_to_unix(time: Option<std::time::SystemTime>) -> Option<i64> {
    time.and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
}

fn parse_payload(payload_json: &Option<String>) -> Value {
    payload_json
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_else(|| json!({}))
}

fn to_sql_error(err: SyncError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Integer, Box::new(err))
}

trait OptionalRemote<T> {
    fn optional(self) -> Result<Option<T>, YandexError>;
}

impl<T> OptionalRemote<T> for Result<T, YandexError> {
    fn optional(self) -> Result<Option<T>, YandexError> {
        match self {
            Ok(value) => Ok(Some(value)),
            Err(YandexError::NotFound) => Ok(None),
            Err(err) => Err(err),
        }
    }
}

fn ov<T>(value: T) -> OwnedValue
where
    OwnedValue: TryFrom<T>,
    <OwnedValue as TryFrom<T>>::Error: std::fmt::Debug,
{
    OwnedValue::try_from(value).expect("value must be convertible to OwnedValue")
}

fn ov_string(value: String) -> OwnedValue {
    OwnedValue::from(Str::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Mutex, time::SystemTime};

    #[derive(Default)]
    struct FakeRemoteClient {
        resources: Mutex<HashMap<String, ResourceEntry>>,
        contents: Mutex<HashMap<String, Vec<u8>>>,
        fail_uploads: Mutex<HashSet<String>>,
    }

    impl FakeRemoteClient {
        fn with_fixture() -> Arc<Self> {
            let client = Arc::new(Self::default());
            client.insert_dir(ROOT_PATH);
            client.insert_file("disk:/fast.txt", b"fast", "rev-fast");
            client
        }

        fn insert_dir(&self, path: &str) {
            let name = if path == ROOT_PATH {
                ROOT_PATH
            } else {
                name_of(path)
            };
            self.resources.lock().unwrap().insert(
                path.to_owned(),
                ResourceEntry {
                    path: path.to_owned(),
                    name: name.to_owned(),
                    kind: ResourceKind::Directory,
                    size: 0,
                    created: Some(SystemTime::UNIX_EPOCH),
                    modified: Some(SystemTime::UNIX_EPOCH),
                    remote_version: Some(format!("dir:{path}")),
                },
            );
        }

        fn insert_file(&self, path: &str, bytes: &[u8], remote_version: &str) {
            let name = name_of(path).to_owned();
            self.resources.lock().unwrap().insert(
                path.to_owned(),
                ResourceEntry {
                    path: path.to_owned(),
                    name,
                    kind: ResourceKind::File,
                    size: bytes.len() as u64,
                    created: Some(SystemTime::UNIX_EPOCH),
                    modified: Some(SystemTime::UNIX_EPOCH),
                    remote_version: Some(remote_version.to_owned()),
                },
            );
            self.contents
                .lock()
                .unwrap()
                .insert(path.to_owned(), bytes.to_vec());
        }

        fn fail_upload(&self, path: &str) {
            self.fail_uploads.lock().unwrap().insert(path.to_owned());
        }
    }

    impl RemoteSyncClient for FakeRemoteClient {
        fn fetch_resource_metadata(&self, path: &str) -> Result<ResourceEntry, YandexError> {
            self.resources
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or(YandexError::NotFound)
        }

        fn list_directory(&self, path: &str) -> Result<Vec<ResourceEntry>, YandexError> {
            self.fetch_resource_metadata(path)?;
            let prefix = if path == ROOT_PATH {
                String::from(ROOT_PATH)
            } else {
                format!("{path}/")
            };
            let mut children = Vec::new();
            for resource in self.resources.lock().unwrap().values() {
                if resource.path == path || !resource.path.starts_with(&prefix) {
                    continue;
                }
                let rest = &resource.path[prefix.len()..];
                if !rest.contains('/') {
                    children.push(resource.clone());
                }
            }
            children.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(children)
        }

        fn create_directory(&self, path: &str) -> Result<(), YandexError> {
            if self.resources.lock().unwrap().contains_key(path) {
                return Err(YandexError::Conflict(String::from("exists")));
            }
            self.insert_dir(path);
            Ok(())
        }

        fn delete_resource(&self, path: &str, _permanently: bool) -> Result<(), YandexError> {
            let keys: Vec<String> = self
                .resources
                .lock()
                .unwrap()
                .keys()
                .filter(|key| *key == path || key.starts_with(&format!("{path}/")))
                .cloned()
                .collect();
            if keys.is_empty() {
                return Err(YandexError::NotFound);
            }
            for key in keys {
                self.resources.lock().unwrap().remove(&key);
                self.contents.lock().unwrap().remove(&key);
            }
            Ok(())
        }

        fn move_resource(&self, from: &str, to: &str, _overwrite: bool) -> Result<(), YandexError> {
            let descendants: Vec<String> = self
                .resources
                .lock()
                .unwrap()
                .keys()
                .filter(|key| *key == from || key.starts_with(&format!("{from}/")))
                .cloned()
                .collect();
            if descendants.is_empty() {
                return Err(YandexError::NotFound);
            }
            for old_path in descendants {
                let suffix = old_path.strip_prefix(from).unwrap_or("");
                let new_path = format!("{to}{suffix}");
                let mut resource = self.resources.lock().unwrap().remove(&old_path).unwrap();
                resource.path = new_path.clone();
                resource.name = name_of(&new_path).to_owned();
                resource.remote_version = Some(format!("rev:{new_path}"));
                self.resources
                    .lock()
                    .unwrap()
                    .insert(new_path.clone(), resource);
                if let Some(bytes) = self.contents.lock().unwrap().remove(&old_path) {
                    self.contents.lock().unwrap().insert(new_path, bytes);
                }
            }
            Ok(())
        }

        fn resolve_download_url(&self, path: &str) -> Result<String, YandexError> {
            Ok(path.to_owned())
        }

        fn resolve_upload_url(&self, path: &str, _overwrite: bool) -> Result<String, YandexError> {
            Ok(path.to_owned())
        }

        fn upload_file(&self, href: &str, local_path: &Path) -> Result<(), YandexError> {
            if self.fail_uploads.lock().unwrap().contains(href) {
                return Err(YandexError::Forbidden);
            }
            let bytes = fs::read(local_path).map_err(YandexError::Io)?;
            self.insert_file(href, &bytes, &format!("rev:{href}:{}", bytes.len()));
            Ok(())
        }

        fn download_file(&self, href: &str) -> Result<Vec<u8>, YandexError> {
            self.contents
                .lock()
                .unwrap()
                .get(href)
                .cloned()
                .ok_or(YandexError::NotFound)
        }
    }

    fn temp_state_dir() -> PathBuf {
        let dir = env::temp_dir().join(format!("discohack-sync-test-{:016x}", random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fresh_service(client: Arc<FakeRemoteClient>) -> Arc<SyncService> {
        let remote: Arc<dyn RemoteSyncClient> = client;
        let service = SyncService::open_at(
            temp_state_dir(),
            remote,
            PathBuf::from("/tmp/discohack-test"),
        )
        .unwrap();
        if let Some(job) = service.lease_next_operation().unwrap() {
            service.process_operation(job).unwrap();
        }
        service
    }

    #[test]
    fn local_write_enqueues_upload_and_keeps_local_bytes() {
        let client = FakeRemoteClient::with_fixture();
        let service = fresh_service(client);
        service.create_file(ROOT_PATH, "offline.txt").unwrap();

        let path = join_remote_path(ROOT_PATH, "offline.txt");
        let staging = temp_state_dir().join("offline.txt");
        fs::write(&staging, b"hello offline").unwrap();
        let entry = service
            .apply_local_file_from_staging(&path, &staging)
            .unwrap();

        assert_eq!(entry.sync_state, SyncState::QueuedUpload);
        assert_eq!(service.load_local_bytes(&path).unwrap(), b"hello offline");
    }

    #[test]
    fn upload_job_completes_and_marks_file_synced() {
        let client = FakeRemoteClient::with_fixture();
        let service = fresh_service(Arc::clone(&client));
        service.create_file(ROOT_PATH, "sync-me.txt").unwrap();
        let path = join_remote_path(ROOT_PATH, "sync-me.txt");
        let staging = temp_state_dir().join("sync-me.txt");
        fs::write(&staging, b"sync me").unwrap();
        service
            .apply_local_file_from_staging(&path, &staging)
            .unwrap();

        let job = service.lease_next_operation().unwrap().unwrap();
        service.process_operation(job).unwrap();
        let entry = service.get_entry(&path).unwrap().unwrap();

        assert_eq!(entry.sync_state, SyncState::Synced);
        assert_eq!(client.download_file(&path).unwrap(), b"sync me");
    }

    #[test]
    fn repeated_uploads_coalesce_to_one_pending_job() {
        let client = FakeRemoteClient::with_fixture();
        let service = fresh_service(client);
        service.create_file(ROOT_PATH, "coalesce.txt").unwrap();
        let path = join_remote_path(ROOT_PATH, "coalesce.txt");
        let staging = temp_state_dir().join("coalesce.txt");
        fs::write(&staging, b"one").unwrap();
        service
            .apply_local_file_from_staging(&path, &staging)
            .unwrap();
        fs::write(&staging, b"two").unwrap();
        service
            .apply_local_file_from_staging(&path, &staging)
            .unwrap();

        let conn = service.db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM operations_queue WHERE file_id = (SELECT file_id FROM files WHERE path = ?1) AND op_type = ?2 AND op_status = ?3",
                params![path, QueueOpType::Upload as i32, QueueOpStatus::Pending as i32],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn restart_recovers_pending_jobs() {
        let client = FakeRemoteClient::with_fixture();
        let root_dir = temp_state_dir();
        let remote: Arc<dyn RemoteSyncClient> = client.clone();
        let service = SyncService::open_at(
            root_dir.clone(),
            remote,
            PathBuf::from("/tmp/discohack-test"),
        )
        .unwrap();
        service.create_file(ROOT_PATH, "restart.txt").unwrap();
        let reopened = SyncService::open_at(
            root_dir,
            client as Arc<dyn RemoteSyncClient>,
            PathBuf::from("/tmp/discohack-test"),
        )
        .unwrap();

        let entry = reopened
            .get_entry(&join_remote_path(ROOT_PATH, "restart.txt"))
            .unwrap()
            .unwrap();
        assert_eq!(entry.sync_state, SyncState::QueuedUpload);
    }

    #[test]
    fn remote_version_conflict_creates_conflict_copy() {
        let client = FakeRemoteClient::with_fixture();
        let service = fresh_service(Arc::clone(&client));
        let path = join_remote_path(ROOT_PATH, "fast.txt");
        let staging = temp_state_dir().join("fast.txt");
        fs::write(&staging, b"local change").unwrap();
        service
            .apply_local_file_from_staging(&path, &staging)
            .unwrap();
        client.insert_file(&path, b"remote change", "rev-remote-new");

        let job = service.lease_next_operation().unwrap().unwrap();
        service.process_operation(job).unwrap();

        assert!(service.get_entry(&path).unwrap().is_some());
        assert!(service.get_entry("disk:/fast (2).txt").unwrap().is_some());
    }

    #[test]
    fn conflict_name_helper_handles_suffixes_and_extensions() {
        let used = HashSet::from([
            String::from("file.txt"),
            String::from("file (2).txt"),
            String::from("archive.tar.gz"),
        ]);
        assert_eq!(
            next_conflict_name("file.txt", |name| used.contains(name)),
            "file (3).txt"
        );
        assert_eq!(
            next_conflict_name("file (2).txt", |_| false),
            "file (3).txt"
        );
        assert_eq!(next_conflict_name("plain", |_| false), "plain (2)");
        assert_eq!(
            next_conflict_name("archive.tar.gz", |_| false),
            "archive (2).tar.gz"
        );
    }

    #[test]
    fn sync_status_projection_reports_summary_and_directory_statuses() {
        let client = FakeRemoteClient::with_fixture();
        let service = fresh_service(client);
        let summary = service.sync_summary().unwrap();
        let root_statuses = service.list_directory_statuses(ROOT_PATH).unwrap();

        assert!(summary.last_update_unix > 0);
        assert!(!root_statuses.is_empty());
        assert!(root_statuses
            .iter()
            .any(|entry| entry.path == "disk:/fast.txt"));
    }
}
