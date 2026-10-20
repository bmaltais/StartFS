//! The installer a running daemon upgrades itself with (ADR 0007). It is the
//! installer script, embedded in the binary so it is the one this release shipped
//! with, run on the daemon's behalf: the fetch, checksum, swap and rollback logic
//! lives only there, and the daemon has no download path of its own.

use startfs_core::protocol::PROTOCOL_VERSION;
use startfs_core::upgrade::{InstallError, Installer, Release};
use std::fs::{File, OpenOptions};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

const SCRIPT: &str = include_str!("../../../scripts/install.sh");

/// The look at who is using the mount: it scans every process, which takes seconds
/// on a busy machine, so it runs beside the daemon and is polled.
enum MountCheck {
    Idle,
    Running(Child),
}

enum Staging {
    Idle,
    /// The script is fetching and checking the release.
    Running(Child),
    Done(Result<Release, InstallError>),
}

pub struct ScriptInstaller {
    script: PathBuf,
    log: PathBuf,
    install_dir: PathBuf,
    mountpoint: Option<PathBuf>,
    staging: Mutex<Staging>,
    mount_check: Mutex<MountCheck>,
}

impl ScriptInstaller {
    /// An installer for the binary that is running, keeping its script and its log in `data_dir`.
    pub fn new(data_dir: &Path, mountpoint: Option<PathBuf>) -> Self {
        let install_dir = std::env::var_os("INSTALL_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|exe| exe.parent().map(Path::to_path_buf))
            })
            .unwrap_or_default();
        Self {
            script: data_dir.join("install.sh"),
            log: data_dir.join("upgrade.log"),
            install_dir,
            mountpoint,
            staging: Mutex::new(Staging::Idle),
            mount_check: Mutex::new(MountCheck::Idle),
        }
    }

    /// The script, set up to work on the directory the running binary lives in.
    fn command(&self) -> Result<Command, InstallError> {
        std::fs::write(&self.script, SCRIPT)
            .map_err(|e| format!("cannot write {}: {e}", self.script.display()))?;
        let mut cmd = Command::new("sh");
        cmd.arg(&self.script).env("INSTALL_DIR", &self.install_dir);
        if let Some(mount) = &self.mountpoint {
            cmd.env("STARTFS_MOUNTPOINT", mount);
        }
        Ok(cmd)
    }

    /// Where the script's own output goes: a log the user can read.
    fn log_file(&self) -> Result<File, InstallError> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .map_err(|e| format!("cannot open {}: {e}", self.log.display()))
    }

    /// The last thing the script said about why it stopped.
    fn last_complaint(&self) -> String {
        std::fs::read_to_string(&self.log)
            .ok()
            .and_then(|log| {
                log.lines()
                    .rev()
                    .find_map(|l| l.strip_prefix("install: ").map(str::to_string))
            })
            .unwrap_or_else(|| format!("see {}", self.log.display()))
    }

    fn staged_release(&self) -> Result<Release, InstallError> {
        let binary = self.install_dir.join("startfs.staged").join("startfs");
        let out = Command::new(&binary)
            .arg("--version")
            .output()
            .map_err(|e| format!("cannot run the staged {}: {e}", binary.display()))?;
        parse_version(&String::from_utf8_lossy(&out.stdout))
    }
}

/// `startfs 1.10.0 (protocol 15)`: the version and the protocol the binary reports.
fn parse_version(text: &str) -> Result<Release, InstallError> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let protocol = text
        .split("(protocol ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .and_then(|n| n.trim().parse().ok());
    match (words.get(1), protocol) {
        (Some(version), Some(protocol)) => Ok(Release {
            version: (*version).to_string(),
            protocol,
        }),
        _ => Err(format!("the staged binary reports no protocol: {text:?}")),
    }
}

impl Installer for ScriptInstaller {
    fn current(&self) -> Release {
        Release {
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
        }
    }

    fn latest_stable(&self) -> Result<String, InstallError> {
        let out = self
            .command()?
            .env("STARTFS_LATEST", "1")
            .output()
            .map_err(|e| format!("cannot run the installer: {e}"))?;
        let tag = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !tag.is_empty() {
            Ok(tag)
        } else {
            Err(String::from_utf8_lossy(&out.stderr)
                .lines()
                .last()
                .map_or_else(|| "the installer gave no release".into(), str::to_string))
        }
    }

