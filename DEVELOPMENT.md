# Development

Build, test, and release commands for `hivemind`, plus the errors actually
hit while building this repo — not a generic checklist, real mistakes with
their real fixes.

Run everything from the repo root: `/Users/soulknower/Documents/projects/HiveMind`.

## Prerequisites (one-time)

```sh
brew install rust        # gives cargo + rustc
brew install gh          # GitHub CLI
gh auth status           # confirm you're logged in as BibhabenduMukherjee
```

## Build

```sh
cargo build -p harness-cli              # fast, unoptimized, for iterating
cargo build --release -p harness-cli    # optimized binary, what actually ships

# Check a single crate without building anything (fastest feedback loop):
cargo check -p harness-agent            # swap the crate name as needed

# Full workspace build (slower — prefer -p <crate> while iterating):
cargo build --workspace
```

Binary lands at `target/debug/hivemind` or `target/release/hivemind`.

## Test

Run all four every time before committing — this is the actual gate CI
enforces (`.github/workflows/ci.yml`), so failing any of these locally means
CI will fail too:

```sh
cargo fmt --all                                      # auto-formats in place
cargo fmt --all -- --check                            # CI's version: fails instead of fixing
cargo clippy --workspace --all-targets -- -D warnings # CI treats warnings as errors
cargo test --workspace
```

### Manual end-to-end check

Unit tests don't cover the live network path or the interactive terminal.
Before a release, actually run it:

```sh
export DEEPSEEK_API_KEY=sk-...
./target/release/hivemind activate -p "list the files here and summarize one"
```

For the interactive REPL (banner, `@` file completion, slash commands), you
have to eyeball it yourself — there's no way to script-verify a raw-mode
terminal UI:

```sh
./target/release/hivemind activate
# try: @<tab> for file completion, /help, /compact, /tier pro, /cost, Ctrl-D
```

## Try your own build without cutting a release

```sh
cp target/release/hivemind ~/.local/bin/hivemind
hivemind activate
```

If `command not found: hivemind` after this, your shell cached an old PATH
lookup — run `hash -r` or open a new terminal tab. (`~/.local/bin` should
already be on PATH via `~/.zshrc`; check with `echo $PATH | tr ':' '\n' |
grep local/bin` if unsure.)

## Release

This repo (`HiveMind`, private, source) never publishes to itself — release
binaries go to the public `BibhabenduMukherjee/HiveMind-releases` repo,
which has no source in it. The cross-repo publish needs the
`RELEASES_REPO_TOKEN` secret (see [Common errors](#common-errors) if it's
missing or expired).

```sh
# 1. Bump the version if this is a real release (breaking change = bump).
#    Edit `version = "..."` in the root Cargo.toml under [workspace.package].

# 2. Run the full test gate (see above) — don't skip this.

# 3. Commit and push to main first.
git add -A
git commit -m "..."
git push origin main

# 4. Tag and push the tag — THIS triggers the release build.
git tag v0.3.0
git push origin v0.3.0

# 5. Watch it (5-target cross-compile matrix, ~2-3 min):
gh run list --repo BibhabenduMukherjee/HiveMind --limit 1
gh run watch <run-id>

# 6. Verify the ACTUAL conclusion, not just that the watch command exited:
gh run view <run-id> --repo BibhabenduMukherjee/HiveMind \
  --json status,conclusion,jobs --jq '{status, conclusion}'

# 7. Confirm the release landed in the public repo with all 5 assets:
gh release view v0.3.0 --repo BibhabenduMukherjee/HiveMind-releases

# 8. Prove the public install actually works (fresh dir, no cached state):
rm -rf /tmp/install_check && mkdir -p /tmp/install_check
HIVEMIND_INSTALL_DIR=/tmp/install_check bash -c \
  "$(curl -fsSL https://raw.githubusercontent.com/BibhabenduMukherjee/HiveMind-releases/main/install.sh)"
/tmp/install_check/hivemind --version
```

### If a tagged run fails and you need to retry

Deleting and recreating a tag is normal here — don't be afraid of it, this
repo has no other collaborators sharing tags with you.

```sh
git tag -d v0.3.0                              # local
git push origin --delete v0.3.0                # remote
# fix whatever broke, commit, push to main, THEN:
git tag v0.3.0                                 # recreates at current HEAD
git push origin v0.3.0
git rev-parse v0.3.0 HEAD                       # should print the SAME sha twice
```

That last check matters — see the stale-tag error below.

## Common errors

Real ones, hit while building this repo, in the order you're likely to hit
them.

### `error: this manual char comparison can be written more succinctly` (clippy)
Clippy's `manual_pattern_char_comparison` lint. Fix: pass an array of chars
instead of a closure —
`s.trim_end_matches(|c: char| matches!(c, 'a' | 'b'))` becomes
`s.trim_end_matches(['a', 'b'])`.

### `no method named 'with_name' found for struct 'ColumnarMenu'`
A trait method that isn't in scope — the trait itself (`MenuBuilder` for
reedline, but this pattern applies generally) needs an explicit `use`. Rust
usually tells you the exact fix in the error's `help:` line; read it before
guessing.

