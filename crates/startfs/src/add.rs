//! `startfs add`: bring a new machine into the filesystem over SSH, from the CLI of a member.
//!
//! The sequence runs here, with the invoking user's own credentials, and never in the daemon
//! (ADR 0011): a daemon that could open SSH sessions to its peers would turn one compromised
//! member into code execution on all of them. Everything the machine does goes through two
//! small seams: [`Remote`] (run a command on the target) and [`Local`] (what this member's
//! daemon is asked), so the order of steps and the failure handling are tested with fakes.

use startfs_core::error::Error as CoreError;
use std::process::{Command, Stdio};
use std::time::Duration;

const INSTALL_URL: &str =
    "https://raw.githubusercontent.com/bmaltais/StartFS/main/scripts/install.sh";
/// How long the join token lives. It is revoked as soon as the add ends; this is the backstop.
pub const TOKEN_TTL: Duration = Duration::from_secs(10 * 60);
/// Where the installer puts the binary, as the remote shell spells it (`INSTALL_DIR` stays default).
const REMOTE_BIN: &str = "\"$HOME/.local/bin/startfs\"";

/// What a command on the target said. `stdout` is only kept for commands that were not
/// interactive; an interactive command talks to the user's terminal directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub success: bool,
    pub stdout: String,
}

/// Run a command on the target machine.
pub trait Remote {
    /// `interactive`: give the command the user's terminal (a password or sudo prompt can be
    /// answered, progress is shown) instead of capturing its output.
    fn run(&mut self, command: &str, interactive: bool) -> Result<Output, AddError>;
}

/// What `add` asks of this member's daemon.
pub trait Local {
    fn encrypted(&self) -> Result<bool, CoreError>;
    fn mint_token(&self, ttl: Duration) -> Result<String, CoreError>;
    fn revoke(&self, token: &str) -> Result<(), CoreError>;
}

impl Local for startfs_core::control::Client {
    fn encrypted(&self) -> Result<bool, CoreError> {
        Ok(self.status()?.filesystem.is_some_and(|f| f.encrypted))
    }
    fn mint_token(&self, ttl: Duration) -> Result<String, CoreError> {
        self.new_token_for(ttl)
    }
    fn revoke(&self, token: &str) -> Result<(), CoreError> {
        startfs_core::control::Client::revoke(self, token)
    }
}

/// What the new member should be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// A release tag like `v1.16.6`: the version this member runs, so the joiner speaks the
    /// cluster's protocol.
    pub version: String,
    pub offer: Option<String>,
    pub site: Option<String>,
    pub storage_only: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error(
        "this filesystem is encrypted: joining needs the recovery key, which must not travel on a command line. Install and join by hand (`startfs join TOKEN --key KEY`)"
    )]
    Encrypted,
    #[error("{0} is not a release tag like v1.16.6")]
    BadVersion(String),
    #[error("cannot run ssh: {0}")]
    Spawn(std::io::Error),
    #[error("cannot reach the machine or log in: {0}")]
    Unreachable(String),
    #[error(
        "the target runs {0}; `startfs add` supports Linux only (no macOS or Windows release exists yet)"
    )]
    UnsupportedSystem(String),
    #[error(
        "fuse3 is missing and no supported package manager (pacman, apt, dnf) was found; install fuse3, or add the machine with --storage-only"
    )]
    NoPackageManager,
    #[error("installing fuse3 failed (sudo may have needed a password it could not get)")]
    FuseInstall,
    #[error("installing StartFS on the machine failed")]
    Install,
    #[error(
        "`startfs join` failed on the machine (installed but not joined; re-run `startfs add`)"
    )]
    Join,
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// What happened, beyond success: things worth saying that did not fail the add.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub warnings: Vec<String>,
}

/// Quote `text` for a POSIX shell as one word.
pub fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn check_version(version: &str) -> Result<(), AddError> {
    let ok = version.strip_prefix('v').is_some_and(|rest| {
        rest.chars().next().is_some_and(|c| c.is_ascii_digit())
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    });
    if ok {
        Ok(())
    } else {
        Err(AddError::BadVersion(version.to_string()))
    }
}

fn join_command(token: &str, plan: &Plan) -> String {
    let mut cmd = format!("{REMOTE_BIN} join {}", quote(token));
    if let Some(offer) = &plan.offer {
        cmd.push_str(&format!(" --offer {}", quote(offer)));
    }
    if let Some(site) = &plan.site {
        cmd.push_str(&format!(" --site {}", quote(site)));
    }
    if plan.storage_only {
        cmd.push_str(" --storage-only");
    }
    cmd
}

/// Package managers `add` can install FUSE with: the program to look for, and the install command.
const PACKAGE_MANAGERS: [(&str, &str); 3] = [
    ("pacman", "sudo pacman -S --needed --noconfirm fuse3"),
    ("apt-get", "sudo apt-get install -y fuse3"),
    ("dnf", "sudo dnf install -y fuse3"),
];