    fn begin_stage(&self, tag: &str) {
        let started = (|| -> Result<Child, InstallError> {
            let log = self.log_file()?;
            self.command()?
                .env("STARTFS_STAGE", "1")
                .env("STARTFS_VERSION", tag)
                .stdin(Stdio::null())
                .stdout(log.try_clone().map_err(|e| e.to_string())?)
                .stderr(log)
                .spawn()
                .map_err(|e| format!("cannot run the installer: {e}"))
        })();
        *self.staging.lock().unwrap() = match started {
            Ok(child) => Staging::Running(child),
            Err(why) => Staging::Done(Err(why)),
        };
    }

    fn staged(&self) -> Option<Result<Release, InstallError>> {
        let mut staging = self.staging.lock().unwrap();
        if let Staging::Running(child) = &mut *staging {
            match child.try_wait() {
                Ok(None) => return None,
                Ok(Some(status)) if status.success() => {
                    *staging = Staging::Done(self.staged_release());
                }
                Ok(Some(_)) | Err(_) => *staging = Staging::Done(Err(self.last_complaint())),
            }
        }
        match &*staging {
            Staging::Idle => None,
            Staging::Running(_) => None,
            Staging::Done(result) => Some(result.clone()),
        }
    }

    fn discard(&self) {
        if let Staging::Running(child) = &mut *self.staging.lock().unwrap() {
            #[allow(
                clippy::let_underscore_must_use,
                reason = "best-effort: the script may have finished already"
            )]
            let _ = child.kill();
        }
        *self.staging.lock().unwrap() = Staging::Idle;
        #[allow(
            clippy::let_underscore_must_use,
            reason = "best-effort: nothing may be staged"
        )]
        let _ = std::fs::remove_dir_all(self.install_dir.join("startfs.staged"));
    }

    fn mount_in_use(&self) -> Option<bool> {
        let mut check = self.mount_check.lock().unwrap();
        match &mut *check {
            MountCheck::Running(child) => {
                let status = child.try_wait().transpose()?;
                *check = MountCheck::Idle;
                // When the script cannot say, the mount is taken to be in use: a restart
                // is never made silently over processes that may be using it.
                Some(status.map_or(true, |s| s.code() != Some(0)))
            }
            MountCheck::Idle => {
                let started = self.command().ok().and_then(|mut cmd| {
                    cmd.env("STARTFS_CHECK_MOUNT", "1")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                        .ok()
                });
                match started {
                    Some(child) => {
                        *check = MountCheck::Running(child);
                        None
                    }
                    None => Some(true),
                }
            }
        }
    }

    fn swap(&self) -> Result<(), InstallError> {
        let log = self.log_file()?;
        let swap_env: Vec<(String, String)> = std::env::vars()
            .filter(|(name, _)| name.starts_with("STARTFS_") || name == "INSTALL_DIR")
            .chain([
                ("STARTFS_APPLY_STAGED".into(), "1".into()),
                // The daemon has decided; no one is at a terminal to ask.
                ("STARTFS_FORCE".into(), "1".into()),
                ("INSTALL_DIR".into(), self.install_dir.display().to_string()),
            ])
            .chain(
                self.mountpoint
                    .iter()
                    .map(|m| ("STARTFS_MOUNTPOINT".into(), m.display().to_string())),
            )
            .collect();
        std::fs::write(&self.script, SCRIPT)
            .map_err(|e| format!("cannot write {}: {e}", self.script.display()))?;
        // The script stops this daemon's service, which stops everything started from
        // it, the script included. Under systemd it runs in a unit of its own.
        let transient = std::env::var_os("STARTFS_NO_SERVICE").is_none()
            && Command::new("systemd-run")
                .arg("--version")
                .stdout(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
        let mut cmd = if transient {
            let mut cmd = Command::new("systemd-run");
            cmd.args(["--user", "--collect", "--quiet", "--unit=startfs-upgrade"]);
            for (name, value) in &swap_env {
                cmd.arg(format!("--setenv={name}={value}"));
            }
            cmd.arg("sh").arg(&self.script);
            cmd
        } else {
            let mut cmd = Command::new("sh");
            cmd.arg(&self.script).envs(swap_env).process_group(0);
            cmd
        };
        cmd.stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("cannot run the installer: {e}"))
    }
}
