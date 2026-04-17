use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsStr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use fuser::{
    AccessFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, OpenAccMode, OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};

use crate::yadisk::{ResourceEntry, ResourceKind, YandexDiskClient, YandexError};

const TTL: Duration = Duration::from_secs(2);
const METADATA_CACHE_TTL: Duration = Duration::from_secs(5);
const DOWNLOAD_URL_TTL: Duration = Duration::from_secs(300);
const ROOT_PATH: &str = "disk:/";

trait RemoteClient: Send + Sync {
    fn fetch_resource_metadata(&self, path: &str) -> Result<ResourceEntry, YandexError>;
    fn list_directory(&self, path: &str) -> Result<Vec<ResourceEntry>, YandexError>;
    fn resolve_download_url(&self, path: &str) -> Result<String, YandexError>;
    fn read_file_range(&self, href: &str, offset: u64, size: u32) -> Result<Vec<u8>, YandexError>;
}

impl RemoteClient for YandexDiskClient {
    fn fetch_resource_metadata(&self, path: &str) -> Result<ResourceEntry, YandexError> {
        YandexDiskClient::fetch_resource_metadata(self, path)
    }

    fn list_directory(&self, path: &str) -> Result<Vec<ResourceEntry>, YandexError> {
        YandexDiskClient::list_directory(self, path)
    }

