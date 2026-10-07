# Agent IDE — environment selection design (v2, 2026-10-04)

Status: accepted; P1 (Python) is being implemented on top of the shared seam in
`crates/agent-ide-core/src/lang/environment.rs`. One environment is current per project root at a
time; an agent switches it with `ide.start environment` and resets it with `auto`.

Basis: the 0.8.1 line (b9a0b7a, includes 5848c5a "nested Python packages, found environments"); file:line references point there and may have shifted. External claims cite primary sources by URL. Items marked not verified (NV) are not relied on. Choices that are Agent IDE policy rather than external practice are labeled **[policy]**.

## 0. Decision in brief

1. **One resolver per language, files only.** It reads files and never runs uv, mise, direnv, nvm or rustup. Every consumer uses it: card, project check, language-server session, ide.test, formatter, syntax probe. Today Python has four resolvers that disagree.
2. **Override through `ide.start`.** The existing tool gets an optional `environment` parameter; no new tool is added. The choice is stored per worktree in the daemon's existing SQLite store, survives the daemon's idle exit, and is cleared with `"auto"`.
   - The **owner** overrides the same way through the agent, or commits the ecosystem's native pin file (pyrightconfig `venv`, rust-toolchain.toml, .nvmrc, …).
   - Launcher-level defaults (toolchains, node) stay where they are.
   - To make an out-of-tree environment selectable, the owner adds its directory to `allowed_roots`.
3. **Precedence.**
   - Python: a Pyright config `venv`/`venvPath` pin comes first, then an explicit selection, then the version request in `.python-version` (warning only), then discovery.
   - Rust: selection, then rustup's own override/pin by proximity, then the launcher default.
   - Node: selection, then pin files **[policy order]**, then the launcher default.
4. **Visibility.** The start card shows the winner, its source, and the other candidates. When the environment changes or goes missing, the plate gets one line with the cause and the next step. Language-server sessions restart and checks rerun on a change.
5. **Access.** The only access rule stays `allowed_roots`: a selected path must lie inside the worktree or an allowed root. No trust store, no new permission layer.
6. **Phases and the separate MCP.** P1 Python with several environments, P2 Rust toolchains, P3 Node. A separate family MCP is **not justified now** (§7).

## 1. Current behavior (file:line)

### 1.1 Python
| Consumer | How it picks the interpreter | Evidence |
|---|---|---|
| Presence rule (card + checks) | A manifest, or a `.venv`/`venv` directory. A worktree with only `.venv-py314` and no manifest is not Python. | crates/agent-ide-lang-python/src/support.rs:162-172 (literal `[".venv","venv"]` at :167) |
| Root discovery | The worktree, plus nested roots that have a manifest and `.py` files. Depth 2, at most 32 dirs, at most 8 roots. | support.rs:99-154 |
| Venv candidates | `.venv`, `venv`, then `.venv*`/`venv*` directories that contain `bin/python`. The suffixed ones come in **read_dir order, unsorted**. | support.rs:174-196 (unsorted at :182-191) |
| Card (`detect`) | The first root's first `venv_directories` entry. **Ignores the pyrightconfig/pyproject `venv` keys.** `.python-version`, uv and poetry are shown as facts only. | support.rs:285-338 (interpreter :293-309; facts :325-338) |
| Project check | Per root: `pyrightconfig.json` venvPath+venv, then `[tool.pyright]` venvPath+venv, then `venv_directories`. A configured pair whose env is missing ends resolution with None. A root without an env is skipped. If every root lacks one, the check reports `EnvMissing`. | checks.rs:390-427 (doc :392-402), loop :251-258, EnvMissing :292-299 |
| Check invocation | `--project <root cfg>` plus `--pythonpath <venv>/bin/python` (not canonicalized). Read roots: the venv root and the base prefix. | checks.rs:145-208 (args :186-195, roots :170-184) |
| Pyright session | `session_interpreter` runs **once, when the session starts**: the worktree root's env, else the first nested root's. It is sent as `python.pythonPath`. The session is reused while the process lives; the env is never rechecked. | backend.rs:171-208 (:206-208), reuse :179-185; checks.rs:429-439; profile.rs:226-244 |
| Session identity | The compatibility key leaves out the interpreter. `ViewGeneration{configuration:1, toolchain:1}` are constants. | profile.rs:188-207; backend.rs:277-282; freshness.rs:41-52 |
| Missing-env summary | **Re-resolved on every answer.** If an env appears mid-session, the summary stops while the server still has no pythonPath, so the raw import flood comes back. | backend.rs:341-343, :441-472 |
| ide.test | `pytest`, or `uv run pytest` when uv.lock exists, taken from the daemon's PATH. **Ignores the interpreter entirely.** `uv run` syncs the project env first. | support.rs:525-570, with_uv :1229-1236; core spawn assistance/tests.rs:644-677 |
| Formatter | Same as ide.test (`black`/`ruff`, plain or via `uv run`). | support.rs:631-664 |
| Syntax probe | The card's interpreter, else `python3`. | support.rs:678-696 |
| Server config change | None: 0 hits for `didChangeConfiguration` in crates/. | grep |

