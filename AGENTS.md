## Agent skills

### Issue tracker

Issues and specs live in GitHub Issues (bmaltais/StartFS), via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## Navigating the code

- Design decisions, non-goals and success criteria: `DESIGN.md`. Measured results and how to rerun them: `docs/validation.md` and `scripts/`.
- Behaviour tests go through the cluster harness (`crates/startfs-harness`), which drives `FsOps` and the daemon control interface under a simulated network and clock. Test behaviour there; leave internals untested. Its tests share `startfs_harness::noise`, `Cluster::with_filesystem`, `with_offers` and `elapse` (in `crates/startfs-harness/src`): use them instead of defining copies.
- The checks CI runs (fmt, clippy with warnings denied, test) are in `.github/workflows/ci.yml`; `scripts/check.sh` runs the same checks, prints only failures and exits non-zero if one fails; run it unpiped and commit on its exit status, before reporting done.
- A node restart as `status` shows it (verdict and member states by second, on a real transport): `scripts/trace-restart.sh`. A release bump: `scripts/bump-version.sh X.Y.Z`.
- Idle cost (CPU, wakeups/s): `scripts/validate-idle.sh`; a live node's tick count and per-phase cost: `startfs activity`.
- Before/after on `main` for any `scripts/` profile: `scripts/ab.sh`.
- Where the suite's time goes (slowest binaries, or slowest tests in one): `scripts/test-times.sh`.
- Write amplification (bytes written to storage per byte of payload: copies, a patch, a delete; the daemon's figure and the disk's): `scripts/profile-amplification.sh`, with `scripts/ab.sh -r REF` to compare.
- Write-path speed (large file, many small files, syscall counts): `scripts/profile-write.sh`; the syscall counts need `strace` and `sudo` (`ptrace_scope` blocks attaching to the daemon).
