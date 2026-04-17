mod fs;
mod yadisk;

use std::{
    env, io,
    path::{Path, PathBuf},
    process,
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::Duration,
};

use dotenvy::dotenv;
use fs::YandexDiskFs;
use fuser::{BackgroundSession, Config, MountOption};
use signal_hook::{
    consts::signal::{SIGINT, SIGTERM},
    iterator::Signals,
};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use yadisk::YandexDiskClient;

const SESSION_POLL_INTERVAL: Duration = Duration::from_millis(250);

fn usage() -> &'static str {
    "usage: discohack-daemon <mountpoint>"
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .finish();

    let _ = tracing::subscriber::set_global_default(subscriber);
}

fn load_token() -> Result<String, String> {
    for key in ["YANDEX_DISK_TOKEN", "TOKEN", "YANDEX_TOKEN"] {
        if let Ok(value) = env::var(key) {
            let token = value.trim();
            if !token.is_empty() {
                return Ok(token.to_owned());
            }
        }
    }

    Err(
        "missing Yandex Disk OAuth token; set YANDEX_DISK_TOKEN in the environment or .env"
            .to_owned(),
    )
}

fn mountpoint_from_args() -> Result<PathBuf, String> {
    env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or_else(|| usage().to_owned())
}

enum ShutdownTrigger {
    Signal(&'static str),
    SessionEnded,
}

fn install_signal_handlers() -> io::Result<Receiver<&'static str>> {
    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    let (tx, rx) = mpsc::channel();

    thread::Builder::new()
        .name("signal-listener".to_owned())
        .spawn(move || {
            for signal in signals.forever() {
                let name = match signal {
                    SIGINT => "SIGINT",
                    SIGTERM => "SIGTERM",
                    _ => "UNKNOWN",
                };

                if tx.send(name).is_err() {
                    break;
                }
            }
        })?;

    Ok(rx)
}

fn finalize_session(
    session: &mut Option<BackgroundSession>,
    mountpoint: &Path,
    trigger: ShutdownTrigger,
) -> i32 {
    let Some(session) = session.take() else {
        warn!(mountpoint = %mountpoint.display(), "shutdown already completed");
        return 0;
    };

    match trigger {
        ShutdownTrigger::Signal(signal) => {
            info!(
                signal,
                mountpoint = %mountpoint.display(),
                "starting graceful shutdown"
            );

            match session.umount_and_join() {
                Ok(()) => {
                    info!(mountpoint = %mountpoint.display(), "graceful shutdown complete");
                    0
                }
                Err(err) => {
                    error!(
                        signal,
                        mountpoint = %mountpoint.display(),
                        error = %err,
                        "graceful shutdown failed"
                    );
                    1
                }
            }
        }
        ShutdownTrigger::SessionEnded => match session.join() {
            Ok(()) => {
                info!(mountpoint = %mountpoint.display(), "filesystem session ended");
                0
            }
            Err(err) => {
                error!(
                    mountpoint = %mountpoint.display(),
                    error = %err,
                    "filesystem session ended with an error"
                );
                1
            }
        },
    }
}

fn wait_for_shutdown(
    session: BackgroundSession,
    mountpoint: &Path,
    shutdown_rx: Receiver<&'static str>,
) -> i32 {
    let mut session = Some(session);
    let mut signal_listener_alive = true;

    loop {
        if session
            .as_ref()
            .is_some_and(|session| session.guard.is_finished())
        {
            return finalize_session(&mut session, mountpoint, ShutdownTrigger::SessionEnded);
        }

        if signal_listener_alive {
            match shutdown_rx.recv_timeout(SESSION_POLL_INTERVAL) {
                Ok(signal) => {
                    return finalize_session(
                        &mut session,
                        mountpoint,
                        ShutdownTrigger::Signal(signal),
                    );
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    signal_listener_alive = false;
                    warn!("signal listener stopped unexpectedly; waiting for session to end");
                }
            }
        } else {
            thread::sleep(SESSION_POLL_INTERVAL);
        }
    }
}

fn run() -> i32 {
    let mountpoint = match mountpoint_from_args() {
        Ok(path) => path,
        Err(message) => {
            error!(usage = usage(), error = %message, "invalid command line arguments");
            return 2;
        }
    };

    info!(mountpoint = %mountpoint.display(), "starting discohack-daemon");

    let token = match load_token() {
        Ok(token) => token,
        Err(message) => {
            error!(mountpoint = %mountpoint.display(), error = %message, "missing configuration");
            return 2;
        }
    };

    let client = match YandexDiskClient::new(token) {
        Ok(client) => client,
        Err(err) => {
            error!(
                mountpoint = %mountpoint.display(),
                error = %err,
                "failed to initialize Yandex Disk client"
            );
            return 1;
        }
    };

    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };

    let fs = match YandexDiskFs::new(client, uid, gid) {
        Ok(fs) => fs,
        Err(err) => {
            error!(
                mountpoint = %mountpoint.display(),
                error = %err,
                "failed to initialize Yandex Disk filesystem"
            );
            return 1;
        }
    };

    let shutdown_rx = match install_signal_handlers() {
        Ok(rx) => rx,
        Err(err) => {
            error!(
                mountpoint = %mountpoint.display(),
                error = %err,
                "failed to install signal handlers"
            );
            return 1;
        }
    };

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("yandex-disk-ro".into()),
    ];

    info!(mountpoint = %mountpoint.display(), "mounting filesystem");
    let session = match fuser::spawn_mount2(fs, &mountpoint, &config) {
        Ok(session) => session,
        Err(err) => {
            error!(
                mountpoint = %mountpoint.display(),
                error = %err,
                "failed to mount filesystem"
            );
            return 1;
        }
    };

    info!(mountpoint = %mountpoint.display(), "filesystem mounted");
    wait_for_shutdown(session, &mountpoint, shutdown_rx)
}

fn main() {
    dotenv().ok();
    init_tracing();
    process::exit(run());
}
