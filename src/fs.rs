use std::{
    collections::HashMap,
    ffi::OsStr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};

use fuser::{
    AccessFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, OpenAccMode, OpenFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};

use crate::sync::{FileKind, LocalNode, SyncError, SyncService};

const TTL: Duration = Duration::from_secs(2);

pub struct YandexDiskFs {
    sync: Arc<SyncService>,
    state: Mutex<FsState>,
    uid: u32,
    gid: u32,
}

struct FsState {
    next_ino: u64,
    next_fh: u64,
    entries: HashMap<INodeNo, EntryRef>,
    path_to_ino: HashMap<String, INodeNo>,
    handles: HashMap<FileHandle, String>,
}

#[derive(Clone)]
struct EntryRef {
    path: String,
    parent: INodeNo,
    kind: FileKind,
}

impl YandexDiskFs {
    pub fn new(sync: Arc<SyncService>, uid: u32, gid: u32) -> Result<Self, SyncError> {
        let root = sync.root_node()?;
        Ok(Self {
            sync,
            state: Mutex::new(FsState::new(root)),
            uid,
            gid,
        })
    }

    fn attr_for(&self, node: &LocalNode, ino: INodeNo) -> FileAttr {
        let is_dir = node.kind == FileKind::Directory;
        let size = if is_dir { 0 } else { node.size };
        FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: node.mtime,
            mtime: node.mtime,
            ctime: node.mtime,
            crtime: node.mtime,
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

    fn entry_ref(&self, ino: INodeNo) -> Result<EntryRef, SyncError> {
        let state = self.state.lock().unwrap();
        state.entries.get(&ino).cloned().ok_or(SyncError::NotFound)
    }

    fn intern_node(&self, node: &LocalNode, parent_ino: Option<INodeNo>) -> INodeNo {
        let mut state = self.state.lock().unwrap();
        state.intern(node, parent_ino)
    }

    fn refresh_entry(&self, ino: INodeNo) -> Result<(LocalNode, INodeNo), SyncError> {
        let path = self.entry_ref(ino)?.path;
        let node = self.sync.get_entry(&path)?;
        let parent_ino = self.resolve_parent_ino(&node)?;
        let resolved_ino = self.intern_node(&node, Some(parent_ino));
        Ok((node, resolved_ino))
    }

    fn resolve_parent_ino(&self, node: &LocalNode) -> Result<INodeNo, SyncError> {
        if node.path == crate::sync::ROOT_PATH {
            return Ok(INodeNo::ROOT);
        }
        let parent_path = node.parent_path.clone().ok_or_else(|| {
            SyncError::InvalidState(format!("node {} is missing parent path", node.path))
        })?;

        {
            let state = self.state.lock().unwrap();
            if let Some(ino) = state.path_to_ino.get(&parent_path).copied() {
                return Ok(ino);
            }
        }

        let parent = self.sync.get_entry(&parent_path)?;
        Ok(self.intern_node(&parent, Some(INodeNo::ROOT)))
    }

    fn lookup_entry(&self, parent: INodeNo, name: &str) -> Result<(LocalNode, INodeNo), SyncError> {
        let parent_ref = self.entry_ref(parent)?;
        let node = self.sync.lookup_child(&parent_ref.path, name)?;
        let ino = self.intern_node(&node, Some(parent));
        Ok((node, ino))
    }

    fn readdir_listing(&self, ino: INodeNo) -> Result<Vec<(INodeNo, FileType, String)>, SyncError> {
        let entry = self.entry_ref(ino)?;
        let node = self.sync.get_entry(&entry.path)?;
        if node.kind != FileKind::Directory {
            return Err(SyncError::NotDir);
        }

        let children = self.sync.list_directory(&node.path)?;
        let mut out = vec![
            (ino, FileType::Directory, String::from(".")),
            (
                if ino == INodeNo::ROOT {
                    INodeNo::ROOT
                } else {
                    entry.parent
                },
                FileType::Directory,
                String::from(".."),
            ),
        ];
        for child in children {
            let child_ino = self.intern_node(&child, Some(ino));
            out.push((
                child_ino,
                if child.kind == FileKind::Directory {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                },
                child.name,
            ));
        }
        Ok(out)
    }

    fn open_file_handle(
        &self,
        ino: INodeNo,
        writable: bool,
        truncate: bool,
    ) -> Result<FileHandle, SyncError> {
        let (node, _) = self.refresh_entry(ino)?;
        if node.kind != FileKind::File {
            return Err(SyncError::IsDir);
        }
        if !writable {
            return Ok(FileHandle(0));
        }

        let prepared = self.sync.prepare_write(&node.file_id, truncate)?;
        let mut state = self.state.lock().unwrap();
        let fh = FileHandle(state.next_fh);
        state.next_fh += 1;
        state.handles.insert(fh, prepared.file_id);
        Ok(fh)
    }

    fn create_pending_file(
        &self,
        parent: INodeNo,
        name: &str,
    ) -> Result<(LocalNode, INodeNo, FileHandle), SyncError> {
        let parent_ref = self.entry_ref(parent)?;
        let node = self.sync.create_file(&parent_ref.path, name)?;
        let ino = self.intern_node(&node, Some(parent));
        let mut state = self.state.lock().unwrap();
        let fh = FileHandle(state.next_fh);
        state.next_fh += 1;
        state.handles.insert(fh, node.file_id.clone());
        Ok((node, ino, fh))
    }

    fn handle_file_id(&self, fh: FileHandle) -> Result<String, SyncError> {
        let state = self.state.lock().unwrap();
        state
            .handles
            .get(&fh)
            .cloned()
            .ok_or_else(|| SyncError::InvalidState(format!("missing file handle {}", fh.0)))
    }

    fn write_handle_data(
        &self,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
    ) -> Result<u32, SyncError> {
        let file_id = self.handle_file_id(fh)?;
        self.sync.write_file(&file_id, offset, data)
    }

    fn truncate_entry(
        &self,
        ino: INodeNo,
        fh: Option<FileHandle>,
        size: u64,
    ) -> Result<(LocalNode, INodeNo), SyncError> {
        let file_id = if let Some(fh) = fh {
            self.handle_file_id(fh)?
        } else {
            self.refresh_entry(ino)?.0.file_id
        };
        let node = self.sync.truncate_file(&file_id, size)?;
        let parent_ino = self.resolve_parent_ino(&node)?;
        let resolved = self.intern_node(&node, Some(parent_ino));
        Ok((node, resolved))
    }

    fn read_data(
        &self,
        ino: INodeNo,
        fh: Option<FileHandle>,
        offset: u64,
        size: u32,
    ) -> Result<Vec<u8>, SyncError> {
        let file_id = if let Some(fh) = fh {
            if fh != FileHandle(0) {
                self.handle_file_id(fh)?
            } else {
                self.refresh_entry(ino)?.0.file_id
            }
        } else {
            self.refresh_entry(ino)?.0.file_id
        };
        self.sync.read_file(&file_id, offset, size)
    }

    fn mkdir_entry(&self, parent: INodeNo, name: &str) -> Result<(LocalNode, INodeNo), SyncError> {
        let parent_ref = self.entry_ref(parent)?;
        let node = self.sync.mkdir(&parent_ref.path, name)?;
        let ino = self.intern_node(&node, Some(parent));
        Ok((node, ino))
    }

    fn unlink_entry(&self, parent: INodeNo, name: &str, expect_dir: bool) -> Result<(), SyncError> {
        let parent_ref = self.entry_ref(parent)?;
        self.sync.delete(
            &format!("{}/{}", parent_ref.path.trim_end_matches('/'), name),
            expect_dir,
        )
    }

    fn rename_entry(
        &self,
        parent: INodeNo,
        name: &str,
        newparent: INodeNo,
        newname: &str,
    ) -> Result<(), SyncError> {
        let parent_ref = self.entry_ref(parent)?;
        let new_parent_ref = self.entry_ref(newparent)?;
        let old_path = if parent_ref.path == crate::sync::ROOT_PATH {
            format!("{}{}", parent_ref.path, name)
        } else {
            format!("{}/{}", parent_ref.path, name)
        };
        let new_path = if new_parent_ref.path == crate::sync::ROOT_PATH {
            format!("{}{}", new_parent_ref.path, newname)
        } else {
            format!("{}/{}", new_parent_ref.path, newname)
        };
        self.sync.rename(&old_path, &new_path)?;
        let mut state = self.state.lock().unwrap();
        state.rename_cached_paths(&old_path, &new_path, newparent);
        Ok(())
    }

    fn release_handle(&self, fh: FileHandle) {
        let mut state = self.state.lock().unwrap();
        state.handles.remove(&fh);
    }
}

impl Filesystem for YandexDiskFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.lookup_entry(parent, name) {
            Ok((node, ino)) => reply.entry(&TTL, &self.attr_for(&node, ino), Generation(0)),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.refresh_entry(ino) {
            Ok((node, resolved_ino)) => reply.attr(&TTL, &self.attr_for(&node, resolved_ino)),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn access(&self, _req: &Request, ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        if self.entry_ref(ino).is_ok() {
            reply.ok();
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.refresh_entry(ino) {
            Ok((node, _)) if node.kind == FileKind::Directory => {
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
                    if reply.add(entry_ino, (index + 1) as u64, kind, name) {
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
            None => self.refresh_entry(ino),
        };
        match result {
            Ok((node, resolved_ino)) => reply.attr(&TTL, &self.attr_for(&node, resolved_ino)),
            Err(err) => reply.error(errno_for(&err)),
        }
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
        reply.error(Errno::EOPNOTSUPP);
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.mkdir_entry(parent, name) {
            Ok((node, ino)) => reply.entry(&TTL, &self.attr_for(&node, ino), Generation(0)),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.unlink_entry(parent, name, false) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(name) = name.to_str() else {
            reply.error(Errno::EINVAL);
            return;
        };
        match self.unlink_entry(parent, name, true) {
            Ok(()) => reply.ok(),
            Err(err) => reply.error(errno_for(&err)),
        }
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
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
        name: &OsStr,
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
            Ok((node, ino, fh)) => reply.created(
                &TTL,
                &self.attr_for(&node, ino),
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
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
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
        if fh != FileHandle(0) {
            self.release_handle(fh);
        }
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
    fn new(root: LocalNode) -> Self {
        let mut entries = HashMap::new();
        let mut path_to_ino = HashMap::new();
        entries.insert(
            INodeNo::ROOT,
            EntryRef {
                path: root.path.clone(),
                parent: INodeNo::ROOT,
                kind: root.kind,
            },
        );
        path_to_ino.insert(root.path, INodeNo::ROOT);
        Self {
            next_ino: 2,
            next_fh: 1,
            entries,
            path_to_ino,
            handles: HashMap::new(),
        }
    }

    fn intern(&mut self, node: &LocalNode, parent_ino: Option<INodeNo>) -> INodeNo {
        if let Some(existing) = self.path_to_ino.get(&node.path).copied() {
            if let Some(entry) = self.entries.get_mut(&existing) {
                entry.parent = parent_ino.unwrap_or(entry.parent);
                entry.kind = node.kind;
            }
            return existing;
        }

        let ino = INodeNo(self.next_ino);
        self.next_ino += 1;
        self.entries.insert(
            ino,
            EntryRef {
                path: node.path.clone(),
                parent: parent_ino.unwrap_or(INodeNo::ROOT),
                kind: node.kind,
            },
        );
        self.path_to_ino.insert(node.path.clone(), ino);
        ino
    }

    fn rename_cached_paths(&mut self, old_path: &str, new_path: &str, new_parent: INodeNo) {
        let affected: Vec<(INodeNo, String)> = self
            .entries
            .iter()
            .filter_map(|(ino, entry)| {
                if entry.path == old_path || entry.path.starts_with(&format!("{old_path}/")) {
                    Some((*ino, entry.path.clone()))
                } else {
                    None
                }
            })
            .collect();

        for (ino, old) in affected {
            if let Some(entry) = self.entries.get_mut(&ino) {
                self.path_to_ino.remove(&old);
                let suffix = old.strip_prefix(old_path).unwrap_or("");
                entry.path = format!("{new_path}{suffix}");
                if old == old_path {
                    entry.parent = new_parent;
                }
                self.path_to_ino.insert(entry.path.clone(), ino);
            }
        }
    }
}

fn errno_for(error: &SyncError) -> Errno {
    match error {
        SyncError::NotFound => Errno::ENOENT,
        SyncError::AlreadyExists | SyncError::Conflict(_) => Errno::EEXIST,
        SyncError::NotDir => Errno::ENOTDIR,
        SyncError::IsDir => Errno::EISDIR,
        SyncError::DirectoryNotEmpty => Errno::ENOTEMPTY,
        SyncError::Io(_) => Errno::EIO,
        SyncError::Db(_) | SyncError::InvalidEnum { .. } | SyncError::InvalidState(_) => Errno::EIO,
        SyncError::Remote(crate::yadisk::YandexError::NotFound) => Errno::ENOENT,
        SyncError::Remote(crate::yadisk::YandexError::Unauthorized)
        | SyncError::Remote(crate::yadisk::YandexError::Forbidden)
        | SyncError::Remote(crate::yadisk::YandexError::Auth(_)) => Errno::EACCES,
        SyncError::Remote(crate::yadisk::YandexError::Conflict(_)) => Errno::EEXIST,
        SyncError::Remote(_) => Errno::EIO,
    }
}