    fn resolve_download_url(&self, path: &str) -> Result<String, YandexError> {
        YandexDiskClient::resolve_download_url(self, path)
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
    entries: HashMap<INodeNo, Entry>,
    path_to_ino: HashMap<String, INodeNo>,
    dir_children: HashMap<INodeNo, BTreeMap<String, INodeNo>>,
    dir_cache_time: HashMap<INodeNo, Instant>,
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
    Remote(YandexError),
}

impl From<YandexError> for FsError {
    fn from(value: YandexError) -> Self {
        Self::Remote(value)
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
            perm: if is_dir { 0o555 } else { 0o444 },
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

    fn read_data(&self, ino: INodeNo, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
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
}

impl Filesystem for YandexDiskFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
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

    fn access(&self, _req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        if mask.contains(AccessFlags::W_OK) {
            reply.error(Errno::EROFS);
            return;
        }

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
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }

        match self.getattr_entry(ino) {
            Ok(entry) if entry.kind == EntryKind::File => {
                reply.opened(FileHandle(0), FopenFlags::empty())
            }
            Ok(_) => reply.error(Errno::EISDIR),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.read_data(ino, offset, size) {
            Ok(data) => reply.data(&data),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        _size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mknod(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn mkdir(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        reply.error(Errno::EROFS);
    }

    fn unlink(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rmdir(&self, _req: &Request, _parent: INodeNo, _name: &OsStr, reply: ReplyEmpty) {
        reply.error(Errno::EROFS);
    }

    fn rename(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _newparent: INodeNo,
        _newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        reply.error(Errno::EROFS);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _offset: u64,
        _data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        reply.error(Errno::EROFS);
    }

    fn create(
        &self,
        _req: &Request,
        _parent: INodeNo,
        _name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        reply.error(Errno::EROFS);
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
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
            entries: HashMap::new(),
            path_to_ino: HashMap::new(),
            dir_children: HashMap::new(),
            dir_cache_time: HashMap::new(),
        };

        let root_entry = Entry::from_resource(INodeNo::ROOT, INodeNo::ROOT, root);
        state.entries.insert(INodeNo::ROOT, root_entry);
        state
            .path_to_ino
            .insert(ROOT_PATH.to_owned(), INodeNo::ROOT);
        state
    }

    fn entry_refresh_plan(&self, ino: INodeNo) -> Result<EntryRefreshPlan, FsError> {
        let entry = self.entries.get(&ino).cloned().ok_or(FsError::NotFound)?;
        if entry.cached_at.elapsed() < METADATA_CACHE_TTL {
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

        let ino = INodeNo(self.next_ino);
        self.next_ino += 1;

        let path = resource.path.clone();
        let entry = Entry::from_resource(ino, parent, resource);
        self.path_to_ino.insert(path, ino);
        self.entries.insert(ino, entry);
        ino
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
        if self.kind == EntryKind::Directory {
            self.download_url = None;
            self.download_url_cached_at = None;
        }
    }
}

fn errno_for(error: &FsError) -> Errno {
    match error {
        FsError::NotFound => Errno::ENOENT,
        FsError::NotDir => Errno::ENOTDIR,
        FsError::IsDir => Errno::EISDIR,
        FsError::Remote(YandexError::NotFound) => Errno::ENOENT,
        FsError::Remote(YandexError::Unauthorized)
        | FsError::Remote(YandexError::Forbidden)
        | FsError::Remote(YandexError::Auth(_)) => Errno::EACCES,
        FsError::Remote(YandexError::InvalidResponse(_))
        | FsError::Remote(YandexError::Http(_))
        | FsError::Remote(YandexError::Status { .. }) => Errno::EIO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::Mutex,
        thread,
        time::{Duration, Instant, SystemTime},
    };

    #[derive(Default)]
    struct FakeClient {
        resources: Mutex<HashMap<String, ResourceEntry>>,
        directories: Mutex<HashMap<String, Vec<ResourceEntry>>>,
        download_urls: Mutex<HashMap<String, String>>,
        file_contents: Mutex<HashMap<String, Vec<u8>>>,
        metadata_delays: Mutex<HashMap<String, Duration>>,
        read_delays: Mutex<HashMap<String, Duration>>,
    }

    impl FakeClient {
        fn with_fixture() -> Arc<Self> {
            let client = Arc::new(Self::default());
            client.insert_resource(dir("disk:/", "disk:/"));
            client.insert_resource(file("disk:/slow.txt", "slow.txt", 11));
            client.insert_resource(file("disk:/fast.txt", "fast.txt", 11));
            client.set_directory(
                "disk:/",
                vec![
                    file("disk:/fast.txt", "fast.txt", 11),
                    file("disk:/slow.txt", "slow.txt", 11),
                ],
            );
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

        fn set_directory(&self, path: &str, entries: Vec<ResourceEntry>) {
            self.directories
                .lock()
                .unwrap()
                .insert(path.to_owned(), entries);
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
            self.directories
                .lock()
                .unwrap()
                .get(path)
                .cloned()
                .ok_or(YandexError::NotFound)
        }

        fn resolve_download_url(&self, path: &str) -> Result<String, YandexError> {
            self.download_urls
                .lock()
                .unwrap()
                .get(path)
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
    fn slow_read_does_not_block_unrelated_getattr() {
        let client = FakeClient::with_fixture();
        client.set_read_delay("download://slow.txt", Duration::from_millis(250));
        let fs = fresh_fs(client);

        let slow = fs.lookup_entry(INodeNo::ROOT, "slow.txt").unwrap();
        let fs_for_read = Arc::clone(&fs);
        let handle = thread::spawn(move || fs_for_read.read_data(slow.ino, 0, 4).unwrap());

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

    #[test]
    fn concurrent_operations_preserve_inode_and_error_mapping_semantics() {
        let client = FakeClient::with_fixture();
        client.set_read_delay("download://slow.txt", Duration::from_millis(250));
        let fs = fresh_fs(client);

        let first_fast = fs.lookup_entry(INodeNo::ROOT, "fast.txt").unwrap();
        let slow = fs.lookup_entry(INodeNo::ROOT, "slow.txt").unwrap();

        let fs_for_read = Arc::clone(&fs);
        let handle = thread::spawn(move || fs_for_read.read_data(slow.ino, 0, 4).unwrap());

        thread::sleep(Duration::from_millis(40));
        let second_fast = fs.lookup_entry(INodeNo::ROOT, "fast.txt").unwrap();

        assert_eq!(first_fast.ino, second_fast.ino);
        assert!(matches!(
            fs.read_data(INodeNo::ROOT, 0, 4),
            Err(FsError::IsDir)
        ));
        assert_eq!(
            format!("{:?}", errno_for(&FsError::IsDir)),
            format!("{:?}", Errno::EISDIR)
        );
        assert_eq!(
            format!(
                "{:?}",
                errno_for(&FsError::Remote(YandexError::Unauthorized))
            ),
            format!("{:?}", Errno::EACCES)
        );
        assert_eq!(handle.join().unwrap(), b"slow".to_vec());
    }
}
