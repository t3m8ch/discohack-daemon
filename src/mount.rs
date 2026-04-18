use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
};

use fuser::{BackgroundSession, Config, MountOption};
use thiserror::Error;
use tracing::{error, info, warn};

use crate::{fs::YandexDiskFs, sync::SyncService};

const MIN_FUSE_WORKER_THREADS: usize = 2;
const MAX_FUSE_WORKER_THREADS: usize = 8;

pub struct MountManager {
    mountpoint: PathBuf,
    sync: Arc<SyncService>,
    uid: u32,
    gid: u32,
    session: Mutex<Option<BackgroundSession>>,
}

#[derive(Debug, Error)]
pub enum MountError {
    #[error("failed to initialize filesystem: {0}")]
    Filesystem(#[from] crate::sync::SyncError),
    #[error("failed to mount filesystem: {0}")]
    Mount(#[source] io::Error),
    #[error("failed to stop filesystem: {0}")]
    Shutdown(#[source] io::Error),
}

impl MountManager {
    pub fn new(mountpoint: PathBuf, sync: Arc<SyncService>, uid: u32, gid: u32) -> Self {
        Self {
            mountpoint,
            sync,
            uid,
            gid,
            session: Mutex::new(None),
        }
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    pub fn is_mounted(&self) -> bool {
        self.session.lock().unwrap().is_some()
    }

    pub fn ensure_mounted(&self) -> Result<(), MountError> {
        let mut session_guard = self.session.lock().unwrap();
        if let Some(existing) = session_guard.as_ref() {
            if !existing.guard.is_finished() {
                return Ok(());
            }
        }

        if let Some(finished) = session_guard.take() {
            match finished.join() {
                Ok(()) => {
                    info!(mountpoint = %self.mountpoint.display(), "previous mount session ended")
                }
                Err(err) => {
                    warn!(mountpoint = %self.mountpoint.display(), error = %err, "previous mount session ended with an error")
                }
            }
        }

        let fs = YandexDiskFs::new(self.sync.clone(), self.uid, self.gid)?;
        let mut config = Config::default();
        config.mount_options = vec![MountOption::FSName("yandex-disk".into())];
        let worker_threads = configure_fuse_session(&mut config);

        info!(
            mountpoint = %self.mountpoint.display(),
            worker_threads,
            clone_fd = config.clone_fd,
            "mounting filesystem"
        );

        let session =
            fuser::spawn_mount2(fs, &self.mountpoint, &config).map_err(MountError::Mount)?;
        *session_guard = Some(session);
        info!(mountpoint = %self.mountpoint.display(), "filesystem mounted");
        Ok(())
    }

    pub fn shutdown(&self) -> Result<(), MountError> {
        let mut session_guard = self.session.lock().unwrap();
        let Some(session) = session_guard.take() else {
            return Ok(());
        };
        drop(session_guard);

        info!(mountpoint = %self.mountpoint.display(), "starting graceful shutdown");
        match session.umount_and_join() {
            Ok(()) => {
                info!(mountpoint = %self.mountpoint.display(), "graceful shutdown complete");
                Ok(())
            }
            Err(err) => {
                error!(mountpoint = %self.mountpoint.display(), error = %err, "graceful shutdown failed");
                Err(MountError::Shutdown(err))
            }
        }
    }
}

fn configure_fuse_session(config: &mut Config) -> usize {
    let worker_threads = thread::available_parallelism()
        .map(|parallelism| {
            parallelism
                .get()
                .clamp(MIN_FUSE_WORKER_THREADS, MAX_FUSE_WORKER_THREADS)
        })
        .unwrap_or(MIN_FUSE_WORKER_THREADS);

    #[cfg(target_os = "linux")]
    {
        config.n_threads = Some(worker_threads);
        config.clone_fd = worker_threads > 1;
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = worker_threads;
    }

    config.n_threads.unwrap_or(1)
}
