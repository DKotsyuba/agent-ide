# Release installation and update

Each Agent IDE release publishes one `aarch64-apple-darwin` archive plus `install.sh`,
`SHA256SUMS`, `release-manifest.json` and `acceptance.json`. macOS arm64 is the only claimed platform; Linux remains explicitly `not_tested`
and has no release artifact.

Publication and installation are separate boundaries: a green publication workflow does not
prove a production cutover, and a local install never publishes to GitHub.

## Install or update

The primary public entry point is the [one-line installer](../README.md#install). Use the
same command for a fresh installation or an update:

```bash
curl -fsSL https://github.com/DKotsyuba/agent-ide/releases/latest/download/install.sh | sh
```

Or use wget:

```bash
wget -qO- https://github.com/DKotsyuba/agent-ide/releases/latest/download/install.sh | sh -s -- --downloader wget
```

The repository is private today: the installer accepts `GITHUB_TOKEN` (sent as a bearer
token) or falls back to `gh release download` when `gh` is authenticated. It verifies the
tarball against the release `SHA256SUMS`, rejects unsafe tar members, and checks the bundle's
own manifest (`metadata.json`, `SHA256SUMS`, `COMPLETE`). It keeps immutable versions under
`~/.agent-ide/standalone/releases/<version>` with a `current` symlink, installs the host
plugin under `~/.local/share/agent-ide/plugin/<version>` with a `plugin/current` symlink, and
places a managed launcher shim at `~/.local/bin/agent-ide`. Repeating the command updates;
the same version is a no-op; an existing version is never overwritten with different bytes.

`install.sh --version X.Y.Z` pins a published release; omitting the version uses the latest
stable GitHub Release. To pin a version or choose directories, download `install.sh` and run:

```bash
sh install.sh --version X.Y.Z --home "$HOME/.agent-ide" \
  --prefix "$HOME/.agent-ide/standalone" --bin-dir "$HOME/.local/bin" \
  --share-dir "$HOME/.local/share/agent-ide"
```

State lives in `~/.agent-ide` (`--home`); the host plugin lives under `--share-dir`
(default `~/.local/share/agent-ide`). `AGENT_IDE_HOME` is a user-home override for tests and relocation: it moves the whole per-user tree (`.agent-ide`, `.config/agent-ide`, `.local`) together. The
installer does not edit host MCP or hook configuration.

Restart agent-run after an install or update: it resolves
`~/.local/share/agent-ide/plugin/current` once, when its service starts, so its runtimes keep
the previous plugin until the restart. Register Codex hooks once with
`agent-ide codex-hooks print`. Afterward, run `agent-ide launcher check
~/.config/agent-ide/launcher.json` and `agent-ide doctor` to verify the installation.

## Codex plugin

Install the binary first, then download and verify the tag-pinned release archive. Extract it and
add its `.agents/plugins/marketplace.json` as a local Codex marketplace, then install `agent-ide`.
The plugin supplies the coding workflow skill; MCP remains machine-local because its launcher
template contains accepted absolute toolchain paths.

Configure the installed executable and launcher template in `~/.codex/config.toml`:

```toml
[mcp_servers.agent-ide]
command = "/absolute/path/to/agent-ide"
args = ["mcp", "--launcher-template", "/absolute/path/to/launcher.json"]
```

To update, run the installer with the new explicit version, download and verify the matching tagged
archive, replace the local marketplace source with its extracted bundle, and restart Codex. Verify
that the configured `command` still names the installer destination.

## Claude Code plugin

Add the tag-pinned marketplace and install its plugin:

```sh
claude plugin marketplace add DKotsyuba/agent-ide@v0.8.1
claude plugin install agent-ide@agent-ide
```

Export the installed absolute binary path before starting Claude Code, and use that identical path
as the MCP `command`:

```sh
export AGENT_IDE_BIN="$HOME/.local/bin/agent-ide"
```

```json
{
  "mcpServers": {
    "agent-ide": {
      "type": "stdio",
      "command": "/absolute/path/to/agent-ide",
      "args": ["mcp", "--claude-launcher-template", "/absolute/path/to/launcher.json"]
    }
  }
}
```

The plugin hook accepts only an absolute executable `AGENT_IDE_BIN` and never searches `PATH`.
Consequently MCP and hooks execute the same atomically replaced file when the configured command
and environment value match. After installing a newer explicit binary version, move the
marketplace source to the matching tag, run `claude plugin marketplace update agent-ide` and
`claude plugin update agent-ide@agent-ide`, then run `/reload-plugins` or start a new session.
Recheck `claude mcp get agent-ide` and `AGENT_IDE_BIN` before using the updated plugin.

## Local install from source

From a clean checkout, build the release binary, package the deterministic bundle, and run the
installer against it:

```bash
release_root="$(mktemp -d)"
version="$(awk -F '"' '/^version = / { print $2; exit }' Cargo.toml)"
cargo build --locked --release --bin agent-ide
scripts/package-release.sh target/release/agent-ide "v$version" "$release_root"
tar -xzf "$release_root/agent-ide-v$version-aarch64-apple-darwin.tar.gz" -C "$release_root"
"$release_root/agent-ide-v$version/agent-ide" self-install --release "$release_root/agent-ide-v$version" --version "$version"
```

`package-release.sh` (a thin wrapper over `cargo xtask package`) verifies that the tag matches the plugin manifest versions and produces
the bundle (`agent-ide`, plugin manifests, marketplace catalogs, hooks, skill, this guide,
`metadata.json`, `SHA256SUMS`, `COMPLETE`) plus the tarball and its `SHA256SUMS`; it writes only
those two files, so the tarball is extracted before `self-install` runs the installer's own code
path on that local bundle. The release workflow verifies the tag against the Cargo version before
packaging. The temporary build directory may be removed after installation. Use a new version for
changed source: the installer never overwrites an existing version with different bytes (source
builds may pass `self-install --replace` — `scripts/install-local.sh` does — to rebuild a
same-version release).

For a development install without a release bundle, `scripts/install-local.sh [--prefix DIR]
[--no-build]` builds this checkout, packages and extracts the sealed bundle, and installs it
through `self-install` into a user prefix (default `$HOME/.local`) so agent-run runtimes, crew
hooks, and Claude skills catalogs can depend on a stable installed path instead of the checkout
itself. The checkout remains the development working copy; nothing about it changes.

What is installed where (all of it by `self-install`; `<home>` is `$AGENT_IDE_HOME` or the real
user home):

- `<home>/.agent-ide/standalone/releases/<version>/` with a `current` symlink — the verified
  sealed bundle, installed immutably.
- `<prefix>/bin/agent-ide` — the managed launcher shim, not a copied binary: `exec
  '<home>/.agent-ide/standalone/current/agent-ide' "$@"`, staged under a temporary name and
  renamed over the previous file.
- `<prefix>/share/agent-ide/plugin/<version>/` — a plugin bundle containing `.claude-plugin/`,
  `.codex-plugin/`, `hooks/`, and `skills/` copied from the checkout. Its `hooks/claude-hook.sh`
  is regenerated to `exec` the installed launcher's absolute path directly, so the installed copy
  no longer needs `AGENT_IDE_BIN`; the checkout's own strict `hooks/claude-hook.sh` is untouched.
  The bundle is staged in a temporary directory and renamed into place. Re-running the installer
  while `plugin/current` already selects the same version is refresh-only: the release and plugin
  directories stay untouched and only the launcher shim is rewritten (`self-install` reports
  `action:"refreshed"`); the script passes `--replace`, so a same-version release with different
  bytes is rebuilt instead of refused.
- `<prefix>/share/agent-ide/plugin/current` — a symlink to the just-installed version directory,
  swapped atomically (staged as `.current-next-<pid>-<unique>`, then renamed over the link).

The script never edits `~/.claude/settings.json`, `~/.agent-run/config.toml`, `crew.toml`, or any
skills catalog — it only prints the exact lines an operator should apply there, pointed at
`<prefix>/share/agent-ide/plugin/current`.

agent-run resolves `<prefix>/share/agent-ide/plugin/current` to its versioned directory once, when
its service starts. After an install, restart the agent-run service when no agents are running;
until then its Claude runtimes keep loading the previous plugin version (seen with 0.3.12 → 0.3.13,
whose old hooks no longer ran under Claude Code 2.1.280).

To roll back: point `<home>/.agent-ide/standalone/current` and
`<prefix>/share/agent-ide/plugin/current` back at the previous version directories
(`<home>/.agent-ide/standalone/releases/<old-version>/` and
`<prefix>/share/agent-ide/plugin/<old-version>/`). A `<prefix>/bin/agent-ide.bak-<old-version>`
backup is written only once — when the installer migrates a previously installed Mach-O binary at
the launcher path to the managed shim (named from the old binary's own `--version` output when it
prints one, else a timestamp); afterwards the launcher is a shim and is simply rewritten.

`agent-ide -v`, `-V`, `--version`, and `version` all print `agent-ide <version>` and exit 0
without touching the daemon, runtime dir, config, or network.

`agent-ide errors [--repo <path>] [--since <minutes>] [--limit <n>] [--summary] [--all]` reads the
local append-only log at `~/.agent-ide/logs/<repository-key>/events.jsonl[.1]` (`--repo` defaults
to the current directory; the repository key is the same one an `ai-r-<id>` runtime directory
uses). It works with no daemon running. Every tool call and daemon/client lifecycle fact is
logged with a closed `level` (`error`/`warn`/`info`); by default only `warn` and `error` are shown,
`--all` also includes `info` (successes, `pending`, lifecycle). Prints one compact `time level
method outcome reason worktree detail` line per event (bounded to `--limit`, default 200, most
recent last), or with `--summary` grouped `(level, method, outcome, reason)` counts instead. See
[`docs/contracts/error-log-v0.3.md`](contracts/error-log-v0.3.md).

## Publishing a release

Every step below is a command; nothing is published from a developer machine.

1. **Prepare locally.** On a clean checkout of `main`, preview the edits, then apply them:

   ```sh
   cargo xtask release prepare X.Y.Z          # preview: lists every edit, writes nothing
   cargo xtask release prepare X.Y.Z --apply  # writes them; refuses a dirty checkout
   ```

   `--apply` edits exactly the workspace version in `Cargo.toml`, the three plugin manifests
   (`.codex-plugin/plugin.json`, `.claude-plugin/plugin.json`, `plugins[0]` of
   `.claude-plugin/marketplace.json`), the `Cargo.lock` entries whose name starts with
   `agent-ide` (a third-party crate may share the version string; `xtask` has its own version),
   and turns `## Unreleased` into `## X.Y.Z — <date>` under a new empty `## Unreleased`. It
   refuses a non-increasing version or an empty Unreleased section, ends with a locked
   `cargo metadata` check, and never commits, tags or pushes. Update version mentions in prose
   (for example the tag-pinned `claude plugin marketplace add` line above) by hand.
2. **Review and merge.** Run `cargo xtask check`, commit, and merge the release PR through the
   normal CI gate.
3. **Rehearse (optional).** Run the Release workflow by hand (`workflow_dispatch`, `dry_run`
   true) on the merge commit or any branch: it runs the whole build job — the gate, package,
   verify, packaged-executable acceptance, evidence validation and the manifest — and uploads the
   payload artifact, but never publishes. A dispatch with `dry_run` false is refused.
4. **Tag.** Create an annotated `vX.Y.Z` tag on the accepted commit and push it. Tags are
   immutable; corrections ship as a new patch version.
5. **Build job** (`contents: read`, plus `actions: read` for the workflow id). It checks the tag
   against the Cargo version and the three plugin manifests (the 4-way gate), prepares the
   accepted toolchains, runs `cargo fetch --locked` and `cargo xtask check` — whose last step is
   the one release build of `target/release/agent-ide` — then:
   - `cargo xtask package target/release/agent-ide vX.Y.Z "$PAYLOAD"` seals the bundle and writes
     the tarball (byte-identical to the former `scripts/package-release.sh`);
   - `cargo xtask package verify` checks it as the former release smoke test did;
   - `scripts/macos-acceptance.sh --route product --payload <tarball>` runs the product gates
     against the executable extracted from that tarball and writes `acceptance.json`, which names
     the tarball and its SHA-256;
   - `scripts/validate-release-evidence.sh "$GITHUB_SHA"` validates the checked-in host evidence;
   - `cargo xtask release manifest "$PAYLOAD"` writes `release-manifest.json` and the aggregate
     `SHA256SUMS`, and `package verify` runs again against both;
   - the payload directory (tarball, `install.sh`, `acceptance.json`, `release-manifest.json`,
     `SHA256SUMS`) is uploaded as the `payload` artifact.
6. **Publish job** (tag pushes only; `contents: write`, `id-token: write`, `attestations: write`,
   environment `release`). It downloads the artifact, builds only `xtask` before any credential
   is in the environment, verifies every hash with `package verify` before any `chmod`, attests
   build provenance for `SHA256SUMS` (skipped while the repository is private: GitHub keeps
   attestations for public repositories only), then runs `xtask release publish`, which:
   - checks the manifest against the run (repository, tag, commit, run id and attempt) and the
     checked-out tag;
   - refuses when any release or draft already exists for the tag — nothing is overwritten;
   - creates a **draft** with all five assets and generated notes;
   - downloads the draft's assets back and verifies them against the manifest and `SHA256SUMS`;
   - publishes the draft, then checks that the visible release carries exactly those assets.
   A failure after the draft exists leaves it unpublished for inspection; delete it by hand
   before re-running the workflow.
7. **Observe.** Bind the published release to the exact tag, commit and run:

   ```sh
   scripts/wait-release.sh --repo DKotsyuba/agent-ide --tag vX.Y.Z --commit FULL_SHA \
     [--run-id N] [--timeout 1800] [--result-file /absolute/new/result.json]
   ```

   (`cargo xtask release wait` with the same flags). It waits for the non-draft release, reads
   its manifest, requires the workflow run of that commit, attempt, workflow id and path to have
   concluded `success`, resolves the tag to the commit, downloads every asset the manifest names
   and verifies sizes and digests. It prints the result and creates `--result-file` exclusively
   (mode 0600); `installed` is always `false` — installing is the separate step above.

### Release assets

| Asset | Content |
|---|---|
| `agent-ide-vX.Y.Z-aarch64-apple-darwin.tar.gz` | the sealed bundle (unchanged format; `install.sh` and `self-install` read it) |
| `install.sh` | the bootstrap installer |
| `SHA256SUMS` | bare-name digests of the tarball, `install.sh`, `acceptance.json` and `release-manifest.json` (never itself) |
| `release-manifest.json` | product, version, tag, full commit, workflow id/path/run/attempt, standard/devkit/baseline, trust profile and name/kind/size/SHA-256 of the tarball, `install.sh` and `acceptance.json` (never itself) |
| `acceptance.json` | the CI product-acceptance evidence of the packaged executable, naming the tarball's SHA-256 |

## Publication gate

The build job repeats formatting, locked workspace tests, Clippy, rustdoc, the four ignored
real-provider tests and the release build (`cargo xtask check`), then the complete product
acceptance route against the packaged executable, on macOS arm64. The accepted release languages
are Rust, Python, and TypeScript/JavaScript; Go and gopls are outside the release scope. No Go
toolchain is installed, the workspace gate skips exactly the three real-gopls toolchain contracts
(`real_gopls_production_context_tracks_exact_observed_bytes`,
`shared_gopls_isolates_divergent_worktrees_and_detaches_one_view`, and
`dropping_live_gopls_owner_closes_its_owned_listener`) by name while running every other
workspace test, and the runner records `go` and `gopls` evidence as `not_tested`. Publication
additionally requires the checked-in product, direct Codex, direct Claude, installed
agent-run-to-Claude, and installed agent-run-to-Codex evidence to name one ancestor candidate
revision. Every host scenario must be `real_pass`, the product scenarios must be `product_pass`,
and all rows must carry the accepted language toolchain versions with `go` and `gopls` pinned to
`not_tested` and closed privacy fields. Missing drivers, `failed`, `not_tested` outside the
go/gopls rows, mixed revisions, partial scenarios, and Linux evidence all block publication.

The toolchains prepared on the runner, and therefore the accepted versions in evidence rows, are
rust-analyzer 1.98.1 (from the pinned Rust toolchain), pyright 1.1.413,
typescript-language-server 6.0.0, TypeScript 5.9.3, and Node 24.4.0
(`.github/workflows/release.yml`). Raising the pinned rust-analyzer also means recording the
lexical outline corpus again with the new build
(`node crates/agent-ide-lang-rust/tests/fixtures/lexical/record.mjs "$AGENT_IDE_RUST_ANALYZER"`,
which writes its version to `VERSION` there) and keeping the corpus test green: the outline Rust
answers from while rust-analyzer loads must equal that build's document symbols. A failed run
leaves no public partial release.