### `GitHub release failed with status: 403` — `Resource not accessible by personal access token`
Hit twice: once on the auto-generate-release-notes call, once on
create-a-release itself, both with a **fine-grained** PAT that had
`Contents: Read and write`. This is a real GitHub platform gap, not a scope
you got wrong — fine-grained PATs don't reliably support the Releases API.

**Fix:** use a **classic** PAT (github.com/settings/tokens/new, classic —
not fine-grained) with just the `repo` scope, then:
```sh
gh secret set RELEASES_REPO_TOKEN --repo BibhabenduMukherjee/HiveMind
# paste the token when prompted — don't pass it inline as an argument,
# that puts it in your shell history
```

### `bash: tmp: unbound variable` at the very end of `install.sh` (even on success)
A `local` variable set inside a function, referenced by a `trap ... EXIT`
set in that same function, goes out of scope the instant the function
returns — and the EXIT trap fires *after* that return. Under `set -u` that's
fatal, even though everything the script was supposed to do already
succeeded. Fix: make the trap's variable a plain script-global, not
`local`. (Already fixed in `install.sh`; noting the pattern in case it gets
reintroduced.)

### Tag still points at the old commit after you amended/rewrote history
`git push origin v0.3.0` doesn't error if the tag already exists remotely
pointing at the SAME name — but if you rewrote history locally (amend,
rebase, squash) without moving the tag first, you'll silently push the
*old* commit's tag again. Always check before trusting a tag push:
```sh
git rev-parse v0.3.0    # what the tag points at
git rev-parse HEAD      # what you meant to release
```
If they don't match: `git tag -d v0.3.0 && git tag v0.3.0` (recreates at
current HEAD), then push.

### `install.sh` works via `gh`/direct fetch but not via the public `raw.githubusercontent.com` URL
GitHub's raw-content CDN caches for a few minutes after a push. Not a bug —
just wait ~2-3 minutes after pushing to `main` before testing the public
curl command, or check via the API first (`gh api
repos/OWNER/REPO/contents/install.sh --jq '.content' | base64 -d`, which
bypasses the CDN) to confirm the fix is really live before blaming the code.

### `gh run watch` exits 0 but the release didn't actually happen
`gh run watch --exit-status` reports the *workflow's* conclusion, but
piping through other commands or reading a background task's "exit code 0"
notification can make it look like success when only the *watch command
itself* exited cleanly. Always double check the real conclusion:
```sh
gh run view <run-id> --repo BibhabenduMukherjee/HiveMind \
  --json status,conclusion --jq '{status, conclusion}'
```
`"conclusion":"success"` is the only thing that actually means success.

### Cargo.lock conflicts or looks stale after bumping the workspace version
Don't hand-edit `Cargo.lock`. Just rebuild — `cargo build` regenerates the
affected entries automatically:
```sh
cargo build --release -p harness-cli
git add Cargo.lock
```

### `workdir: No such file or directory` when running `hivemind activate`
`--workdir` (default `.`) must exist and be canonicalizable *before*
startup — it's not created for you. `mkdir -p` it first, or don't pass a
`--workdir` that doesn't exist yet.

### A live commit shows the wrong author (e.g. a machine/company identity instead of yours)
Local `git config` on a shared or freshly-provisioned machine may not match
who you actually are. Check before committing anything that matters:
```sh
git log -1 --format="%an <%ae>"
```
Override per-commit without touching global config:
```sh
GIT_AUTHOR_NAME="Bibhabendu Mukherjee" GIT_AUTHOR_EMAIL="mukherjee4004@gmail.com" \
GIT_COMMITTER_NAME="Bibhabendu Mukherjee" GIT_COMMITTER_EMAIL="mukherjee4004@gmail.com" \
git commit -m "..."
```
To fix an already-made commit that hasn't been shared with anyone else yet:
`git commit --amend --reset-author` with the same env vars (`--reset-author`
is required — plain `--amend` keeps the old author).