**Wrong precedence comment.** checks.rs:93-96 and :125-128 say `--pythonpath` takes precedence over a config `venv`/`venvPath`. Pyright source says the opposite:
- With `venvPath`+`venv` set and yielding site-packages, those site-packages are used, and pythonPath adds only interpreter roots that are not site-packages.
- If the pinned venv yields nothing, Pyright silently falls back to pythonPath.

Sources: pythonPathUtils.ts https://github.com/microsoft/pyright/blob/main/packages/pyright-internal/src/analyzer/pythonPathUtils.ts ; service.ts ; https://github.com/microsoft/pyright/blob/main/docs/import-resolution.md . This is harmless today only because the resolver reads the config pair first.

**Known failure: `python: environment not found` while `.venv-py314` exists.** On released 0.8.x, `venv_directories` knew only `.venv`/`venv`; suffixed discovery landed in 5848c5a today (support.rs:2243-2291 test). On this branch it still fails in these cases:

- **(a) Config names an absent venv.** pyrightconfig/pyproject names a venv that does not exist. Resolution stops (checks.rs:418-423), yet the card says `venv .venv-py314` (support.rs:293), so the card and the plate contradict each other.
- **(b) The suffixed venv is the only marker.** With nothing but `.venv-py314`, the presence rule fails (support.rs:167): no check runs, and the card says "no manifest".
- **(c) The env lives outside the tree.** Poetry cache, conda, an absolute `UV_PROJECT_ENVIRONMENT`, or pyenv only: never discovered.
- **(d) Host read denies.** A deny plus a link chain fails closed (checks.rs:455-485).
- **(e) Both `.venv` and `.venv-py314` exist.** `.venv` wins silently; the only override is editing the pyright config.

In every case the reply says just `environment not found`, with no cause and no next step (feed/mod.rs:415-426; assistance/problems.rs:855-866).

### 1.2 Rust
| Consumer | Toolchain source | Evidence |
|---|---|---|
| rust-analyzer | The launcher provider: `toolchain` (a rustup selector) plus accepted `cargo`/`rustc` executables with digest and version identity. The env sets `RUSTUP_TOOLCHAIN`, `CARGO` and `RUSTC`; all of them are in the compatibility key. | lang-rust/src/backend.rs:35-52, :88-105, :209-238; profile.rs:212-260, :262-293 |
| Project check | Launcher `project_checks.rust.toolchain_dir`: runs `<dir>/bin/cargo` with PATH set to that toolchain's bin. | lang-rust/src/checks.rs:858-871, :115-182 |
| ide.test | Daemon env `AGENT_IDE_RUST_TOOLCHAIN_DIR`, else `cargo` from PATH. That is the rustup proxy, which follows rustup precedence and may auto-install. | lang-rust/src/support.rs:480-492; tests.rs:652-667 |
| Card | `rust toolchain <channel>` read from rust-toolchain.toml only. This is **the pin file, not what runs**: the analyzer's `RUSTUP_TOOLCHAIN` outranks it, and rustup directory overrides are not read. | support.rs:62-75, :915-930; live card of this session: `environment: rust toolchain 1.98.1 · rust edition 2024` |

So three consumers draw from three different toolchain sources, and the card reports a fourth thing.

### 1.3 TypeScript / JavaScript
| Consumer | Node/TS source | Evidence |
|---|---|---|
| Language server | A release-pinned bundle: Node 24.4.0, typescript-language-server 6.0.0, TS 5.9.3, each with a digest. The session is keyed by the project's input files; a change retires it with `TYPESCRIPT_INPUTS_CHANGED`. | lang-typescript/src/backend.rs:42-94, :131-149, :565-626; profile.rs:196-223 |
| Project check | Launcher `project_checks.typescript.{node,tsc_cli}`. | checks.rs:637-679 |
| Card | `package_manager` (lockfile, then `packageManager`) and `node` (`.nvmrc`, then `.node-version`, then `engines.node`). Display only; volta, `.tool-versions` and mise are not read. | support.rs:60-132 |
| Syntax probe | The accepted `typescript.js`, else the project's `node_modules/typescript`; node is the configured one, else `node` from PATH. | support.rs:549-593 |
| ide.test / format | Package-manager scripts run from the daemon's inherited PATH. | tests.rs:644-677 |

### 1.4 Core: configuration, cache, invalidation
- **Launcher file.** Read only at restart (launcher.rs:511-522). It holds:
  - tool binaries per target (launcher.rs:172-195, :436-462);
  - `allowed_roots` (:475-476; admission :730-807);
  - `project_checks.<lang>` (:326-342);
  - the daemon idle exit, default 300 s (:317-320).

  It holds no per-project environment.
