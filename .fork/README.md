# Fork maintenance

`gbleu/domino` is a patch stack on top of `frontops-dev/domino`. `main` is always
_upstream release + the patches listed in `patches.txt`, in that order, with no merge
commits_. Everything here exists to keep that sentence true.

## Rules

1. **Never merge upstream into `main`.** Syncing is a rebase; landing a sync is a
   force-push. A merge was tried once (`5539d2d`) and left four patches duplicated in
   history, which every later sync had to replay and re-resolve.
2. **Prefer a new file to an edited one.** A file upstream does not have can never
   conflict. The fork's release pipeline is its own workflow rather than a rewrite of
   upstream's, which is why `CI.yml` diverges by six lines instead of a hundred and sixty.
3. **Never bump `package.json` / `Cargo.toml`.** Those versions are upstream's. Releases
   are tagged `fork-v<upstream-version>-<n>`, so nothing tracked has to diverge.
   `optionalDependencies` stays pinned at upstream's last published version and is inert —
   the fork is not on npm.
4. **Every patch carries its own test.** A clean rebase proves a patch still _applies_;
   only a test proves it still _works_ after upstream refactors underneath it.

## The patch stack

`patches.txt` is the manifest, newest first, one patch per line as
`<commit subject> | <upstream PR>`. The sync workflow strips the second column and diffs
the first against `git log --format=%s upstream-tag..HEAD`, so the manifest is the tripwire
for a patch silently dropped during an automated rebase.

| Patch | Upstream PR | State |
| --- | --- | --- |
| `chore(fork): automate releases and the upstream rebase` | fork-only | — |
| `fix(semantic): resolve project tsconfig paths aliases` | `upstream#95` | open; absent from upstream `main` |
| `fix(semantic): cascade the default export through dynamic imports` | not submitted | prior art: `upstream#12`, `upstream#69` |
| `feat(workspace): merge package-manager workspace members into Nx discovery` | `upstream#78` | open; absent from upstream `main` |
| `ci(fork): allow manual dispatch and disable the npm publish job` | fork-only | — |

Keep the PR column current: it is the difference between "we chose to carry this" and "we
forgot to send it". Only one patch is still unsubmitted; upstream has already touched the
same code in `upstream#12` and `upstream#69`, which is where a submission should build from.

The cheapest maintenance is deletion, but note how upstream has actually absorbed fork work
so far: `upstream#75` and `upstream#77` were both superseded by upstream's own
reimplementations (`#85` and `#87`) rather than merged, and the fork's versions
(`4709dc7`, `65f0de3`) became dead weight that the old merge-based history kept replaying.
Expect the same for `#78` and `#95`: the equivalent may arrive under a different number.
Either way the rebase drops the patch by patch-id and the manifest check fails — delete the
line and land the sync.

## Syncing

`.github/workflows/upstream-sync.yml` runs Mondays at 06:00 UTC and on demand. It rebases
the stack onto the newest upstream `v*` tag, verifies the manifest, pushes `sync/<tag>` and
opens a PR. The PR diff against `main` is exactly what upstream changed.

Land it by force-pushing — **not** by merging:

```bash
git fetch origin
git push --force-with-lease origin origin/sync/v2.1.0:main
```

By hand, the whole sync is:

```bash
git fetch upstream --tags
git rebase v2.1.0
```

### When it stops on a conflict

The workflow refuses any conflict it has no recorded resolution for, because guessing at a
semantic merge is how a fork silently loses behavior. Resolve it locally once:

```bash
git config rerere.enabled true
git config rerere.autoupdate true
git rebase v2.1.0          # resolve, git add, git rebase --continue
```

`rerere` records the resolution; the workflow caches `.git/rr-cache` and replays it on every
later rebase. Resolve each conflict once, not once per sync.

The two patches that conflict are the ones that edit upstream functions in place
(`resolve_options.rs`, `workspace/mod.rs`). Restructuring them to be additive — a new module
plus a one-line call site — would remove most of the conflict surface, and is the
highest-value follow-up here.

## Releases

Pushing to `main` runs upstream's CI, which builds all seven targets. `fork-release.yml`
then picks up those artifacts and publishes a GitHub release tagged
`fork-v<upstream-version>-<n>` with the per-target `domino-<target>` executables, the
`.node` bindings and `SHA256SUMS`. It is the push-side twin of `preview-release.yml`, which
does the same for pull requests.

Upstream's own `publish` job in `CI.yml` is disabled with `if: false` rather than deleted, so
upstream's edits to it keep applying cleanly. Nothing is published to npm.
`domino --version` reports the upstream base version, since the fork does not bump it.
