//! `startfs add` with a scripted target machine and a scripted local daemon: the order of the
//! steps, what is revoked when, and what each failure leaves behind. Real SSH is tried by hand.

use startfs::add::{
    AddError, Local, Output, Plan, Remote, TOKEN_TTL, Via, add, choose_transport, quote,
};
use startfs_core::error::Error as CoreError;
use std::cell::RefCell;
use std::time::Duration;

/// A target machine: answers by the first rule whose pattern the command contains, and records
/// every command it was asked to run.
struct FakeRemote {
    rules: Vec<(&'static str, bool, &'static str)>,
    log: Vec<String>,
}

impl FakeRemote {
    /// A Linux machine with FUSE, linger on, where everything works.
    fn linux() -> Self {
        FakeRemote {
            rules: vec![
                ("uname", true, "Linux\n"),
                ("fusermount", true, ""),
                ("show-user", true, "Linger=yes\n"),
            ],
            log: Vec::new(),
        }
    }
    /// Make commands containing `pattern` answer `success` with `stdout`, ahead of the rest.
    fn answer(mut self, pattern: &'static str, success: bool, stdout: &'static str) -> Self {
        self.rules.insert(0, (pattern, success, stdout));
        self
    }
}

impl Remote for FakeRemote {
    fn run(&mut self, command: &str, _interactive: bool) -> Result<Output, AddError> {
        self.log.push(command.to_string());
        let (success, stdout) = self
            .rules
            .iter()
            .find(|(p, _, _)| command.contains(p))
            .map_or((true, ""), |(_, s, o)| (*s, *o));
        Ok(Output {
            success,
            stdout: stdout.to_string(),
        })
    }
}

struct FakeLocal {
    encrypted: bool,
    events: RefCell<Vec<String>>,
}

impl FakeLocal {
    fn new(encrypted: bool) -> Self {
        FakeLocal {
            encrypted,
            events: RefCell::new(Vec::new()),
        }
    }
    fn events(&self) -> Vec<String> {
        self.events.borrow().clone()
    }
}

impl Local for FakeLocal {
    fn encrypted(&self) -> Result<bool, CoreError> {
        Ok(self.encrypted)
    }
    fn mint_token(&self, ttl: Duration) -> Result<String, CoreError> {
        self.events
            .borrow_mut()
            .push(format!("mint {}s", ttl.as_secs()));
        Ok("startfs1-fs.tok@100.64.0.1:7600".to_string())
    }
    fn revoke(&self, token: &str) -> Result<(), CoreError> {
        self.events.borrow_mut().push(format!("revoke {token}"));
        Ok(())
    }
}

fn plan(storage_only: bool) -> Plan {
    Plan {
        version: "v1.16.6".to_string(),
        offer: Some("200G".to_string()),
        site: Some("home".to_string()),
        storage_only,
    }
}

fn position(log: &[String], needle: &str) -> usize {
    log.iter()
        .position(|c| c.contains(needle))
        .unwrap_or_else(|| panic!("no `{needle}` in {log:#?}"))
}

#[test]
fn a_storage_only_add_installs_the_pinned_version_then_joins_then_revokes() {
    let mut remote = FakeRemote::linux();
    let local = FakeLocal::new(false);
    let report = add(&mut remote, &local, &plan(true)).unwrap();
    assert!(report.warnings.is_empty());

    let install = position(&remote.log, "install.sh");
    let join = position(&remote.log, " join ");
    assert!(install < join);
    assert!(remote.log[install].contains("STARTFS_VERSION='v1.16.6'"));
    assert!(
        remote.log[join].ends_with("--offer '200G' --site 'home' --storage-only"),
        "{}",
        remote.log[join]
    );
    // Storage-only needs no FUSE: it is never looked for.
    assert!(!remote.log.iter().any(|c| c.contains("fusermount")));
    assert_eq!(
        local.events(),
        [
            format!("mint {}s", TOKEN_TTL.as_secs()),
            "revoke startfs1-fs.tok@100.64.0.1:7600".to_string()
        ]
    );
}

#[test]
fn a_failed_install_or_join_revokes_the_token_and_uninstalls_nothing() {
    for (failing, expected) in [("install.sh", "install"), (" join ", "join")] {
        let mut remote = FakeRemote::linux().answer(failing, false, "");
        let local = FakeLocal::new(false);
        let err = add(&mut remote, &local, &plan(true)).unwrap_err();
        match (expected, &err) {
            ("install", AddError::Install) | ("join", AddError::Join) => {}
            _ => panic!("unexpected {err}"),
        }
        assert_eq!(
            local.events().len(),
            2,
            "minted then revoked: {:?}",
            local.events()
        );
        assert!(local.events()[1].starts_with("revoke "));
        assert!(
            !remote
                .log
                .iter()
                .any(|c| c.contains("rm ") || c.contains("uninstall"))
        );
    }
}

#[test]
fn a_join_failure_says_it_is_installed_and_a_rerun_succeeds() {
    let mut remote = FakeRemote::linux().answer(" join ", false, "");
    let local = FakeLocal::new(false);
    let err = add(&mut remote, &local, &plan(true)).unwrap_err();
    assert!(
        err.to_string()
            .contains("installed but not joined; re-run `startfs add`"),
        "{err}"
    );

    // The same target, joining works this time: the install runs again without harm.
    let mut remote = FakeRemote::linux();
    add(&mut remote, &local, &plan(true)).unwrap();
    assert!(remote.log.iter().any(|c| c.contains("install.sh")));
}

