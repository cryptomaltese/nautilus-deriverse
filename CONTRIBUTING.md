# Contributing

This repository is a **generated mirror**: its contents are exported by an
automated sync from a private monorepo, which is the source of truth. Nothing
is ever committed directly here — every commit you see was produced by the
sync.

Contributions are welcome all the same. The flow is import, not merge:

1. Open a pull request here as usual.
2. A maintainer reviews it here, then imports the commits into the upstream
   monorepo via `git am`, so your authorship is preserved exactly as you
   wrote it.
3. The change lands in this repository with the next sync from upstream.
4. Your pull request is then closed with a comment linking to the landed
   commit.

**Honest caveat:** because the change arrives via the sync rather than a
GitHub merge, your pull request will show as *closed*, not *merged* — GitHub's
"merged" badge is structurally unavailable in this flow. The linked commit in
the close comment is the proof your change (and authorship) landed.

Issues are welcome too, and are triaged upstream the same way.

## Local development

```sh
cargo build
cargo test
```

MSRV 1.96.0. The suite is hermetic — live network probes are `#[ignore]`d.
Please keep it that way: no test may require network access, credentials, or
funded accounts unless it is `#[ignore]`d with a reason string.
