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

`fork-manifest.yml` runs the same comparison on every PR, so a commit with no line at the top
of `patches.txt` fails while the PR is open rather than at the next sync. `sync/*` branches are
exempt: they land by force-push, and their commits against `main` are upstream's.

| Patch | Upstream PR | State |
| --- | --- | --- |
| `fix(git): keep the old side of replacement hunks` | not submitted | worth sending; upstream's `parse_diff` also keeps the old side of pure-deletion hunks only |
| `fix(semantic): ignore type-only re-exports for side-effect changes` | not submitted | worth sending with the side-effect patch; addresses upstream #103, still open at `v2.2.0` |
| `fix(semantic): carry side-effect changes through importers that re-export` | not submitted | worth sending with the side-effect patch; addresses upstream #103, still open at `v2.2.0` |
| `fix(semantic): carry side-effect changes through re-exporting barrels` | not submitted | worth sending with the side-effect patch; addresses upstream #103, still open at `v2.2.0` |
| `fix(semantic): treat re-exports as loading the module for side-effect changes` | not submitted | worth sending with the side-effect patch; addresses upstream #103, still open at `v2.2.0` |
| `fix(semantic): carry side-effect changes through importers without exports` | not submitted | worth sending with the side-effect patch; addresses upstream #103, still open at `v2.2.0` |
| `fix(semantic): propagate side-effect imports and top-level side-effect changes` | not submitted | worth sending; addresses upstream #103, still open at `v2.2.0` |
| `ci(fork): delete the previews of closed pull requests` | fork-only | — |
| `ci(fork): fail a pull request whose commits are not in the manifest` | fork-only | — |
| `chore(fork): stop the sync on conflicts in merge=union files` | fork-only | — |
| `fix(resolve): resolve package.json "imports" subpath specifiers` | not submitted | worth sending; upstream's `is_workspace_specifier` still drops `#` specifiers |
| `chore(fork): list the asset specifier patch in the manifest` | fork-only | — |
| `fix(core): affect every project when lockfile analysis fails` | not submitted | worth sending; upstream still logs the failure at `debug` and carries on |
| `fix(lockfile): read pnpm v9 dependencies from snapshots` | not submitted | worth sending; upstream's parser still reads `packages` only |
| `fix(lockfile): reject a pnpm lockfile that is not a mapping` | not submitted | worth sending; upstream's parser still reads such a file as an empty package list |
| `feat: resolve bare package specifiers for asset references (#5)` | not submitted | no upstream equivalent at `v2.2.0` |
| `chore(fork): automate releases and the upstream rebase` | fork-only | — |
| `fix(semantic): resolve project tsconfig paths aliases` | `upstream#95` | open; absent from upstream `main` |
| `fix(semantic): cascade the default export through dynamic imports` | not submitted | prior art: `upstream#12`, `upstream#69` |
| `feat(workspace): merge package-manager workspace members into Nx discovery` | `upstream#78` | open; absent from upstream `main` |
| `ci(fork): allow manual dispatch and disable the npm publish job` | fork-only | — |

Keep the PR column current: it is the difference between "we chose to carry this" and "we
forgot to send it". The dynamic-import cascade is unsubmitted, and upstream has already touched the same code in
`upstream#12` and `upstream#69`, which is where a submission should build from. The `snapshots`,
fail-closed lockfile, bare-specifier asset and `imports` patches are unsubmitted and have no upstream equivalent.

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

By hand, the whole sync is the sequence under
[When it stops on a conflict](#when-it-stops-on-a-conflict). Do not skip its override: a
plain `git rebase` does not stop on most conflicts.

### When it stops on a conflict

The workflow stops at any conflict rather than guessing at a semantic merge, which is how a
fork silently loses behavior. Upstream's `.gitattributes` sets `merge=union` on `*.ts`,
`*.tsx`, `*.rs`, `*.js`, `*.json` and `*.debug`, which keeps both sides of a conflict in those
files with no markers, so a plain rebase carries on: replaying the stack onto `v2.2.0` stopped
once where it should have stopped six times. `info/attributes` outranks `.gitattributes`, so
the workflow overrides it there rather than editing upstream's file, and a rebase on your
machine needs the same override. Conflicts are resolved by a person, locally:

```bash
git config rerere.enabled true      # once per clone
git config rerere.autoupdate true
printf '%s merge=text\n' '*.ts' '*.tsx' '*.rs' '*.js' '*.json' '*.debug' >> "$(git rev-parse --git-path info/attributes)"   # once per clone
git fetch upstream --tags
git rebase v2.1.0                   # resolve, git add, git rebase --continue
git push --force-with-lease origin HEAD:main
```

The override lives in the clone's git directory, so every worktree of the clone shares it and
a fresh clone needs it again.

`rerere` is worth enabling in your own clone: it records each resolution and replays it on
your later rebases, so a conflict that recurs every sync is resolved once. It is
deliberately **not** wired into the workflow — rerere only replays resolutions recorded in
the same `.git`, and a resolution made on your machine never reaches a runner, so caching
`.git/rr-cache` in CI would only ever restore caches that never held a resolution.

Conflicts come from the patches that edit upstream code in place. The `v2.2.0` sync conflicted in
`CI.yml`, `semantic/analyzer.rs` and `semantic/mod.rs` (tsconfig paths),
`tests/integration_test.rs` (tsconfig paths, `imports`), `core.rs` (asset specifiers) and
`lockfile.rs` (both lockfile patches). Restructuring them to be additive — a new module plus a
one-line call site — would remove most of the conflict surface, and is the highest-value
follow-up here.

## Releases

Pushing to `main` runs upstream's CI, which builds all seven targets. `fork-release.yml`
then picks up those artifacts and publishes a GitHub release tagged
`fork-v<upstream-version>-<n>` with the per-target `domino-<target>` executables, the
`.node` bindings and `SHA256SUMS`. It is the push-side twin of `preview-release.yml`, which
does the same for pull requests.

`fork-preview-cleanup.yml` deletes those previews. Upstream's `cleanup-preview-release.yml` only
runs in upstream's repository and only at close, which misses a PR merged before its CI ends,
since that PR publishes its preview afterwards. The fork's workflow also runs after each
preview run, whatever its outcome, and sweeps every preview whose PR is no longer open.

Upstream's own `publish` job in `CI.yml` is disabled with `if: false` rather than deleted, so
upstream's edits to it keep applying cleanly. Nothing is published to npm.
`domino --version` reports the upstream base version, since the fork does not bump it.