- **`AGENT_IDE_*` overrides.** These cover launcher, attachment, state, home and log, plus `AGENT_IDE_RUST_TOOLCHAIN_DIR` and `AGENT_IDE_GOPLS_PROFILE`. None of them selects a Python or Node environment.
- **Card.** `render_environment` prints `"<lang> <key> <value>"` joined by ` · ` (project/mod.rs:748-767), from `LanguageProject.environment/interpreter` (lang/mod.rs:583-586). The test toolchain seam is `LanguageSupport::test_toolchain` (lang/mod.rs:717-725).
- **Check scheduler.**
  - The skip-unchanged fingerprint is `git ls-files` plus untracked directories, ignored ones included, so an env directory appearing or vanishing moves it (checks/fingerprint.rs:40-101).
  - A selection held only in daemon state does **not** move it.
  - Invalidation precedent: `add_read_denies` bumps the generation, cancels runs, and drops the snapshot and baseline (scheduler.rs:203-238).
  - The cache dir is `<worktree>/<policy_digest>/<language>` (scheduler.rs:1128-1165). `CacheIdentity.toolchain` exists (freshness.rs:282-327).
- **Durable store.** There is no environment table (workspace/durable.rs:29-45). The ide.start activation digest is computed at durable.rs:379-389.

## 2. External practice (cited)

