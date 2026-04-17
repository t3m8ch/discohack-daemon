use std::{
    env,
    ffi::OsStr,
    time::{Duration, SystemTime},
};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, Request,
};

const TTL: Duration = Duration::from_secs(1);

const ROOT_INO: INodeNo = INodeNo::ROOT;
const HELLO_INO: INodeNo = INodeNo(2);

const HELLO_NAME: &str = "hello.txt";
const HELLO_CONTENT: &[u8] = b"Hello from FUSE + Rust + fuser 0.17\n";

struct HelloFs;

fn now() -> SystemTime {
    SystemTime::now()
}

fn errno(code: i32) -> Errno {
    // fuser::Errno newtype
    Errno::from_i32(code)
}

fn root_attr() -> FileAttr {
    FileAttr {
        ino: ROOT_INO,
        size: 0,
        blocks: 0,
        atime: now(),
        mtime: now(),
        ctime: now(),
        crtime: now(),
        kind: FileType::Directory,
        perm: 0o755,
        nlink: 2,
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

fn hello_attr() -> FileAttr {
    FileAttr {
        ino: HELLO_INO,
        size: HELLO_CONTENT.len() as u64,
        blocks: 1,
        atime: now(),
        mtime: now(),
        ctime: now(),
        crtime: now(),
        kind: FileType::RegularFile,
        perm: 0o444,
        nlink: 1,
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        rdev: 0,
        blksize: 512,
        flags: 0,
    }
}

impl Filesystem for HelloFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        if parent == ROOT_INO && name.to_str() == Some(HELLO_NAME) {
            reply.entry(&TTL, &hello_attr(), Generation(0));
        } else {
            reply.error(Errno::ENOENT);
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match ino {
            ROOT_INO => reply.attr(&TTL, &root_attr()),
            HELLO_INO => reply.attr(&TTL, &hello_attr()),
            _ => reply.error(errno(libc::ENOENT)),
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
        if ino != ROOT_INO {
            reply.error(errno(libc::ENOENT));
            return;
        }

        let entries = [
            (ROOT_INO, FileType::Directory, "."),
            (ROOT_INO, FileType::Directory, ".."),
            (HELLO_INO, FileType::RegularFile, HELLO_NAME),
        ];

        for (i, (entry_ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            let next_offset = (i + 1) as u64;
            let full = reply.add(*entry_ino, next_offset, *kind, name);
            if full {
                break;
            }
        }

        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match ino {
            HELLO_INO => reply.opened(FileHandle(0), FopenFlags::empty()),
            ROOT_INO => reply.error(Errno::EISDIR),
            _ => reply.error(Errno::ENOENT),
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
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        if ino != HELLO_INO {
            reply.error(errno(libc::ENOENT));
            return;
        }

        let start = offset as usize;
        if start >= HELLO_CONTENT.len() {
            reply.data(&[]);
            return;
        }

        let end = (start + size as usize).min(HELLO_CONTENT.len());
        reply.data(&HELLO_CONTENT[start..end]);
    }
}

fn main() {
    let mountpoint = env::args().nth(1).expect("usage: mini-fuse <mountpoint>");

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("hello-fs".into()),
        //MountOption::AutoUnmount,
    ];

    fuser::mount2(HelloFs, mountpoint, &config).unwrap();
}
