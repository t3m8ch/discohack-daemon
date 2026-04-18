use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use fuser::{
    AccessFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, OpenAccMode, OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use rand::random;

use crate::yadisk::{ResourceEntry, ResourceKind, YandexDiskClient, YandexError};

const TTL: Duration = Duration::from_secs(2);
const METADATA_CACHE_TTL: Duration = Duration::from_secs(5);
const DOWNLOAD_URL_TTL: Duration = Duration::from_secs(300);
const ROOT_PATH: &str = "disk:/";

trait RemoteClient: Send + Sync {
    fn fetch_resource_metadata(&self, path: &str) -> Result<ResourceEntry, YandexError>;
    fn list_directory(&self, path: &str) -> Result<Vec<ResourceEntry>, YandexError>;
    fn create_directory(&self, path: &str) -> Result<(), YandexError>;
    fn delete_resource(&self, path: &str, permanently: bool) -> Result<(), YandexError>;
    fn move_resource(&self, from: &str, to: &str, overwrite: bool) -> Result<(), YandexError>;
    fn resolve_download_url(&self, path: &str) -> Result<String, YandexError>;
    fn resolve_upload_url(&self, path: &str, overwrite: bool) -> Result<String, YandexError>;
    fn upload_file(&self, href: &str, local_path: &std::path::Path) -> Result<(), YandexError>;
    fn download_file(&self, href: &str) -> Result<Vec<u8>, YandexError>;
    fn read_file_range(&self, href: &str, offset: u64, size: u32) -> Result<Vec<u8>, YandexError>;
}

impl RemoteClient for YandexDiskClient {
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

    fn upload_file(&self, href: &str, local_path: &std::path::Path) -> Result<(), YandexError> {
        YandexDiskClient::upload_file(self, href, local_path)
    }

    fn download_file(&self, href: &str) -> Result<Vec<u8>, YandexError> {
        YandexDiskClient::download_file(self, href)
    }

    fn read_file_range(&self, href: &str, offset: u64, size: u32) -> Result<Vec<u8>, YandexError> {
        YandexDiskClient::read_file_range(self, href, offset, size)
    }
}

pub struct YandexDiskFs {
    client: Arc<dyn RemoteClient>,
    state: Mutex<FsState>,
    uid: u32,
    gid: u32,
}

struct FsState {
    next_ino: u64,
    next_fh: u64,
    entries: HashMap<INodeNo, Entry>,
    path_to_ino: HashMap<String, INodeNo>,
    dir_children: HashMap<INodeNo, BTreeMap<String, INodeNo>>,
    dir_cache_time: HashMap<INodeNo, Instant>,
    write_handles: HashMap<FileHandle, WriteHandleState>,
}

#[derive(Clone)]
struct Entry {
    ino: INodeNo,
    parent: INodeNo,
    path: String,
    name: String,
    kind: EntryKind,
    size: u64,
    created: SystemTime,
    modified: SystemTime,
    cached_at: Instant,
    download_url: Option<String>,
    download_url_cached_at: Option<Instant>,
    remote_present: bool,
}

#[derive(Clone)]
struct WriteHandleSnapshot {
    ino: INodeNo,
    parent: INodeNo,
    path: String,
    staging_path: PathBuf,
    dirty: bool,
    is_new: bool,
}

struct WriteHandleState {
    ino: INodeNo,
    parent: INodeNo,
    path: String,
    staging_path: PathBuf,
    dirty: bool,
    is_new: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    Directory,
    File,
}

#[derive(Debug)]
enum FsError {
    NotFound,
    NotDir,
    IsDir,
    AlreadyExists,
    BadHandle,
    Io(std::io::Error),
    Remote(YandexError),
}

impl From<YandexError> for FsError {
    fn from(value: YandexError) -> Self {
        Self::Remote(value)
    }
}

impl From<std::io::Error> for FsError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

enum EntryRefreshPlan {
    Ready(Entry),
    Fetch { path: String, parent: INodeNo },
}

enum DirectoryLoadPlan {
    Ready,
    Fetch { path: String },
}

enum DownloadUrlPlan {
    Ready(String),
    Fetch { path: String },
}

impl YandexDiskFs {
    pub fn new(client: YandexDiskClient, uid: u32, gid: u32) -> Result<Self, YandexError> {
        Self::with_client(Arc::new(client), uid, gid)
    }

    fn with_client(client: Arc<dyn RemoteClient>, uid: u32, gid: u32) -> Result<Self, YandexError> {
        let root = client.fetch_resource_metadata(ROOT_PATH)?;
        let state = FsState::new(root);
        Ok(Self {
            client,
            state: Mutex::new(state),
            uid,
            gid,
        })
    }

    fn attr_for(&self, entry: &Entry) -> FileAttr {
        let is_dir = entry.kind == EntryKind::Directory;
        let size = if is_dir { 0 } else { entry.size };
        FileAttr {
            ino: entry.ino,
            size,
            blocks: size.div_ceil(512),
            atime: entry.modified,
            mtime: entry.modified,
            ctime: entry.modified,
            crtime: entry.created,
            kind: if is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if is_dir { 0o755 } else { 0o644 },
            nlink: if is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 512,
            flags: 0,
        }
    }

    fn lookup_entry(&self, parent: INodeNo, name: &str) -> Result<Entry, FsError> {
        self.ensure_directory_loaded(parent)?;
        let child_ino = {
            let state = self.state.lock().unwrap();
            state.lookup_cached_child(parent, name)?
        };
        self.getattr_entry(child_ino)
    }

    fn getattr_entry(&self, ino: INodeNo) -> Result<Entry, FsError> {
        self.ensure_entry_fresh(ino)
    }

    fn readdir_listing(&self, ino: INodeNo) -> Result<Vec<(INodeNo, FileType, String)>, FsError> {
        self.ensure_directory_loaded(ino)?;
        let state = self.state.lock().unwrap();
        state.readdir_snapshot(ino)
    }

    fn read_data(
        &self,
        ino: INodeNo,
        fh: Option<FileHandle>,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, FsError> {
        if let Some(fh) = fh {
            if let Ok(bytes) = self.read_handle_data(fh, offset, size) {
                return Ok(bytes);
            }
        }

        let entry = self.getattr_entry(ino)?;
        if entry.kind != EntryKind::File {
            return Err(FsError::IsDir);
        }
        if offset >= entry.size || size == 0 {
            return Ok(Vec::new());
        }

        let href = self.download_url_for(ino)?;
        match self.client.read_file_range(&href, offset, size) {
            Ok(bytes) => Ok(bytes),
            Err(YandexError::Forbidden)
            | Err(YandexError::Unauthorized)
            | Err(YandexError::NotFound) => {
                self.invalidate_download_url(ino);
                let refreshed = self.download_url_for(ino)?;
                Ok(self.client.read_file_range(&refreshed, offset, size)?)
            }
            Err(err) => Err(err.into()),
        }
    }

    fn read_handle_data(&self, fh: FileHandle, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
        let snapshot = {
            let state = self.state.lock().unwrap();
            state.write_handle_snapshot(fh)?
        };
        read_local_range(&snapshot.staging_path, offset, size)
    }

    fn ensure_entry_fresh(&self, ino: INodeNo) -> Result<Entry, FsError> {
        loop {
            let plan = {
                let state = self.state.lock().unwrap();
                state.entry_refresh_plan(ino)?
            };

            match plan {
                EntryRefreshPlan::Ready(entry) => return Ok(entry),
                EntryRefreshPlan::Fetch { path, parent } => {
                    let fresh = self.client.fetch_resource_metadata(&path)?;
                    let mut state = self.state.lock().unwrap();
                    state.upsert_entry(parent, fresh);
                }
            }
        }
    }

    fn ensure_directory_loaded(&self, ino: INodeNo) -> Result<(), FsError> {
        loop {
            let entry = self.ensure_entry_fresh(ino)?;
            if entry.kind != EntryKind::Directory {
                return Err(FsError::NotDir);
            }

            let plan = {
                let state = self.state.lock().unwrap();
                state.directory_load_plan(ino)?
            };

            match plan {
                DirectoryLoadPlan::Ready => return Ok(()),
                DirectoryLoadPlan::Fetch { path } => {
                    let children = self.client.list_directory(&path)?;
                    let mut state = self.state.lock().unwrap();
                    state.replace_directory_children(ino, children);
                }
            }
        }
    }

    fn download_url_for(&self, ino: INodeNo) -> Result<String, FsError> {
        loop {
            let plan = {
                let state = self.state.lock().unwrap();
                state.download_url_plan(ino)?
            };

            match plan {
                DownloadUrlPlan::Ready(url) => return Ok(url),
                DownloadUrlPlan::Fetch { path } => {
                    let url = self.client.resolve_download_url(&path)?;
                    let mut state = self.state.lock().unwrap();
                    state.cache_download_url(ino, url.clone())?;
                    return Ok(url);
                }
            }
        }
    }

    fn invalidate_download_url(&self, ino: INodeNo) {
        let mut state = self.state.lock().unwrap();
        state.invalidate_download_url(ino);
    }

    fn has_write_handle(&self, fh: FileHandle) -> bool {
        let state = self.state.lock().unwrap();
        state.write_handles.contains_key(&fh)
    }

    fn open_file_handle(
        &self,
        ino: INodeNo,
        writable: bool,
        truncate: bool,
    ) -> Result<FileHandle, FsError> {
        let entry = self.getattr_entry(ino)?;
        if entry.kind != EntryKind::File {
            return Err(FsError::IsDir);
        }

        if !writable {
            return Ok(FileHandle(0));
        }

        let initial = if entry.size == 0 {
            Vec::new()
        } else {
            let href = self.download_url_for(ino)?;
            self.client.download_file(&href)?
        };
        let staging_path = create_staging_file(&initial)?;
        if truncate {
            fs::OpenOptions::new()
                .write(true)
                .open(&staging_path)?
                .set_len(0)?;
        }

        let mut state = self.state.lock().unwrap();
        let fh = state.allocate_file_handle();
        state.write_handles.insert(
            fh,
            WriteHandleState {
                ino,
                parent: entry.parent,
                path: entry.path.clone(),
                staging_path: staging_path.clone(),
                dirty: truncate,
                is_new: false,
            },
        );
        if truncate {
            state.update_entry_size(ino, 0)?;
            state.invalidate_download_url(ino);
        }
        Ok(fh)
    }

    fn create_pending_file(
        &self,
        parent: INodeNo,
        name: &str,
    ) -> Result<(Entry, FileHandle), FsError> {
        self.ensure_directory_loaded(parent)?;
        let parent_entry = self.getattr_entry(parent)?;
        if parent_entry.kind != EntryKind::Directory {
            return Err(FsError::NotDir);
        }
        let path = join_remote_path(&parent_entry.path, name);
        let staging_path = create_staging_file(&[])?;

        let mut state = self.state.lock().unwrap();
        if state.path_to_ino.contains_key(&path)
            || state
                .dir_children
                .get(&parent)
                .and_then(|children| children.get(name))
                .is_some()
        {
            return Err(FsError::AlreadyExists);
        }

        let now = SystemTime::now();
        let ino = state.allocate_ino();
        let fh = state.allocate_file_handle();
        let entry = Entry {
            ino,
            parent,
            path: path.clone(),
            name: name.to_owned(),
            kind: EntryKind::File,
            size: 0,
            created: now,
            modified: now,
            cached_at: Instant::now(),
            download_url: None,
            download_url_cached_at: None,
            remote_present: false,
        };
        state.entries.insert(ino, entry.clone());
        state.write_handles.insert(
            fh,
            WriteHandleState {
                ino,
                parent,
                path,
                staging_path,
                dirty: false,
                is_new: true,
            },
        );
        Ok((entry, fh))
    }

    fn write_handle_data(&self, fh: FileHandle, offset: u64, data: &[u8]) -> Result<u32, FsError> {
        let snapshot = {
            let state = self.state.lock().unwrap();
            state.write_handle_snapshot(fh)?
        };
        write_local_range(&snapshot.staging_path, offset, data)?;
        let size = fs::metadata(&snapshot.staging_path)?.len();
        let mut state = self.state.lock().unwrap();
        state.mark_handle_dirty(fh)?;
        state.update_entry_size(snapshot.ino, size)?;
        state.invalidate_download_url(snapshot.ino);
        Ok(data.len() as u32)
    }

    fn truncate_handle(&self, fh: FileHandle, size: u64) -> Result<Entry, FsError> {
        let snapshot = {
            let state = self.state.lock().unwrap();
            state.write_handle_snapshot(fh)?
        };
        fs::OpenOptions::new()
            .write(true)
            .open(&snapshot.staging_path)?
            .set_len(size)?;

        let mut state = self.state.lock().unwrap();
        state.mark_handle_dirty(fh)?;
        state.update_entry_size(snapshot.ino, size)?;
        state.invalidate_download_url(snapshot.ino);
        state
            .entries
            .get(&snapshot.ino)
            .cloned()
            .ok_or(FsError::NotFound)
    }

    fn truncate_entry(
        &self,
        ino: INodeNo,
        fh: Option<FileHandle>,
        size: u64,
    ) -> Result<Entry, FsError> {
        if let Some(fh) = fh {
            return self.truncate_handle(fh, size);
        }

        let temp_handle = self.open_file_handle(ino, true, false)?;
        let truncated = self.truncate_handle(temp_handle, size)?;
        self.commit_write_handle(temp_handle)?;
        self.finish_write_handle(temp_handle, true)?;
        Ok(truncated)
    }

    fn commit_write_handle(&self, fh: FileHandle) -> Result<(), FsError> {
        let snapshot = {
            let state = self.state.lock().unwrap();
            state.write_handle_snapshot(fh)?
        };
        if !snapshot.dirty {
            return Ok(());
        }

        let href = self.client.resolve_upload_url(&snapshot.path, true)?;
        self.client.upload_file(&href, &snapshot.staging_path)?;
        let fresh = self.client.fetch_resource_metadata(&snapshot.path)?;

        let mut state = self.state.lock().unwrap();
        state.apply_committed_resource(fh, fresh)?;
        Ok(())
    }

    fn finish_write_handle(&self, fh: FileHandle, cleanup_on_success: bool) -> Result<(), FsError> {
        let snapshot = {
            let mut state = self.state.lock().unwrap();
            state.remove_write_handle(fh)?
        };

        if cleanup_on_success {
            let _ = fs::remove_file(snapshot.staging_path);
        }
        Ok(())
    }

    fn mkdir_entry(&self, parent: INodeNo, name: &str) -> Result<Entry, FsError> {
        self.ensure_directory_loaded(parent)?;
        let parent_entry = self.getattr_entry(parent)?;
        if parent_entry.kind != EntryKind::Directory {
            return Err(FsError::NotDir);
        }
        let path = join_remote_path(&parent_entry.path, name);
        self.client.create_directory(&path)?;
        let fresh = self.client.fetch_resource_metadata(&path)?;

        let mut state = self.state.lock().unwrap();
        let ino = state.upsert_entry(parent, fresh);
        state.mark_directory_loaded_stale(parent);
        state.entries.get(&ino).cloned().ok_or(FsError::NotFound)
    }

    fn unlink_entry(&self, parent: INodeNo, name: &str, expect_dir: bool) -> Result<(), FsError> {
        let entry = self.lookup_entry(parent, name)?;
        match (expect_dir, entry.kind) {
            (true, EntryKind::File) => return Err(FsError::NotDir),
            (false, EntryKind::Directory) => return Err(FsError::IsDir),
            _ => {}
        }

        self.client.delete_resource(&entry.path, true)?;
        let mut state = self.state.lock().unwrap();
        state.remove_entry_recursive(entry.ino);
        state.mark_directory_loaded_stale(parent);
        Ok(())
    }

    fn rename_entry(
        &self,
        parent: INodeNo,
        name: &str,
        newparent: INodeNo,
        newname: &str,
    ) -> Result<(), FsError> {
        let entry = self.lookup_entry(parent, name)?;
        let new_parent_entry = self.getattr_entry(newparent)?;
        if new_parent_entry.kind != EntryKind::Directory {
            return Err(FsError::NotDir);
        }
        let new_path = join_remote_path(&new_parent_entry.path, newname);
        let old_path = entry.path.clone();

        self.client.move_resource(&old_path, &new_path, true)?;
        let fresh = self.client.fetch_resource_metadata(&new_path)?;

        let mut state = self.state.lock().unwrap();
        if let Some(existing) = state.path_to_ino.get(&new_path).copied() {
            if existing != entry.ino {
                state.remove_entry_recursive(existing);
            }
        }
        state.rename_entry_subtree(entry.ino, newparent, newname, &new_path);
        state.upsert_entry(newparent, fresh);
        state.mark_directory_loaded_stale(parent);
        state.mark_directory_loaded_stale(newparent);
        Ok(())
    }
}

impl Filesystem for YandexDiskFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEntry) {
        let name = match name.to_str() {
            Some(name) => name,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        match self.lookup_entry(parent, name) {
            Ok(entry) => reply.entry(&TTL, &self.attr_for(&entry), Generation(0)),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.getattr_entry(ino) {
            Ok(entry) => reply.attr(&TTL, &self.attr_for(&entry)),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn access(&self, _req: &Request, ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        let state = self.state.lock().unwrap();
        if state.entries.contains_key(&ino) {
            reply.ok();
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.getattr_entry(ino) {
            Ok(entry) if entry.kind == EntryKind::Directory => {
                reply.opened(FileHandle(0), FopenFlags::empty())
            }
            Ok(_) => reply.error(Errno::ENOTDIR),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        match self.readdir_listing(ino) {
            Ok(entries) => {
                for (index, (entry_ino, kind, name)) in
                    entries.into_iter().enumerate().skip(offset as usize)
                {
                    let next_offset = (index + 1) as u64;
                    if reply.add(entry_ino, next_offset, kind, name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let writable = flags.acc_mode() != OpenAccMode::O_RDONLY;
        let truncate = flags.0 & libc::O_TRUNC != 0;
        match self.open_file_handle(ino, writable, truncate) {
            Ok(handle) => reply.opened(handle, FopenFlags::empty()),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.read_data(ino, Some(fh), offset, size) {
            Ok(data) => reply.data(&data),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let result = match size {
            Some(size) => self.truncate_entry(ino, fh, size),
            None => self.getattr_entry(ino),
        };
        match result {
            Ok(entry) => reply.attr(&TTL, &self.attr_for(&entry)),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &std::ffi::OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EOPNOTSUPP);
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.mkdir_entry(parent, name) {
            Ok(entry) => reply.entry(&TTL, &self.attr_for(&entry), Generation(0)),
            Err(FsError::Remote(YandexError::Conflict(_))) | Err(FsError::AlreadyExists) => {
                reply.error(Errno::EEXIST)
            }
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.unlink_entry(parent, name, false) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &std::ffi::OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.unlink_entry(parent, name, true) {
            Ok(()) => reply.ok(),
            Err(FsError::Remote(YandexError::Conflict(_))) => reply.error(Errno::ENOTEMPTY),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        newparent: INodeNo,
        newname: &std::ffi::OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let (Some(name), Some(newname)) = (name.to_str(), newname.to_str()) else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.rename_entry(parent, name, newparent, newname) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        match self.write_handle_data(fh, offset, data) {
            Ok(written) => reply.written(written),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &std::ffi::OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.create_pending_file(parent, name) {
            Ok((entry, fh)) => reply.created(
                &TTL,
                &self.attr_for(&entry),
                Generation(0),
                fh,
                FopenFlags::empty(),
            ),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        if fh == FileHandle(0) || !self.has_write_handle(fh) {
            reply.ok();
            return;
        }

        match self.commit_write_handle(fh) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        if fh == FileHandle(0) || !self.has_write_handle(fh) {
            reply.ok();
            return;
        }

        match self.commit_write_handle(fh) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let commit_result = if fh == FileHandle(0) {
            Ok(())
        } else {
            self.commit_write_handle(fh)
        };
        match commit_result {
            Ok(()) => {
                if fh != FileHandle(0) {
                    let _ = self.finish_write_handle(fh, true);
                }
                reply.ok();
            }
            Err(err) => {
                let _ = self.finish_write_handle(fh, false);
                reply.error(errno_for(&err));
            }
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }
}

impl FsState {
    fn new(root: ResourceEntry) -> Self {
        let mut state = Self {
            next_ino: 2,
            next_fh: 1,
            entries: HashMap::new(),
            path_to_ino: HashMap::new(),
            dir_children: HashMap::new(),
            dir_cache_time: HashMap::new(),
            write_handles: HashMap::new(),
        };

        let root_entry = Entry::from_resource(INodeNo::ROOT, INodeNo::ROOT, root);
        state.entries.insert(INodeNo::ROOT, root_entry);
        state
            .path_to_ino
            .insert(ROOT_PATH.to_owned(), INodeNo::ROOT);
        state
    }

    fn allocate_ino(&mut self) -> INodeNo {
        let ino = INodeNo(self.next_ino);
        self.next_ino += 1;
        ino
    }

    fn allocate_file_handle(&mut self) -> FileHandle {
        let fh = FileHandle(self.next_fh);
        self.next_fh += 1;
        fh
    }

    fn has_active_write_handle(&self, ino: INodeNo) -> bool {
        self.write_handles.values().any(|handle| handle.ino == ino)
    }

    fn entry_refresh_plan(&self, ino: INodeNo) -> Result<EntryRefreshPlan, FsError> {
        let entry = self.entries.get(&ino).cloned().ok_or(FsError::NotFound)?;
        if !entry.remote_present
            || self.has_active_write_handle(ino)
            || entry.cached_at.elapsed() < METADATA_CACHE_TTL
        {
            return Ok(EntryRefreshPlan::Ready(entry));
        }

        Ok(EntryRefreshPlan::Fetch {
            path: entry.path,
            parent: entry.parent,
        })
    }

    fn directory_load_plan(&self, ino: INodeNo) -> Result<DirectoryLoadPlan, FsError> {
        let entry = self.entries.get(&ino).ok_or(FsError::NotFound)?;
        if entry.kind != EntryKind::Directory {
            return Err(FsError::NotDir);
        }

        let is_fresh = self
            .dir_cache_time
            .get(&ino)
            .map(|cached| cached.elapsed() < METADATA_CACHE_TTL)
            .unwrap_or(false);
        if is_fresh {
            return Ok(DirectoryLoadPlan::Ready);
        }

        Ok(DirectoryLoadPlan::Fetch {
            path: entry.path.clone(),
        })
    }

    fn download_url_plan(&self, ino: INodeNo) -> Result<DownloadUrlPlan, FsError> {
        let entry = self.entries.get(&ino).ok_or(FsError::NotFound)?;
        if entry.kind != EntryKind::File {
            return Err(FsError::IsDir);
        }

        if let (Some(url), Some(cached_at)) = (&entry.download_url, entry.download_url_cached_at) {
            if cached_at.elapsed() < DOWNLOAD_URL_TTL {
                return Ok(DownloadUrlPlan::Ready(url.clone()));
            }
        }

        Ok(DownloadUrlPlan::Fetch {
            path: entry.path.clone(),
        })
    }

    fn lookup_cached_child(&self, parent: INodeNo, name: &str) -> Result<INodeNo, FsError> {
        self.dir_children
            .get(&parent)
            .and_then(|children| children.get(name).copied())
            .ok_or(FsError::NotFound)
    }

    fn readdir_snapshot(&self, ino: INodeNo) -> Result<Vec<(INodeNo, FileType, String)>, FsError> {
        let entry = self.entries.get(&ino).cloned().ok_or(FsError::NotFound)?;
        if entry.kind != EntryKind::Directory {
            return Err(FsError::NotDir);
        }

        let mut entries = vec![
            (entry.ino, FileType::Directory, String::from(".")),
            (
                if entry.ino == INodeNo::ROOT {
                    INodeNo::ROOT
                } else {
                    entry.parent
                },
                FileType::Directory,
                String::from(".."),
            ),
        ];

        if let Some(children) = self.dir_children.get(&ino) {
            for (name, child_ino) in children {
                if let Some(child) = self.entries.get(child_ino) {
                    entries.push((
                        *child_ino,
                        if child.kind == EntryKind::Directory {
                            FileType::Directory
                        } else {
                            FileType::RegularFile
                        },
                        name.clone(),
                    ));
                }
            }
        }

        Ok(entries)
    }

    fn replace_directory_children(&mut self, parent: INodeNo, children: Vec<ResourceEntry>) {
        let mut mapped = BTreeMap::new();
        for child in children {
            let name = child.name.clone();
            let child_ino = self.upsert_entry(parent, child);
            mapped.insert(name, child_ino);
        }

        self.dir_children.insert(parent, mapped);
        self.dir_cache_time.insert(parent, Instant::now());
    }

    fn cache_download_url(&mut self, ino: INodeNo, url: String) -> Result<(), FsError> {
        let entry = self.entries.get_mut(&ino).ok_or(FsError::NotFound)?;
        if entry.kind != EntryKind::File {
            return Err(FsError::IsDir);
        }

        entry.download_url = Some(url);
        entry.download_url_cached_at = Some(Instant::now());
        Ok(())
    }

    fn invalidate_download_url(&mut self, ino: INodeNo) {
        if let Some(entry) = self.entries.get_mut(&ino) {
            entry.download_url = None;
            entry.download_url_cached_at = None;
        }
    }

    fn upsert_entry(&mut self, parent: INodeNo, resource: ResourceEntry) -> INodeNo {
        if let Some(&ino) = self.path_to_ino.get(&resource.path) {
            if let Some(entry) = self.entries.get_mut(&ino) {
                entry.parent = parent;
                entry.update_from_resource(resource);
            }
            return ino;
        }

        let ino = self.allocate_ino();
        let path = resource.path.clone();
        let entry = Entry::from_resource(ino, parent, resource);
        self.path_to_ino.insert(path, ino);
        self.entries.insert(ino, entry);
        ino
    }

    fn write_handle_snapshot(&self, fh: FileHandle) -> Result<WriteHandleSnapshot, FsError> {
        let handle = self.write_handles.get(&fh).ok_or(FsError::BadHandle)?;
        Ok(WriteHandleSnapshot {
            ino: handle.ino,
            parent: handle.parent,
            path: handle.path.clone(),
            staging_path: handle.staging_path.clone(),
            dirty: handle.dirty,
            is_new: handle.is_new,
        })
    }

    fn mark_handle_dirty(&mut self, fh: FileHandle) -> Result<(), FsError> {
        let handle = self.write_handles.get_mut(&fh).ok_or(FsError::BadHandle)?;
        handle.dirty = true;
        Ok(())
    }

    fn update_entry_size(&mut self, ino: INodeNo, size: u64) -> Result<(), FsError> {
        let entry = self.entries.get_mut(&ino).ok_or(FsError::NotFound)?;
        entry.size = size;
        entry.modified = SystemTime::now();
        entry.cached_at = Instant::now();
        Ok(())
    }

    fn apply_committed_resource(
        &mut self,
        fh: FileHandle,
        fresh: ResourceEntry,
    ) -> Result<(), FsError> {
        let (ino, parent) = {
            let handle = self.write_handles.get(&fh).ok_or(FsError::BadHandle)?;
            (handle.ino, handle.parent)
        };
        let entry = self.entries.get_mut(&ino).ok_or(FsError::NotFound)?;
        entry.parent = parent;
        entry.path = fresh.path.clone();
        entry.update_from_resource(fresh.clone());
        entry.remote_present = true;
        self.path_to_ino.insert(fresh.path.clone(), ino);
        let handle = self.write_handles.get_mut(&fh).ok_or(FsError::BadHandle)?;
        handle.path = fresh.path.clone();
        handle.dirty = false;
        handle.is_new = false;
        self.mark_directory_loaded_stale(parent);
        Ok(())
    }

    fn remove_write_handle(&mut self, fh: FileHandle) -> Result<WriteHandleSnapshot, FsError> {
        let handle = self.write_handles.remove(&fh).ok_or(FsError::BadHandle)?;
        Ok(WriteHandleSnapshot {
            ino: handle.ino,
            parent: handle.parent,
            path: handle.path,
            staging_path: handle.staging_path,
            dirty: handle.dirty,
            is_new: handle.is_new,
        })
    }

    fn mark_directory_loaded_stale(&mut self, ino: INodeNo) {
        self.dir_children.remove(&ino);
        self.dir_cache_time.remove(&ino);
    }

    fn remove_entry_recursive(&mut self, ino: INodeNo) {
        if let Some(children) = self.dir_children.remove(&ino) {
            for child_ino in children.into_values() {
                self.remove_entry_recursive(child_ino);
            }
        }
        self.dir_cache_time.remove(&ino);
        if let Some(entry) = self.entries.remove(&ino) {
            self.path_to_ino.remove(&entry.path);
        }
    }

    fn rename_entry_subtree(
        &mut self,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &str,
        new_path: &str,
    ) {
        let old_path = match self.entries.get(&ino) {
            Some(entry) => entry.path.clone(),
            None => return,
        };
        let descendants = self.collect_subtree(ino);
        for desc_ino in descendants {
            if let Some(entry) = self.entries.get_mut(&desc_ino) {
                self.path_to_ino.remove(&entry.path);
                let suffix = entry.path.strip_prefix(&old_path).unwrap_or("");
                entry.path = format!("{new_path}{suffix}");
                if desc_ino == ino {
                    entry.parent = newparent;
                    entry.name = newname.to_owned();
                }
                entry.cached_at = Instant::now();
                self.path_to_ino.insert(entry.path.clone(), desc_ino);
            }
        }
        for handle in self.write_handles.values_mut() {
            if let Some(suffix) = handle.path.strip_prefix(&old_path) {
                handle.path = format!("{new_path}{suffix}");
                if handle.ino == ino {
                    handle.parent = newparent;
                }
            }
        }
    }

    fn collect_subtree(&self, ino: INodeNo) -> Vec<INodeNo> {
        let mut nodes = vec![ino];
        if let Some(children) = self.dir_children.get(&ino) {
            for child in children.values() {
                nodes.extend(self.collect_subtree(*child));
            }
        }
        nodes
    }
}

impl Entry {
    fn from_resource(ino: INodeNo, parent: INodeNo, resource: ResourceEntry) -> Self {
        let created = resource
            .created
            .or(resource.modified)
            .unwrap_or_else(SystemTime::now);
        let modified = resource.modified.or(resource.created).unwrap_or(created);
        Self {
            ino,
            parent,
            path: resource.path,
            name: resource.name,
            kind: match resource.kind {
                ResourceKind::Directory => EntryKind::Directory,
                ResourceKind::File => EntryKind::File,
            },
            size: resource.size,
            created,
            modified,
            cached_at: Instant::now(),
            download_url: None,
            download_url_cached_at: None,
            remote_present: true,
        }
    }

    fn update_from_resource(&mut self, resource: ResourceEntry) {
        self.name = resource.name;
        self.kind = match resource.kind {
            ResourceKind::Directory => EntryKind::Directory,
            ResourceKind::File => EntryKind::File,
        };
        self.size = resource.size;
        self.created = resource
            .created
            .or(resource.modified)
            .unwrap_or(self.created);
        self.modified = resource
            .modified
            .or(resource.created)
            .unwrap_or(self.modified);
        self.cached_at = Instant::now();
        self.remote_present = true;
        if self.kind == EntryKind::Directory {
            self.download_url = None;
            self.download_url_cached_at = None;
        } else {
            self.download_url = None;
            self.download_url_cached_at = None;
        }
    }
}

fn join_remote_path(parent: &str, name: &str) -> String {
    if parent == ROOT_PATH {
        format!("{ROOT_PATH}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn create_staging_file(initial: &[u8]) -> Result<PathBuf, FsError> {
    let dir = env::temp_dir();
    for _ in 0..32 {
        let candidate = dir.join(format!(
            "discohack-staging-{}-{:016x}",
            std::process::id(),
            random::<u64>()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                file.write_all(initial)?;
                return Ok(candidate);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        }
    }

    Err(FsError::Io(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "failed to allocate unique staging file",
    )))
}

fn read_local_range(path: &std::path::Path, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
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

fn write_local_range(path: &std::path::Path, offset: u64, data: &[u8]) -> Result<(), FsError> {
    let mut file = fs::OpenOptions::new().write(true).open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)?;
    Ok(())
}

fn errno_for(error: &FsError) -> Errno {
    match error {
        FsError::NotFound => Errno::ENOENT,
        FsError::NotDir => Errno::ENOTDIR,
        FsError::IsDir => Errno::EISDIR,
        FsError::AlreadyExists => Errno::EEXIST,
        FsError::BadHandle => Errno::EBADF,
        FsError::Io(_) => Errno::EIO,
        FsError::Remote(YandexError::NotFound) => Errno::ENOENT,
        FsError::Remote(YandexError::Unauthorized)
        | FsError::Remote(YandexError::Forbidden)
        | FsError::Remote(YandexError::Auth(_)) => Errno::EACCES,
        FsError::Remote(YandexError::Conflict(_)) => Errno::EEXIST,
        FsError::Remote(YandexError::InvalidResponse(_))
        | FsError::Remote(YandexError::Http(_))
        | FsError::Remote(YandexError::Io(_))
        | FsError::Remote(YandexError::Status { .. }) => Errno::EIO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashSet,
        thread,
        time::{Duration, Instant},
    };

    #[derive(Default)]
    struct FakeClient {
        resources: Mutex<HashMap<String, ResourceEntry>>,
        download_urls: Mutex<HashMap<String, String>>,
        file_contents: Mutex<HashMap<String, Vec<u8>>>,
        metadata_delays: Mutex<HashMap<String, Duration>>,
        read_delays: Mutex<HashMap<String, Duration>>,
        upload_delays: Mutex<HashMap<String, Duration>>,
        upload_failures: Mutex<HashSet<String>>,
    }

    impl FakeClient {
        fn with_fixture() -> Arc<Self> {
            let client = Arc::new(Self::default());
            client.insert_resource(dir("disk:/", "disk"));
            client.insert_resource(file("disk:/slow.txt", "slow.txt", 11));
            client.insert_resource(file("disk:/fast.txt", "fast.txt", 11));
            client.set_download_url("disk:/slow.txt", "download://slow.txt");
            client.set_download_url("disk:/fast.txt", "download://fast.txt");
            client.set_file_content("download://slow.txt", b"slow-content".to_vec());
            client.set_file_content("download://fast.txt", b"fast-content".to_vec());
            client
        }

        fn insert_resource(&self, entry: ResourceEntry) {
            self.resources
                .lock()
                .unwrap()
                .insert(entry.path.clone(), entry);
        }

        fn set_download_url(&self, path: &str, href: &str) {
            self.download_urls
                .lock()
                .unwrap()
                .insert(path.to_owned(), href.to_owned());
        }

        fn set_file_content(&self, href: &str, bytes: Vec<u8>) {
            self.file_contents
                .lock()
                .unwrap()
                .insert(href.to_owned(), bytes);
        }

        fn set_metadata_delay(&self, path: &str, delay: Duration) {
            self.metadata_delays
                .lock()
                .unwrap()
                .insert(path.to_owned(), delay);
        }

        fn set_read_delay(&self, href: &str, delay: Duration) {
            self.read_delays
                .lock()
                .unwrap()
                .insert(href.to_owned(), delay);
        }

        fn set_upload_delay(&self, path: &str, delay: Duration) {
            self.upload_delays
                .lock()
                .unwrap()
                .insert(path.to_owned(), delay);
        }

        fn fail_upload_for(&self, path: &str) {
            self.upload_failures.lock().unwrap().insert(path.to_owned());
        }

        fn direct_children(&self, parent: &str) -> Vec<ResourceEntry> {
            let resources = self.resources.lock().unwrap();
            let mut children = Vec::new();
            let prefix = if parent == ROOT_PATH {
                ROOT_PATH.to_owned()
            } else {
                format!("{parent}/")
            };
            for (path, entry) in resources.iter() {
                if path == parent || !path.starts_with(&prefix) {
                    continue;
                }
                let suffix = &path[prefix.len()..];
                if !suffix.is_empty() && !suffix.contains('/') {
                    children.push(entry.clone());
                }
            }
            children.sort_by(|a, b| a.name.cmp(&b.name));
            children
        }

        fn remove_subtree(&self, path: &str) {
            let keys: Vec<String> = self
                .resources
                .lock()
                .unwrap()
                .keys()
                .filter(|key| *key == path || key.starts_with(&format!("{path}/")))
                .cloned()
                .collect();
            for key in keys {
                self.resources.lock().unwrap().remove(&key);
                if let Some(href) = self.download_urls.lock().unwrap().remove(&key) {
                    self.file_contents.lock().unwrap().remove(&href);
                }
            }
        }
    }

    impl RemoteClient for FakeClient {
        fn fetch_resource_metadata(&self, path: &str) -> Result<ResourceEntry, YandexError> {
            if let Some(delay) = self.metadata_delays.lock().unwrap().get(path).copied() {
                thread::sleep(delay);
            }
            self.resources
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or(YandexError::NotFound)
        }

        fn list_directory(&self, path: &str) -> Result<Vec<ResourceEntry>, YandexError> {
            self.resources
                .lock()
                .unwrap()
                .get(path)
                .filter(|entry| entry.kind == ResourceKind::Directory)
                .ok_or(YandexError::NotFound)?;
            Ok(self.direct_children(path))
        }

        fn create_directory(&self, path: &str) -> Result<(), YandexError> {
            if self.resources.lock().unwrap().contains_key(path) {
                return Err(YandexError::Conflict(String::from("already exists")));
            }
            let name = path.rsplit('/').next().unwrap_or(path);
            self.insert_resource(dir(path, name));
            Ok(())
        }

        fn delete_resource(&self, path: &str, _permanently: bool) -> Result<(), YandexError> {
            let entry = self
                .resources
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or(YandexError::NotFound)?;
            if entry.kind == ResourceKind::Directory && !self.direct_children(path).is_empty() {
                return Err(YandexError::Conflict(String::from("directory not empty")));
            }
            self.remove_subtree(path);
            Ok(())
        }

        fn move_resource(&self, from: &str, to: &str, _overwrite: bool) -> Result<(), YandexError> {
            let descendants: Vec<ResourceEntry> = self
                .resources
                .lock()
                .unwrap()
                .values()
                .filter(|entry| entry.path == from || entry.path.starts_with(&format!("{from}/")))
                .cloned()
                .collect();
            if descendants.is_empty() {
                return Err(YandexError::NotFound);
            }
            self.remove_subtree(to);
            for entry in descendants {
                let suffix = entry.path.strip_prefix(from).unwrap_or("");
                let new_path = format!("{to}{suffix}");
                let name = new_path.rsplit('/').next().unwrap_or(&new_path).to_owned();
                let mut updated = entry.clone();
                updated.path = new_path.clone();
                updated.name = name;
                self.resources.lock().unwrap().remove(&entry.path);
                self.resources
                    .lock()
                    .unwrap()
                    .insert(new_path.clone(), updated);
                let old_href = { self.download_urls.lock().unwrap().remove(&entry.path) };
                if let Some(href) = old_href {
                    let new_href = format!("download://{new_path}");
                    let bytes = self
                        .file_contents
                        .lock()
                        .unwrap()
                        .remove(&href)
                        .unwrap_or_default();
                    self.download_urls
                        .lock()
                        .unwrap()
                        .insert(new_path.clone(), new_href.clone());
                    self.file_contents.lock().unwrap().insert(new_href, bytes);
                }
            }
            Ok(())
        }

        fn resolve_download_url(&self, path: &str) -> Result<String, YandexError> {
            self.download_urls
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or(YandexError::NotFound)
        }

        fn resolve_upload_url(&self, path: &str, _overwrite: bool) -> Result<String, YandexError> {
            Ok(format!("upload://{path}"))
        }

        fn upload_file(&self, href: &str, local_path: &std::path::Path) -> Result<(), YandexError> {
            let path = href.strip_prefix("upload://").unwrap_or(href).to_owned();
            if let Some(delay) = self.upload_delays.lock().unwrap().get(&path).copied() {
                thread::sleep(delay);
            }
            if self.upload_failures.lock().unwrap().contains(&path) {
                return Err(YandexError::Forbidden);
            }
            let bytes = fs::read(local_path)?;
            let name = path.rsplit('/').next().unwrap_or(&path).to_owned();
            self.resources.lock().unwrap().insert(
                path.clone(),
                ResourceEntry {
                    path: path.clone(),
                    name,
                    kind: ResourceKind::File,
                    size: bytes.len() as u64,
                    created: Some(SystemTime::UNIX_EPOCH),
                    modified: Some(SystemTime::UNIX_EPOCH),
                },
            );
            let href = format!("download://{path}");
            self.download_urls
                .lock()
                .unwrap()
                .insert(path, href.clone());
            self.file_contents.lock().unwrap().insert(href, bytes);
            Ok(())
        }

        fn download_file(&self, href: &str) -> Result<Vec<u8>, YandexError> {
            self.file_contents
                .lock()
                .unwrap()
                .get(href)
                .cloned()
                .ok_or(YandexError::NotFound)
        }

        fn read_file_range(
            &self,
            href: &str,
            offset: u64,
            size: u32,
        ) -> Result<Vec<u8>, YandexError> {
            if let Some(delay) = self.read_delays.lock().unwrap().get(href).copied() {
                thread::sleep(delay);
            }

            let body = self
                .file_contents
                .lock()
                .unwrap()
                .get(href)
                .cloned()
                .ok_or(YandexError::NotFound)?;
            let start = offset as usize;
            if start >= body.len() {
                return Ok(Vec::new());
            }

            let end = (start + size as usize).min(body.len());
            Ok(body[start..end].to_vec())
        }
    }

    fn dir(path: &str, name: &str) -> ResourceEntry {
        ResourceEntry {
            path: path.to_owned(),
            name: name.to_owned(),
            kind: ResourceKind::Directory,
            size: 0,
            created: Some(SystemTime::UNIX_EPOCH),
            modified: Some(SystemTime::UNIX_EPOCH),
        }
    }

    fn file(path: &str, name: &str, size: u64) -> ResourceEntry {
        ResourceEntry {
            path: path.to_owned(),
            name: name.to_owned(),
            kind: ResourceKind::File,
            size,
            created: Some(SystemTime::UNIX_EPOCH),
            modified: Some(SystemTime::UNIX_EPOCH),
        }
    }

    fn fresh_fs(client: Arc<FakeClient>) -> Arc<YandexDiskFs> {
        Arc::new(YandexDiskFs::with_client(client, 1000, 1000).unwrap())
    }

    #[test]
    fn create_write_and_commit_new_file() {
        let client = FakeClient::with_fixture();
        let fs = fresh_fs(Arc::clone(&client));

        let (entry, fh) = fs.create_pending_file(INodeNo::ROOT, "new.txt").unwrap();
        assert_eq!(entry.size, 0);
        fs.write_handle_data(fh, 0, b"hello world").unwrap();
        fs.commit_write_handle(fh).unwrap();
        fs.finish_write_handle(fh, true).unwrap();

        let looked_up = fs.lookup_entry(INodeNo::ROOT, "new.txt").unwrap();
        assert_eq!(looked_up.size, 11);
        assert_eq!(
            fs.read_data(looked_up.ino, None, 0, 11).unwrap(),
            b"hello world"
        );
        assert_eq!(
            client
                .fetch_resource_metadata("disk:/new.txt")
                .unwrap()
                .size,
            11
        );
    }

    #[test]
    fn overwrite_existing_file_and_truncate() {
        let client = FakeClient::with_fixture();
        let fs = fresh_fs(Arc::clone(&client));

        let fast = fs.lookup_entry(INodeNo::ROOT, "fast.txt").unwrap();
        let fh = fs.open_file_handle(fast.ino, true, true).unwrap();
        fs.write_handle_data(fh, 0, b"abc123").unwrap();
        fs.commit_write_handle(fh).unwrap();
        fs.finish_write_handle(fh, true).unwrap();
        assert_eq!(fs.read_data(fast.ino, None, 0, 6).unwrap(), b"abc123");

        let fh2 = fs.open_file_handle(fast.ino, true, false).unwrap();
        fs.truncate_handle(fh2, 3).unwrap();
        fs.commit_write_handle(fh2).unwrap();
        fs.finish_write_handle(fh2, true).unwrap();
        assert_eq!(fs.read_data(fast.ino, None, 0, 10).unwrap(), b"abc");
    }

    #[test]
    fn delete_and_rename_update_directory_view() {
        let client = FakeClient::with_fixture();
        let fs = fresh_fs(client);

        fs.mkdir_entry(INodeNo::ROOT, "docs").unwrap();
        let (_entry, fh) = fs
            .create_pending_file(INodeNo::ROOT, "rename-me.txt")
            .unwrap();
        fs.write_handle_data(fh, 0, b"payload").unwrap();
        fs.commit_write_handle(fh).unwrap();
        fs.finish_write_handle(fh, true).unwrap();

        fs.rename_entry(INodeNo::ROOT, "rename-me.txt", INodeNo::ROOT, "renamed.txt")
            .unwrap();
        assert!(fs.lookup_entry(INodeNo::ROOT, "rename-me.txt").is_err());
        let renamed = fs.lookup_entry(INodeNo::ROOT, "renamed.txt").unwrap();
        assert_eq!(renamed.path, "disk:/renamed.txt");

        fs.unlink_entry(INodeNo::ROOT, "renamed.txt", false)
            .unwrap();
        assert!(fs.lookup_entry(INodeNo::ROOT, "renamed.txt").is_err());
        fs.unlink_entry(INodeNo::ROOT, "docs", true).unwrap();
        assert!(fs.lookup_entry(INodeNo::ROOT, "docs").is_err());
    }

    #[test]
    fn failed_commit_keeps_remote_contents_authoritative() {
        let client = FakeClient::with_fixture();
        client.fail_upload_for("disk:/fast.txt");
        let fs = fresh_fs(client);

        let fast = fs.lookup_entry(INodeNo::ROOT, "fast.txt").unwrap();
        let fh = fs.open_file_handle(fast.ino, true, true).unwrap();
        fs.write_handle_data(fh, 0, b"broken").unwrap();
        let err = fs.commit_write_handle(fh).unwrap_err();
        assert!(matches!(err, FsError::Remote(YandexError::Forbidden)));
        assert_eq!(fs.read_data(fast.ino, None, 0, 4).unwrap(), b"fast");
    }

    #[test]
    fn slow_upload_does_not_block_unrelated_getattr() {
        let client = FakeClient::with_fixture();
        let fs = fresh_fs(Arc::clone(&client));

        let fast = fs.lookup_entry(INodeNo::ROOT, "fast.txt").unwrap();
        client.set_upload_delay("disk:/slow.txt", Duration::from_millis(250));
        let slow = fs.lookup_entry(INodeNo::ROOT, "slow.txt").unwrap();
        let fh = fs.open_file_handle(slow.ino, true, false).unwrap();
        fs.write_handle_data(fh, 0, b"slow-content-updated")
            .unwrap();

        let fs_for_upload = Arc::clone(&fs);
        let handle = thread::spawn(move || fs_for_upload.commit_write_handle(fh).unwrap());

        thread::sleep(Duration::from_millis(40));
        let start = Instant::now();
        let fast_entry = fs.getattr_entry(fast.ino).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(fast_entry.path, "disk:/fast.txt");
        assert!(elapsed < Duration::from_millis(150));
        handle.join().unwrap();
    }

    #[test]
    fn readonly_open_does_not_create_write_handle() {
        let client = FakeClient::with_fixture();
        let fs = fresh_fs(client);

        let fast = fs.lookup_entry(INodeNo::ROOT, "fast.txt").unwrap();
        let fh = fs.open_file_handle(fast.ino, false, false).unwrap();

        assert_eq!(fh, FileHandle(0));
        assert!(!fs.has_write_handle(fh));
        assert_eq!(fs.read_data(fast.ino, Some(fh), 0, 4).unwrap(), b"fast");
    }

    #[test]
    fn slow_read_does_not_block_unrelated_getattr() {
        let client = FakeClient::with_fixture();
        client.set_read_delay("download://slow.txt", Duration::from_millis(250));
        let fs = fresh_fs(client);

        let slow = fs.lookup_entry(INodeNo::ROOT, "slow.txt").unwrap();
        let fs_for_read = Arc::clone(&fs);
        let handle = thread::spawn(move || fs_for_read.read_data(slow.ino, None, 0, 4).unwrap());

        thread::sleep(Duration::from_millis(40));
        let start = Instant::now();
        let root = fs.getattr_entry(INodeNo::ROOT).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(root.ino, INodeNo::ROOT);
        assert!(elapsed < Duration::from_millis(150));
        assert_eq!(handle.join().unwrap(), b"slow".to_vec());
    }

    #[test]
    fn slow_metadata_refresh_does_not_block_other_paths() {
        let client = FakeClient::with_fixture();
        client.set_metadata_delay("disk:/slow.txt", Duration::from_millis(250));
        let fs = fresh_fs(client);

        let slow = fs.lookup_entry(INodeNo::ROOT, "slow.txt").unwrap();
        let fast = fs.lookup_entry(INodeNo::ROOT, "fast.txt").unwrap();

        {
            let mut state = fs.state.lock().unwrap();
            state.entries.get_mut(&slow.ino).unwrap().cached_at =
                Instant::now() - METADATA_CACHE_TTL - Duration::from_millis(1);
        }

        let fs_for_refresh = Arc::clone(&fs);
        let handle = thread::spawn(move || fs_for_refresh.getattr_entry(slow.ino).unwrap());

        thread::sleep(Duration::from_millis(40));
        let start = Instant::now();
        let fast_entry = fs.getattr_entry(fast.ino).unwrap();
        let elapsed = start.elapsed();

        assert_eq!(fast_entry.path, "disk:/fast.txt");
        assert!(elapsed < Duration::from_millis(150));
        assert_eq!(handle.join().unwrap().path, "disk:/slow.txt");
    }
}
