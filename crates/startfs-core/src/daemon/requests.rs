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

    fn control_reply(&self, req: Request) -> Result<Response, Error> {
        Ok(match req {
            Request::Create { name, copies } => Response::Created {
                token: self.create_filesystem(name, copies)?,
            },
            Request::NewToken => Response::Created {
                token: self.new_token()?,
            },
            Request::Config {
                offer_bytes,
                weight_bytes,
                copies,
                activity,
                site,
            } => {
                self.configure(offer_bytes, weight_bytes, copies, activity, site)?;
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
            } => {
                self.join(&token, offer_bytes, site)?;
                Response::Joined
            }
            Request::Update { release, force } => Response::Updating(self.update(release, force)?),
            Request::Purge => Response::Purged(self.purge()?),
            Request::NodeStats => Response::NodeStats(self.node_reports()?),
            Request::Activity => Response::Activity(Box::new(self.activity_report()?)),
            Request::Status => Response::Status(Box::new(self.status()?)),
        })
    }
}
