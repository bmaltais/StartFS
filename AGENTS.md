## Agent skills

### Issue tracker

Issues and specs live in GitHub Issues (bmaltais/startfs), via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## Navigating the code

- Design decisions, non-goals and success criteria: `DESIGN.md`. Measured results and how to rerun them: `docs/validation.md` and `scripts/`.
- Behaviour tests go through the cluster harness (`crates/startfs-harness`), which drives `FsOps` and the daemon control interface under a simulated network and clock. Test behaviour there; leave internals untested.
- The checks CI runs (fmt, clippy with warnings denied, test) are in `.github/workflows/ci.yml`; run the same three before reporting done.