| Topic | Practice | Source |
|---|---|---|
| Pyright env order | Order: config `venvPath`+`venv`, then `python.pythonPath` (LS) / `--pythonpath` (CLI), then `python` on PATH. A pinned venv supplies the site-packages; pythonPath adds only non-site-packages roots; an unusable pin silently falls back to pythonPath. pyrightconfig.json beats `[tool.pyright]`. `executionEnvironments` has no per-root interpreter. `--pythonpath` cannot be combined with `--venvpath`. | https://github.com/microsoft/pyright/blob/main/docs/import-resolution.md ; https://github.com/microsoft/pyright/blob/main/docs/configuration.md ; https://github.com/microsoft/pyright/blob/main/docs/command-line.md ; pythonPathUtils.ts (link above) |
| Pyright switching | `workspace/didChangeConfiguration` updates the options and invalidates the import resolver: full reanalysis, no process restart. Config-file edits are watched and reloaded. | https://github.com/microsoft/pyright/blob/main/packages/pyright-internal/src/languageServerBase.ts ; .../analyzer/service.ts |
| uv | The project env is `.venv`; `UV_PROJECT_ENVIRONMENT` overrides it, relative to the workspace root. Project commands ignore `VIRTUAL_ENV` unless `--active`. `.python-version` is searched upward but stops at the project boundary. `uv run` makes sure the project env is up to date before running (it syncs it); `--no-sync`/`--frozen` exist. `uv sync` removes extraneous packages from the env at `UV_PROJECT_ENVIRONMENT`. | https://docs.astral.sh/uv/concepts/projects/config/ ; https://docs.astral.sh/uv/concepts/python-versions/ ; https://docs.astral.sh/uv/concepts/projects/run/ ; https://docs.astral.sh/uv/reference/cli/ |
| pyenv / poetry / conda | pyenv: `PYENV_VERSION`, then `.python-version` in cwd and parents, then the global version. Poetry: `poetry env use` persists per project; `poetry env info --path`. Conda: activation sets `CONDA_PREFIX`. | https://github.com/pyenv/pyenv/blob/master/COMMANDS.md ; https://python-poetry.org/docs/managing-environments/ ; https://docs.conda.io/projects/conda/en/stable/dev-guide/deep-dives/activation.html |
| Venv identity from files | PEP 405: `pyvenv.cfg` with `home` is enough, and activation is not required. CPython writes `version` (X.Y.Z); uv writes `version_info` and no `version`. The key set written by virtualenv/poetry is NV. | https://peps.python.org/pep-0405/ ; https://github.com/python/cpython/blob/main/Lib/venv/__init__.py ; https://github.com/astral-sh/uv/blob/main/crates/uv-virtualenv/src/virtualenv.rs |
| VS Code Python | Selection is per workspace folder: `updateActiveEnvironmentPath(env, resource)`, whose "configuration target … workspace folder". The Envs extension has `setEnvironment(scope)` and `PythonEnvironment.error` for broken envs. Per the docs page, auto-select prefers a workspace `.venv`/`venv`. Where the selection is persisted: NV. | https://github.com/microsoft/vscode-python/blob/main/pythonExtensionApi/src/main.ts ; https://github.com/microsoft/vscode-python-environments/blob/main/src/types.ts ; https://code.visualstudio.com/docs/python/environments |
| Zed / PyCharm | Zed: toolchain selector; the choice is kept in its workspace DB per project; "Server Info" shows the venv sent to the LS. PyCharm: interpreter per project, shown in the status bar, with an "Invalid environment" warning. | https://zed.dev/docs/languages/python ; https://www.jetbrains.com/help/pycharm/configuring-python-interpreter.html |
| rustup | Order: `cargo +tc`, then `RUSTUP_TOOLCHAIN`, then a directory override or toolchain file (whichever is **nearer** the cwd, walking up), then the default. Directory overrides are stored in `$RUSTUP_HOME/settings.toml` as an `[overrides]` table (canonical dir → toolchain). Toolchains live in `$RUSTUP_HOME/toolchains/<name>/bin`. A missing pinned toolchain is auto-installed (since 1.28.1) unless `RUSTUP_AUTO_INSTALL=0`. | https://rust-lang.github.io/rustup/overrides.html ; https://github.com/rust-lang/rustup/blob/master/src/settings.rs ; https://rust-lang.github.io/rustup/environment-variables.html ; https://rust-lang.github.io/rustup/installation/index.html ; https://github.com/rust-lang/rustup/blob/master/CHANGELOG.md |
| rust-analyzer | It finds cargo/rustc through `CARGO`/`RUSTC` env, then PATH, and gets the sysroot from `rustc --print sysroot`. `cargo.sysroot` changes need a restart. Whether it reloads on a toolchain-file edit is NV, so the design restarts. | https://github.com/rust-lang/rust-analyzer/blob/master/crates/toolchain/src/lib.rs ; https://rust-analyzer.github.io/book/configuration.html |
| Cargo caches | The fingerprint includes a `rustc` hash of the compiler version. A rustc-info cache (`target/.rustc_info.json`) is keyed by the rustc mtime and the rustup toolchain. *Inference:* a target dir shared across toolchains recompiles on each switch (correct, but it thrashes). | https://doc.rust-lang.org/nightly/nightly-rustc/cargo/core/compiler/fingerprint/struct.Fingerprint.html ; https://doc.rust-lang.org/nightly/nightly-rustc/src/cargo/util/rustc.rs.html |
| Node managers | nvm: `.nvmrc` is searched upward and may hold aliases or partial versions (`lts/*`, `node`, `5.9`); nvm is a shell function, not callable from a server; installs go to `$NVM_DIR/versions/node/vX.Y.Z`. fnm: `.node-version`/`.nvmrc`, cwd only by default. Volta: the nearest `package.json` `volta` key; the project is unmaintained. asdf: `ASDF_<T>_VERSION`, then `.tool-versions` searched upward. mise: parent configs are merged and the nearest wins; it reads the idiomatic `.nvmrc`/`.node-version`/package.json only when enabled per tool; installs go to `<data>/installs/<tool>/<ver>`, and fuzzy versions may be symlinks; `mise env`/`mise x` may install. **No tool defines an order across tools.** | https://github.com/nvm-sh/nvm ; https://github.com/nvm-sh/nvm/blob/master/nvm.sh ; https://github.com/Schniz/fnm/blob/master/docs/configuration.md ; https://docs.volta.sh/guide/understanding ; https://github.com/volta-cli/volta ; https://github.com/asdf-vm/asdf/blob/master/docs/manage/versions.md ; https://mise.jdx.dev/configuration.html ; https://mise.jdx.dev/directories.html ; https://mise.jdx.dev/dev-tools/shims.html ; https://mise.jdx.dev/cli/env.html |
| corepack | `packageManager` = `name@version[+hash]` picks the package manager, not Node. Corepack may fetch (`COREPACK_ENABLE_NETWORK=0` prevents it). It is not shipped in Node 25+. | https://github.com/nodejs/corepack#readme |
| TS server | VS Code bundles TS; using the workspace TS (`typescript.tsdk`) needs consent. The Node that runs tsserver is a separate setting (`typescript.tsserver.nodePath`). typescript-language-server reports where its TS came from: workspace, user setting, or bundled. | https://code.visualstudio.com/docs/typescript/typescript-compiling ; https://github.com/microsoft/vscode/blob/main/extensions/typescript-language-features/package.nls.json ; https://github.com/typescript-language-server/typescript-language-server |
| direnv / mise trust | `.envrc` files and mise configs execute repository code, and each tool keeps its own trust store: `direnv allow` records in `~/.local/share/direnv/allow`; mise keeps trusted configs in its state dir. | https://direnv.net/man/direnv.1.html ; https://mise.jdx.dev/cli/trust.html |
| Env MCP for agents | `mise mcp` (experimental) exposes read-only resources (tools, env, config) and the tools `list_commands`/`run_task` (`run_task` executes). It has no environment-selection tool and warns that resource reads evaluate project config, so connect only trusted projects. | https://mise.jdx.dev/mcp.html |

Common practice across these tools:
- Precedence runs explicit, then the project file found walking up, then the user default, then PATH.
- Every mature tool shows what is active and why.
- Persistence splits in two: a shareable file in the repo, and a private user choice kept outside it.
- Language servers take the environment as configuration, never by shell activation.

## 3. Requirements
- **R1** One resolver per language, shared by card, check, session, test, formatter and probe.
- **R2** Deterministic, and per project root (nested roots included).
- **R3** Agents override through the existing surface. The override persists per worktree across sessions and across daemon idle exits, and can be undone.
- **R4** Read files only: never execute managers or `.envrc`, and never let a child process sync, install or fetch.
- **R5** Detect a change or disappearance at use: restart the session, rerun checks, and reply with cause and next step.
- **R6** `allowed_roots` is the only access rule.
- **R7** The core stays language-free (architecture.md "Dependency rule").
- **R8** Replies stay short: one card line per language.

## 4. Options