fn install_fuse(remote: &mut dyn Remote) -> Result<(), AddError> {
    for (program, install) in PACKAGE_MANAGERS {
        if remote
            .run(&format!("command -v {program} >/dev/null 2>&1"), false)?
            .success
        {
            return if remote.run(install, true)?.success {
                Ok(())
            } else {
                Err(AddError::FuseInstall)
            };
        }
    }
    Err(AddError::NoPackageManager)
}

/// Add the machine behind `remote` to this member's filesystem.
///
/// Nothing is installed or minted until the cheap refusals are out of the way. The token is
/// revoked on every path after it is minted. A failed add never uninstalls: the installer is
/// re-runnable, so a second `add` picks up where this one stopped.
pub fn add(remote: &mut dyn Remote, local: &dyn Local, plan: &Plan) -> Result<Report, AddError> {
    check_version(&plan.version)?;
    if local.encrypted()? {
        return Err(AddError::Encrypted);
    }
    let system = remote.run("uname -s", false)?;
    let system = system.stdout.trim();
    if !system.is_empty() && system != "Linux" {
        return Err(AddError::UnsupportedSystem(system.to_string()));
    }
    if !plan.storage_only {
        let has_fuse = remote.run(
            "command -v fusermount3 >/dev/null 2>&1 || command -v fusermount >/dev/null 2>&1",
            false,
        )?;
        if !has_fuse.success {
            install_fuse(remote)?;
        }
    }

    let token = local.mint_token(TOKEN_TTL)?;
    let result = provision(remote, &token, plan);
    let mut report = Report::default();
    if let Err(e) = local.revoke(&token) {
        report.warnings.push(format!(
            "could not revoke the join token (it expires in {} minutes): {e}",
            TOKEN_TTL.as_secs() / 60
        ));
    }
    result?;

    // Linger lets the user service start at boot and survive logout.
    if let Ok(linger) = remote.run("loginctl show-user \"$USER\" -p Linger", false)
        && linger.success
        && !linger.stdout.contains("Linger=yes")
    {
        match remote.run("sudo loginctl enable-linger \"$USER\"", true) {
            Ok(o) if o.success => {}
            _ => report.warnings.push(
                "could not enable linger: run `sudo loginctl enable-linger $USER` there so it starts at boot".to_string(),
            ),
        }
    }
    Ok(report)
}

fn provision(remote: &mut dyn Remote, token: &str, plan: &Plan) -> Result<(), AddError> {
    let install = format!(
        "curl -fsSL {INSTALL_URL} | STARTFS_VERSION={} sh",
        quote(&plan.version)
    );
    if !remote.run(&install, true)?.success {
        return Err(AddError::Install);
    }
    if !remote.run(&join_command(token, plan), true)?.success {
        return Err(AddError::Join);
    }
    Ok(())
}

/// How to reach the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Ssh,
    Tailscale,
}

/// `--via` if given; otherwise `tailscale ssh` when the host is a tailnet machine
/// (`tailscale_knows`), and plain `ssh` when not.
pub fn choose_transport(via: Option<Via>, tailscale_knows: impl FnOnce() -> bool) -> Via {
    via.unwrap_or_else(|| {
        if tailscale_knows() {
            Via::Tailscale
        } else {
            Via::Ssh
        }
    })
}

/// True when `tailscale` is installed and knows `host` (a name or address, without `user@`).
pub fn tailscale_knows(host: &str) -> bool {
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    Command::new("tailscale")
        .args(["ip", "-4", host])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Commands run by the user's own `ssh` (or `tailscale ssh`), in their terminal.
pub struct SshRemote {
    pub via: Via,
    pub host: String,
}

impl Remote for SshRemote {
    fn run(&mut self, command: &str, interactive: bool) -> Result<Output, AddError> {
        let mut cmd = match self.via {
            Via::Ssh => Command::new("ssh"),
            Via::Tailscale => {
                let mut c = Command::new("tailscale");
                c.arg("ssh");
                c
            }
        };
        if interactive {
            cmd.arg("-t");
        }
        cmd.arg(&self.host).arg(command);
        if interactive {
            let status = cmd.status().map_err(AddError::Spawn)?;
            return Ok(Output {
                success: status.success(),
                stdout: String::new(),
            });
        }
        cmd.stdin(Stdio::null()).stderr(Stdio::inherit());
        let out = cmd.output().map_err(AddError::Spawn)?;
        // ssh itself failing (unreachable, refused) is 255; a command's own failure is not.
        if out.status.code() == Some(255) {
            return Err(AddError::Unreachable(self.host.clone()));
        }
        Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        })
    }
}
