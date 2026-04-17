use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsStr,
    sync::Mutex,
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

pub struct YandexDiskFs {
    state: Mutex<FsState>,
    uid: u32,
    gid: u32,
}

struct FsState {
    client: YandexDiskClient,
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

impl YandexDiskFs {
    pub fn new(client: YandexDiskClient, uid: u32, gid: u32) -> Result<Self, YandexError> {
        let state = FsState::new(client)?;
        Ok(Self {
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

        let mut state = self.state.lock().unwrap();
        match state.lookup(parent, name) {
            Ok(entry) => reply.entry(&TTL, &self.attr_for(&entry), Generation(0)),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let mut state = self.state.lock().unwrap();
        match state.getattr(ino) {
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
        let mut state = self.state.lock().unwrap();
        match state.getattr(ino) {
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
        let mut state = self.state.lock().unwrap();
        match state.readdir_entries(ino) {
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

        let mut state = self.state.lock().unwrap();
        match state.getattr(ino) {
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
        let mut state = self.state.lock().unwrap();
        match state.read(ino, offset, size) {
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
    fn new(client: YandexDiskClient) -> Result<Self, YandexError> {
        let root = client.fetch_resource_metadata(ROOT_PATH)?;
        let mut state = Self {
            client,
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
        Ok(state)
    }

    fn lookup(&mut self, parent: INodeNo, name: &str) -> Result<Entry, FsError> {
        self.ensure_directory_loaded(parent)?;
        let child_ino = self
            .dir_children
            .get(&parent)
            .and_then(|children| children.get(name).copied())
            .ok_or(FsError::NotFound)?;
        self.getattr(child_ino)
    }

    fn getattr(&mut self, ino: INodeNo) -> Result<Entry, FsError> {
        self.ensure_entry_fresh(ino)?;
        self.entries.get(&ino).cloned().ok_or(FsError::NotFound)
    }

    fn readdir_entries(
        &mut self,
        ino: INodeNo,
    ) -> Result<Vec<(INodeNo, FileType, String)>, FsError> {
        self.ensure_directory_loaded(ino)?;

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

    fn read(&mut self, ino: INodeNo, offset: u64, size: u32) -> Result<Vec<u8>, FsError> {
        let entry = self.getattr(ino)?;
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
                if let Some(node) = self.entries.get_mut(&ino) {
                    node.download_url = None;
                    node.download_url_cached_at = None;
                }
                let refreshed = self.download_url_for(ino)?;
                Ok(self.client.read_file_range(&refreshed, offset, size)?)
            }
            Err(err) => Err(err.into()),
        }
    }

    fn ensure_entry_fresh(&mut self, ino: INodeNo) -> Result<(), FsError> {
        let should_refresh = self
            .entries
            .get(&ino)
            .map(|entry| entry.cached_at.elapsed() >= METADATA_CACHE_TTL)
            .ok_or(FsError::NotFound)?;

        if !should_refresh {
            return Ok(());
        }

        let (path, parent) = {
            let entry = self.entries.get(&ino).ok_or(FsError::NotFound)?;
            (entry.path.clone(), entry.parent)
        };

        let fresh = self.client.fetch_resource_metadata(&path)?;
        self.upsert_entry(parent, fresh);
        Ok(())
    }

    fn ensure_directory_loaded(&mut self, ino: INodeNo) -> Result<(), FsError> {
        self.ensure_entry_fresh(ino)?;

        let is_fresh = self
            .dir_cache_time
            .get(&ino)
            .map(|cached| cached.elapsed() < METADATA_CACHE_TTL)
            .unwrap_or(false);
        if is_fresh {
            let entry = self.entries.get(&ino).ok_or(FsError::NotFound)?;
            if entry.kind != EntryKind::Directory {
                return Err(FsError::NotDir);
            }
            return Ok(());
        }

        let entry = self.entries.get(&ino).cloned().ok_or(FsError::NotFound)?;
        if entry.kind != EntryKind::Directory {
            return Err(FsError::NotDir);
        }

        let children = self.client.list_directory(&entry.path)?;
        let mut mapped = BTreeMap::new();
        for child in children {
            let name = child.name.clone();
            let child_ino = self.upsert_entry(ino, child);
            mapped.insert(name, child_ino);
        }

        self.dir_children.insert(ino, mapped);
        self.dir_cache_time.insert(ino, Instant::now());
        Ok(())
    }

    fn download_url_for(&mut self, ino: INodeNo) -> Result<String, FsError> {
        {
            let entry = self.entries.get(&ino).ok_or(FsError::NotFound)?;
            if entry.kind != EntryKind::File {
                return Err(FsError::IsDir);
            }
            if let (Some(url), Some(cached_at)) =
                (&entry.download_url, entry.download_url_cached_at)
            {
                if cached_at.elapsed() < DOWNLOAD_URL_TTL {
                    return Ok(url.clone());
                }
            }
        }

        let path = self
            .entries
            .get(&ino)
            .map(|entry| entry.path.clone())
            .ok_or(FsError::NotFound)?;
        let url = self.client.resolve_download_url(&path)?;
        if let Some(entry) = self.entries.get_mut(&ino) {
            entry.download_url = Some(url.clone());
            entry.download_url_cached_at = Some(Instant::now());
        }
        Ok(url)
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
        FsError::Remote(YandexError::Unauthorized) | FsError::Remote(YandexError::Forbidden) => {
            Errno::EACCES
        }
        FsError::Remote(YandexError::InvalidResponse(_))
        | FsError::Remote(YandexError::Http(_))
        | FsError::Remote(YandexError::Status { .. })
        | FsError::Remote(YandexError::Header(_)) => Errno::EIO,
    }
}
