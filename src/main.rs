mod fs;
mod yadisk;

use std::{env, path::PathBuf, process};

use dotenvy::dotenv;
use fs::YandexDiskFs;
use fuser::{Config, MountOption};
use yadisk::YandexDiskClient;

fn usage() -> &'static str {
    "usage: discohack-daemon <mountpoint>"
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

fn main() {
    dotenv().ok();

    let mountpoint = match mountpoint_from_args() {
        Ok(path) => path,
        Err(message) => {
            eprintln!("{message}");
            process::exit(2);
        }
    };

    let token = match load_token() {
        Ok(token) => token,
        Err(message) => {
            eprintln!("{message}");
            process::exit(2);
        }
    };

    let client = match YandexDiskClient::new(token) {
        Ok(client) => client,
        Err(err) => {
            eprintln!("failed to initialize Yandex Disk client: {err}");
            process::exit(1);
        }
    };

    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };

    let fs = match YandexDiskFs::new(client, uid, gid) {
        Ok(fs) => fs,
        Err(err) => {
            eprintln!("failed to initialize Yandex Disk filesystem: {err}");
            process::exit(1);
        }
    };

    let mut config = Config::default();
    config.mount_options = vec![
        MountOption::RO,
        MountOption::FSName("yandex-disk-ro".into()),
    ];

    if let Err(err) = fuser::mount2(fs, &mountpoint, &config) {
        eprintln!("failed to mount {}: {err}", mountpoint.display());
        process::exit(1);
    }
}