| | A. Project config file (`.agent-ide.toml`) | B. `ide.start` `environment` param + durable per-worktree choice | B'. New tool `ide.environment` (list/select) | C. Separate family MCP | D. Delegate to uv/mise/direnv/nvm/rustup commands |
|---|---|---|---|---|---|
| **Discovery** | IDE resolver | IDE resolver, with candidates on the card | same as B | the MCP's own resolver | the manager's answer |
| **Override** | edit the file (agent or owner) | one call; `"auto"` resets | one call | call to another server | manager CLI (`uv --python`, `mise use`, `rustup override`) |
| **Storage** | in the repo (committed, or ignored) | daemon SQLite, keyed by worktree incarnation | same as B | another process's state | the manager's own files and stores |
| **Shown** | card | card + plate | card + tool reply | nowhere in the IDE unless synced | nowhere in the IDE unless queried |
| **Session / check invalidation** | A tracked file moves the fingerprint; an ignored file does **not** (fingerprint.rs lists only ignored dirs), so explicit invalidation is needed anyway | The selection is an explicit event: invalidate exactly that (worktree, language) and restart the session (same as TypeScript's `same_project`) | same as B | Needs cross-process notification into IDE sessions, checks and caches; two sources of truth | Ask on every run: slow, and changes in between go unseen |
| **Cost / risk** | Duplicates native pins (pyrightconfig venv, rust-toolchain.toml, .nvmrc); no TOML crate vendored (checks.rs:619-628); dirties git status | +1 optional property; contract snapshot update | +1 tool (the 12th) | New binary, install path and versioning; no second consumer today | Runs repo code (direnv, mise env); installs or touches the network (rustup auto-install, `mise x`, `uv run` sync, corepack); brings its own trust layers (breaks R4/R6); nvm cannot be called at all |
| **Verdict** | **Reject** as a new file; **adopt** the native pin files it would duplicate | **Adopt** | Reject until candidate lists outgrow the card | Reject now (§7) | Reject at runtime; **adopt** their file formats and env-var contracts (`VIRTUAL_ENV`, `RUSTUP_TOOLCHAIN`, `RUSTUP_AUTO_INSTALL`) |

## 5. Recommendation

### 5.1 Precedence, per (worktree, project root, language)

**Python**
1. **Pyright config pin.** `pyrightconfig.json`, then `[tool.pyright]` `venvPath`+`venv` (existing order, checks.rs:418-423). The pin supplies Pyright's site-packages whatever pythonPath says (§2), so a selection cannot override it. A selection is refused: `python: pyrightconfig.json pins venv ".venv" — selection ignored; edit venv there or remove the pin`. If the pinned env is missing, Agent IDE reports `EnvMissing` with a detail. **[policy]** Pyright itself would silently fall back to pythonPath.
2. **Selection** stored by `ide.start environment`. This mirrors `uv --python` / `PYENV_VERSION` outranking files.
3. **`.python-version`.** A version request: shown, and compared with the chosen venv (mismatch warning). It never switches the env. **[policy]**
4. **Discovery.** `.venv`, then `venv`, then suffixed `.venv*`/`venv*` **sorted by name**. **[policy; the first two match VS Code auto-select]**
5. **None.** `EnvMissing` with a next-step detail.

**Rust**: follows rustup precedence.
1. **Selection.** Applied as `RUSTUP_TOOLCHAIN` / explicit cargo/rustc, the equivalent of `+tc`/`RUSTUP_TOOLCHAIN`.
2. **rustup's own resolution.**
   - A directory override from `$RUSTUP_HOME/settings.toml` `[overrides]` versus a `rust-toolchain.toml`/`rust-toolchain` file: **whichever is nearer** the project root, walking up past the worktree exactly as rustup does. The walk reads files only; Rust checks already read ancestor manifests (checks.rs:229-246).
   - The daemon's inherited `RUSTUP_TOOLCHAIN` is **not** honored, because every child env is rebuilt (profile.rs:241-259, checks.rs:135-161), and ide.test will set it explicitly. The card states the source.
3. **Launcher default.** The provider `toolchain` / `toolchain_dir` (operator).
4. **Pinned toolchain not installed.** Never auto-installed (`RUSTUP_AUTO_INSTALL=0`). Reply: `rust 1.99.0 (rust-toolchain.toml) not installed — rustup toolchain install 1.99.0, or ide.start environment {"rust": "launcher"}`.

**Node** (P3)
1. **Selection.**
2. **Pin files, [policy order]:** `package.json` `volta.node`, then `.nvmrc`, then `.node-version`, then `.tool-versions` `nodejs`, then `mise.toml [tools] node` (plain strings only), nearest to the root. No external tool defines this order (§2). When two pins disagree, the card shows both and warns instead of choosing silently. An alias or partial (`lts/*`, `node`, `22`) that matches no installed version is reported `unresolved` (or as the highest installed match, labeled as such); it is never guessed.
3. **Launcher-declared node, else the daemon's PATH.**

`engines.node` is a range: display only.

### 5.2 Contract surface

**`ide.start`** gains one optional property. `activation_id` and `root` are unchanged.
```json
"environment": {
  "type": "object", "maxProperties": 8,
  "propertyNames": {"pattern": "^(python|rust|typescript)(:[^\\u0000]{1,512})?$"},
  "additionalProperties": {"type": "string", "minLength": 1, "maxLength": 1024},
  "description": "Pick the environment per language, optionally per project root (`python:packages/alpha`). Value: a candidate shown on the card (path relative to that root, or absolute), a toolchain/version (`1.99.0`, `22`), `project` (use the pin), `launcher` (operator default), or `auto` to clear. Stored for this worktree until changed."
}
```
- **Not part of the activation digest** (durable.rs:379-389). Repeating `ide.start` with the same `activation_id` and a new `environment` applies the new value and returns the refreshed card. The agent loop is: start, read the candidates, start again with a choice.
- **Validation** uses the existing closed vocabulary:
  - unknown candidate: `error: invalid_detail (environment: python ".venv-py9" not found; candidates .venv, .venv-py314)`
  - a path outside both the worktree and every allowed root: `error: outside_allowed_roots (environment python /opt/envs/x)`, followed by a dedicated recovery text that names the selector and the project venv form (`ide.start` with `environment {"python": ".venv"}`, the directory that holds `bin/python`) instead of the generic project-root text (same `admit_path`, launcher.rs:753)
  - a Pyright pin in force: refused as in §5.1.
- **No new tool**, and nothing new in `ide.context` for P1. Add `kind:"environment"` later only if more than 4 candidates per root become common.

**Card.** One environment line per language replaces the generic facts for these keys (project/mod.rs:748-767). Bounded to 4 candidates plus `+N`.
```
environment: python .venv (3.12.7, discovered) ≠ .python-version 3.14 · also .venv-py314 (3.14.0) — choose: ide.start environment {"python": ".venv-py314"}
environment: python .venv-py314 (3.14.0, selected) · also .venv (3.12.7)
environment: python .venv (pyrightconfig.json pin) · also .venv-py314 (3.14.0)
environment: python missing — no .venv* beside pyproject.toml; create one (uv venv) or ide.start environment {"python": "<path>"}
environment: rust 1.98.1 (rust-toolchain.toml) · analyzer 1.98.1 (launcher)
environment: rust nightly-2026-09-01 (rustup override for ~/projects/x) · analyzer 1.98.1 (launcher)
environment: typescript node 22.11.0 (.nvmrc) ≠ volta.node 20.18.0 · server node 24.4.0, typescript 5.9.3 (bundled; project 5.4.5)
```
- The version comes from `pyvenv.cfg` `version`, or failing that `version_info`. If neither is present the card says `version unknown`; that is not a broken env. An env is `(broken: base interpreter gone)` only when the directory named by `home` is missing (PEP 405).
- The `choose:` hint appears only when there are two or more candidates.

**Plate / feed**
- `EnvMissing` carries a detail, as `Fatal` already does (`unavailable_with_detail`): `python: environment .venv-py314 missing (selected) — recreate it or ide.start environment {"python":"auto"}`.
- One-shot change line, in the style of the `git: HEAD moved` line: `python: environment now .venv-py314 (was .venv; selected) — semantic session restarted`.

**ide.test.** The summary adds ` · env .venv-py314` only when the language has more than one candidate or a selection is in force.

### 5.3 Storage
- A new table in the workspace durable store (crates/agent-ide-core/src/workspace/durable.rs, next to :29-45): `workspace_environment(incarnation INTEGER REFERENCES workspace_worktrees, language TEXT, root TEXT, selector TEXT, PRIMARY KEY(incarnation, language, root))`.
- Keyed per worktree incarnation: it dies with the worktree, and a recreated path does not inherit it.
- Shared by every session on that worktree; actor exclusivity keeps a single writer.
- Survives the 300 s idle exit, and is never written into the repo.
- The shareable, repository-level choice stays in the native pin files. There is no `.agent-ide.toml`.

### 5.4 Change and disappearance (invalidation)
- **Resolve at use.** Stat/read calls only, on every check run and every provider request; the card on every `ide.start`.
- **Identity.** (root, resolved executable path, version, mtime of `pyvenv.cfg` / the pin file / `settings.toml`).
- **Language server.**
  - Pyright's backend `ensure` compares identities and releases and restarts on a mismatch, the same reuse rule as TypeScript's `same_project` (lang-typescript backend.rs:606-613).
  - The identity also goes into `PyrightProfile::compatibility_key` (profile.rs:188-207) and bumps `ViewGeneration.toolchain` (backend.rs:277-282).
  - Restart is chosen over `didChangeConfiguration` because Agent IDE has no client path for that notification. `ponytail:` upgrade to `didChangeConfiguration` (Pyright supports it, §2) only if restarts measurably hurt.
  - Rust and Node restart the same way; rust-analyzer needs a restart for a sysroot change anyway (§2).
- **Checks.**
  - A new `Scheduler::environment_changed(worktree, language)` reuses the body of `add_read_denies` for one language: cancel, drop the snapshot and baseline, trigger urgently (scheduler.rs:203-238).
  - It is called when the selection changes, and when the resolver sees a changed identity at the start of a run.
  - The Rust cache dir adds the toolchain id (`<policy_digest>/<language>/<env-id>`). Per the inference in §2, sharing it would thrash the target dir and `.rustc_info.json`.
  - Python and TypeScript need no cache partition.
- **Disappearance.**
  - A selected or pinned env that is gone gives `EnvMissing` with a detail, never a silent fallback. This keeps the existing "authoritative source" rule (checks.rs:396-402).
  - A discovered env that is gone hands over to the next discovery winner, reported once by the change line.
  - The git fingerprint already moves when env directories appear or vanish (fingerprint.rs:40-44, :67-96).

### 5.5 Seams and code locations (core stays language-free)

**Core: `crates/agent-ide-core/src/lang/mod.rs`**
- Add `EnvCandidate{label, path, version: Option<String>, broken: bool}`.
- Add `EnvSource{Selected, Pin(String), Discovered, Launcher, None}`.
- Add `ResolvedEnv{chosen: Option<EnvCandidate>, source, candidates, warnings: Vec<String>, missing_next_step: Option<String>}`.
- Add `LanguageSupport::environment(&self, worktree, root, selector: Option<&str>) -> Option<ResolvedEnv>`, default `None`.
- Generalize `test_toolchain` (:717-725) to `command_env(&self, env: &ResolvedEnv, program) -> CommandEnv{program, path_prefix, vars}`.
- `LanguageProject.interpreter` (:586) is derived from the resolved env.

**Core: other modules**
- `checks/`: `CheckRequest` carries `selections: Vec<(root, selector)>` for its language; add `Scheduler::environment_changed`.
- `intelligence/server.rs`: `ProviderHost::environment(&binding, language) -> Vec<(root, selector)>`, read from the durable store.
- `assistance/`:
  - `facade.rs`: the schema property;
  - `worker.rs` near `activate` (:2751): validate, store, call `environment_changed`, render;
  - `tests.rs` `spawn_command` (:644-677): apply `CommandEnv`;
  - `problems.rs` and `feed/mod.rs`: render the `EnvMissing` detail;
  - `cargo xtask contract update` to refresh `schemas/tools.json`.
- `project/mod.rs` `render_environment`: the new line.

**Python crate**
- One `resolve(worktree, root, selector)` replaces `resolve_interpreter_with_denies`, `session_interpreter`, the detect interpreter block and the probe fallback (checks.rs:403-439; support.rs:285-324, :678-696).
- Sort the suffixed candidates (support.rs:182-191).
- The presence rule uses `venv_directories` (support.rs:167).
- **Test and format commands, once a venv is resolved:** `<venv>/bin/python -m pytest …` (likewise `-m black` / `-m ruff`, with the env's `bin` first on PATH and `VIRTUAL_ENV=<venv>`).
  - **No `uv run`**: it syncs and mutates the env (§2), which would break R4.
  - Only when no venv is resolved does the current command stay (`pytest` or `uv run pytest` from PATH).
- Fix the precedence comment (checks.rs:93-96, :125-128).

**Rust crate (P2)**
- Resolve the toolchain per §5.1 into `<rustup_home>/toolchains/<name>`. The rustup home is derived as in checks.rs:248-266. Parse `settings.toml` `[overrides]` with a strict `"<path>" = "<toolchain>"` line reader, like the existing TOML line readers (no TOML crate).
- The analyzer, the check and ide.test all use the resolved toolchain.
- Set `RUSTUP_AUTO_INSTALL=0` in every Rust child env (profile.rs:241-259, checks.rs:135-161, the test env).
- For a toolchain that is not the launcher's, measure the cargo/rustc identities at session start and fold them into the compatibility key (profile.rs:262-293).

**TypeScript crate (P3)**
- Resolve the node version per §5.1. Map it to an install by reading confirmed layouts only: nvm `$NVM_DIR/versions/node/vX.Y.Z`; mise `<data>/installs/node/<ver>` (it may be a symlink).
- fnm and volta layouts are NV: verify them in P3. Do not execute `volta which`/`mise which` by default, since that would break R4. P3 may add them as an explicit opt-in.
- The result feeds `command_env` for ide.test, format and lint, and the card.
- The language-server bundle and the check's `node`/`tsc` stay launcher-pinned (accepted-bundle model, backend.rs:47-94).
- The project's `node_modules/typescript` is report-only, mirroring VS Code's consent-gated workspace TS (§2).

### 5.6 Tests

**Python unit tests** (support.rs / checks.rs)
- The precedence table, one case per step of §5.1.
- Deterministic sorted order of suffixed venvs.
- `pyvenv.cfg` `version` vs `version_info`; neither present gives `version unknown`; a missing `home` gives broken.
- A config pin to a missing venv gives `EnvMissing`, and its detail lists the candidates while the card agrees: regression for failure (a).
- A worktree with only `.venv-py314` counts as present: regression for (b).
- One resolver: card == check == session == probe on the same fixtures.
- A `.python-version` mismatch warns but does not switch.
- **No-sync:** with uv.lock and a resolved venv, the test argv is `<venv>/bin/python -m pytest` and contains no `uv`.

**Core**
- `ide.start environment`: parse and validate, unknown language, `auto` clears, outside roots refused, Pyright pin refused, not part of the activation digest.
- Durable round trip; a new incarnation does not inherit the choice.
- `Scheduler::environment_changed` runs even with an unchanged fingerprint and drops the stale snapshot.
- `render_environment` golden lines.
- `tests/contract_snapshot_contract.rs` passes after `contract update`.
- The `language_free` boundary tests stay green.

**Backend**
- Pyright `ensure` restarts when the resolved interpreter changes (fake host).
- The compatibility key differs per interpreter.

**Real provider** (added to the ignored product set run by `cargo xtask check`)
- Fixture: `.venv` lacks package X; `.venv-py314` provides it.
- Before selection, `ide.context` shows the import unresolved; after `ide.start environment`, it resolves and the plate rechecks.
- Pyright precedence: confirm §2 with `pyright --verbose` "Search paths" on a scratch project that sets both a config venv and `--pythonpath`.

**P2**
- Override-vs-file proximity cases, using a fixture `settings.toml`.
- `RUSTUP_AUTO_INSTALL=0` is present in every Rust child env.
- The pin-not-installed reply.
- A separate target dir per toolchain.

**P3**
- Pin resolution against `.nvmrc`/`.node-version`/`.tool-versions`/volta fixtures, including disagreeing pins (warning) and the `lts/*` alias (unresolved).
- PATH prefix in the test env.

**Before release:** the owner's all-tools × all-languages live matrix.

### 5.7 Phased plan

**P1: Python with several environments**
- One resolver, fixing failures (a), (b) and (e).
- Sorted candidates, with versions, on the card.
- `ide.start environment` for python, including `python:<root>`.
- The durable table, session restart and check invalidation.
- Test and format commands run from the selected venv's Python, with no `uv run` sync.
- `EnvMissing` detail with a next step.
- Contract update.
- **Not in P1:** discovery outside the tree (poetry cache, conda, pyenv). Such an env is still selectable by absolute path inside an allowed root; the owner adds the directory to `allowed_roots` if wanted.
- `ponytail:` one Pyright per binding uses the root's (or selected) env for the whole worktree, because Pyright has no per-root interpreter (§2); checks stay per root. Upgrade path: one session per (binding, interpreter) if monorepos with diverging envs appear.

**P2: Rust**
- Rustup-faithful resolution: selection, then override/file by proximity, then launcher.
- A truthful card: the source shown, plus the analyzer's toolchain.
- An installed pin is honored by analyzer, check and test.
- The `rust` selector: `project`, `launcher`, or a toolchain name.
- `RUSTUP_AUTO_INSTALL=0`; a target dir per toolchain; restart on change.

**P3: Node**
- Pin resolution under the policy order, with disagreement warnings and unresolved aliases.
- Install mapping: confirmed layouts first; fnm and volta verified in P3.
- The `typescript` selector applies to ide.test, format and lint.
- The card shows the project's node and TS next to the bundled server's.

**Go:** experimental, unchanged.

## 6. What the agent sees, end to end
1. The `ide.start` card shows: `environment: python .venv (3.12.7, discovered) · also .venv-py314 (3.14.0) — choose: ide.start environment {"python": ".venv-py314"}`.
2. The agent repeats `ide.start` with that choice, and the card now says `(selected)`.
3. The plate reports: `python: environment now .venv-py314 (was .venv; selected) — semantic session restarted`.
4. The next check uses the new env, and `ide.test` runs `.venv-py314/bin/python -m pytest`.
5. If the directory is later deleted, the plate says: `python: environment .venv-py314 missing (selected) — recreate it or ide.start environment {"python":"auto"}`.

## 7. Separate MCP for the agent-* family

**Not justified now.**
- The choice is consumed *inside* Agent IDE's processes, which key sessions, checks and caches on it. A separate server would add a second source of truth and a cross-process invalidation protocol for no gain.
- No other family member consumes environments today.
- The nearest primary-source precedent, `mise mcp`, exposes read-only resources plus a task runner, has no selection tool, and warns that reads evaluate project config (§2).

**Later, only if** a second consumer appears (for example, agent-run must launch delegates in the selected venv):
1. First extract the P1 resolver into a shared Rust library crate.
2. Consider an MCP only if a host without Agent IDE must list and select environments interactively. Even then it would read the same files, honor `allowed_roots`, and keep no trust store of its own.

## 8. Not verified / open
These are not relied on: the location of poetry's `envs.toml`; the semantics of `uv python find`; where VS Code persists its Python selection; whether rust-analyzer reloads on a toolchain-file edit (the design restarts instead); the fnm and volta install layouts (verify in P3); the `pyvenv.cfg` keys written by virtualenv/poetry (the design shows `version unknown`).

The researcher read Pyright's precedence from source with medium-high confidence. A P1 real-provider test confirms it (§5.6).