#[test]
fn a_mounting_add_installs_fuse_with_the_package_manager_the_target_has() {
    for (manager, command) in [
        ("pacman", "sudo pacman -S --needed --noconfirm fuse3"),
        ("apt-get", "sudo apt-get install -y fuse3"),
        ("dnf", "sudo dnf install -y fuse3"),
    ] {
        let mut remote = FakeRemote::linux()
            .answer("command -v pacman", manager == "pacman", "")
            .answer("command -v apt-get", manager == "apt-get", "")
            .answer("command -v dnf", manager == "dnf", "")
            .answer("fusermount", false, "");
        let local = FakeLocal::new(false);
        add(&mut remote, &local, &plan(false)).unwrap();
        let fuse = position(&remote.log, command);
        assert!(
            fuse < position(&remote.log, "install.sh"),
            "FUSE comes before StartFS: {:#?}",
            remote.log
        );
        assert!(fuse < position(&remote.log, " join "));
        assert!(!remote.log[position(&remote.log, " join ")].contains("--storage-only"));
    }
}

#[test]
fn a_mounting_add_with_fuse_already_there_installs_none() {
    let mut remote = FakeRemote::linux();
    add(&mut remote, &FakeLocal::new(false), &plan(false)).unwrap();
    assert!(!remote.log.iter().any(|c| c.contains("fuse3")));
}

#[test]
fn missing_fuse_with_no_package_manager_or_no_sudo_fails_before_a_token_exists() {
    let mut remote = FakeRemote::linux()
        .answer("command -v pacman", false, "")
        .answer("command -v apt-get", false, "")
        .answer("command -v dnf", false, "")
        .answer("fusermount", false, "");
    let local = FakeLocal::new(false);
    assert!(matches!(
        add(&mut remote, &local, &plan(false)),
        Err(AddError::NoPackageManager)
    ));
    assert!(local.events().is_empty());

    let mut remote = FakeRemote::linux()
        .answer("fusermount", false, "")
        .answer("sudo apt-get", false, "")
        .answer("command -v pacman", false, "");
    let local = FakeLocal::new(false);
    assert!(matches!(
        add(&mut remote, &local, &plan(false)),
        Err(AddError::FuseInstall)
    ));
    assert!(local.events().is_empty());
}

#[test]
fn linger_that_cannot_be_enabled_is_a_warning_after_a_successful_join() {
    let mut remote = FakeRemote::linux()
        .answer("show-user", true, "Linger=no\n")
        .answer("enable-linger", false, "");
    let report = add(&mut remote, &FakeLocal::new(false), &plan(true)).unwrap();
    assert_eq!(report.warnings.len(), 1);
    assert!(report.warnings[0].contains("enable-linger"));
}

#[test]
fn linger_is_enabled_when_it_is_off_and_left_alone_when_on() {
    let mut off = FakeRemote::linux().answer("show-user", true, "Linger=no\n");
    add(&mut off, &FakeLocal::new(false), &plan(true)).unwrap();
    assert!(
        off.log
            .iter()
            .any(|c| c.contains("sudo loginctl enable-linger"))
    );

    let mut on = FakeRemote::linux();
    add(&mut on, &FakeLocal::new(false), &plan(true)).unwrap();
    assert!(!on.log.iter().any(|c| c.contains("enable-linger")));
}

#[test]
fn an_encrypted_filesystem_is_refused_before_anything_runs_remotely() {
    let mut remote = FakeRemote::linux();
    let local = FakeLocal::new(true);
    assert!(matches!(
        add(&mut remote, &local, &plan(true)),
        Err(AddError::Encrypted)
    ));
    assert!(remote.log.is_empty());
    assert!(local.events().is_empty());
}

#[test]
fn a_target_that_is_not_linux_is_refused_before_a_token_exists() {
    let mut remote = FakeRemote::linux().answer("uname", true, "Darwin\n");
    let local = FakeLocal::new(false);
    let err = add(&mut remote, &local, &plan(true)).unwrap_err();
    assert!(
        matches!(err, AddError::UnsupportedSystem(ref s) if s == "Darwin"),
        "{err}"
    );
    assert!(local.events().is_empty());
}

#[test]
fn a_version_that_is_not_a_release_tag_never_reaches_a_shell() {
    let mut remote = FakeRemote::linux();
    let mut bad = plan(true);
    bad.version = "v1; rm -rf ~".to_string();
    assert!(matches!(
        add(&mut remote, &FakeLocal::new(false), &bad),
        Err(AddError::BadVersion(_))
    ));
    assert!(remote.log.is_empty());
}

#[test]
fn values_are_quoted_so_a_site_name_cannot_inject_commands() {
    assert_eq!(quote("home"), "'home'");
    assert_eq!(quote("a'; reboot; '"), "'a'\\''; reboot; '\\'''");
    let mut remote = FakeRemote::linux();
    let mut p = plan(true);
    p.site = Some("x'; reboot; '".to_string());
    add(&mut remote, &FakeLocal::new(false), &p).unwrap();
    assert!(remote.log[position(&remote.log, " join ")].contains("--site 'x'\\''; reboot; '\\'''"));
}

#[test]
fn the_transport_follows_via_and_otherwise_the_tailnet() {
    assert_eq!(
        choose_transport(Some(Via::Ssh), || panic!("not asked")),
        Via::Ssh
    );
    assert_eq!(
        choose_transport(Some(Via::Tailscale), || panic!("not asked")),
        Via::Tailscale
    );
    assert_eq!(choose_transport(None, || true), Via::Tailscale);
    assert_eq!(choose_transport(None, || false), Via::Ssh);
}
