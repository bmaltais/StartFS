//! Control requests: what the CLI and the harness ask of the daemon.

use super::Daemon;
use crate::control::{Request, Response};
use crate::error::Error;

impl Daemon {
    /// Answer one control request. A failure becomes `Response::Error` with
    /// the error's display text, which is all the control wire format carries.
    pub fn handle_control_request(&self, req: Request) -> Response {
        self.fs.flush_all_logged(true, 0);
        self.fs.sync_logged();
        self.control_reply(req)
            .unwrap_or_else(|e| Response::Error(e.to_string()))
    }

    /// Say where the filesystem is mounted, so the CLI can name a file by its path there.
    pub fn set_mountpoint(&self, mountpoint: std::path::PathBuf) {
        *self.mountpoint.lock().unwrap() = Some(mountpoint);
    }

    /// A path as the filesystem knows it: one under the mount loses the mountpoint.
    fn fs_path(&self, path: &str) -> String {
        let mount = self.mountpoint.lock().unwrap().clone();
        match mount.and_then(|m| {
            std::path::Path::new(path)
                .strip_prefix(m)
                .ok()
                .map(|p| p.to_path_buf())
        }) {
            Some(rest) => format!("/{}", rest.display()),
            None => path.to_string(),
        }
    }

    fn control_reply(&self, req: Request) -> Result<Response, Error> {
        Ok(match req {
            Request::Create {
                name,
                copies,
                encrypt,
            } => {
                let (token, recovery_key) = self.create_filesystem(name, copies, encrypt)?;
                Response::Created {
                    token,
                    recovery_key,
                }
            }
            Request::NewToken => Response::Created {
                token: self.new_token()?,
                recovery_key: None,
            },
            Request::RecoveryKey => Response::RecoveryKey {
                key: self.new_recovery_key()?,
            },
            Request::Config {
                offer_bytes,
                weight_bytes,
                copies,
                activity,
                site,
                storage_only,
                keep_versions_secs,
            } => {
                self.configure(
                    offer_bytes,
                    weight_bytes,
                    copies,
                    activity,
                    site,
                    storage_only,
                )?;
                if let Some(secs) = keep_versions_secs {
                    self.membership.lock().unwrap().set_keep_versions(secs)?;
                }
                Response::Done
            }
            Request::Revoke { token } => {
                self.revoke_token(&token)?;
                Response::Done
            }
            Request::Leave => {
                self.begin_leave()?;
                Response::Done
            }
            Request::Remove { member } => {
                self.remove_member(member)?;
                Response::Done
            }
            Request::Join {
                token,
                offer_bytes,
                site,
                storage_only,
                recovery_key,
            } => {
                self.join(&token, offer_bytes, site, storage_only, recovery_key)?;
                Response::Joined
            }
            Request::Update { release, force } => Response::Updating(self.update(release, force)?),
            Request::Purge => Response::Purged(self.purge()?),
            Request::NodeStats => Response::NodeStats(self.node_reports()?),
            Request::Activity => Response::Activity(Box::new(self.activity_report()?)),
            Request::Status => Response::Status(Box::new(self.status()?)),
            Request::Restore { path, at } => {
                Response::Restored(self.fs.restore(&self.fs_path(&path), at)?)
            }
            Request::History { path } => Response::History(self.fs.history(&self.fs_path(&path))?),
        })
    }
}
