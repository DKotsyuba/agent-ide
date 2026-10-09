//! Core expansion of an [`EffectRequest`] into the confined [`RunSpec`] the core runs.
//!
//! A recipe is static descriptor data; the module only names it and computes typed parameters.
//! Expansion admits every parameter against the rule the recipe declares for it, using the
//! core's own resolution of each root ([`Admission`]): nothing the module sends becomes a grant
//! unless a rule admits it, an undeclared parameter refuses the whole request, read denies always
//! win, and no list is cut to a fixed count. The core alone decides the working directory, the
//! private cache, the timeout and the capture ceiling.

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Component, Path, PathBuf},
    time::Duration,
};

use super::payload::{
    Arg, ChecksDescription, EffectRecipe, EffectRequest, EnvRule, Param, PathRole, PathRule,
    SlotSource,
};
use crate::{checks::runner::RunSpec, execution::seatbelt::ReadDeny};

/// The core's own resolution of every root a recipe may name, for one run.
#[derive(Clone, Debug)]
pub struct Admission<'a> {
    /// The canonical worktree (the run's working directory).
    pub worktree: &'a Path,
    /// The private cache grant (the only writable root).
    pub cache_dir: &'a Path,
    /// Host read exclusions.
    pub read_denies: &'a [ReadDeny],
    /// The real user home.
    pub home: Option<&'a Path>,
    /// Roots the admitted launcher configuration declares.
    pub launcher_roots: &'a [PathBuf],
    /// Developer directories the core resolved itself.
    pub developer_dirs: &'a [PathBuf],
    /// Resolved executable slots by name (launcher programs, home tools), already measured.
    pub programs: &'a [(String, PathBuf)],
    /// The request's own timeout.
    pub timeout: Duration,
}

/// Why the core refuses to expand a request; nothing runs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Refusal {
    /// No recipe has this id.
    UnknownRecipe(String),
    /// A parameter the recipe does not declare.
    Undeclared(String),
    /// A required parameter is missing.
    Missing(String),
    /// A parameter has the wrong kind for its use.
    WrongKind(String),
    /// A path no rule of its parameter admits.
    OutOfRule(String, PathBuf),
    /// A required path is denied by the host.
    Denied(String, PathBuf),
    /// An executable slot that is not declared or not resolved.
    UnknownSlot(String),
    /// An environment name outside its declared pattern.
    BadEnvName(String),
}

/// Whether `path` is absolute and holds no `.`/`..` component.
fn normal(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::RootDir | Component::Normal(_)))
}

/// Whether `relative` is a relative path without `..` (an ancestor-file entry).
fn plain_relative(relative: &str) -> bool {
    let path = Path::new(relative);
    !relative.is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

/// `path` with its nearest existing ancestor resolved (symlinks followed) and the missing rest
/// re-appended, so a not-yet-created cache path still resolves through its existing parents.
fn resolved(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut rest = Vec::new();
    loop {
        if let Ok(mut canonical) = existing.canonicalize() {
            for part in rest.iter().rev() {
                canonical.push(part);
            }
            return canonical;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

impl Admission<'_> {
    /// Whether `role` admits `path`; with `resolve`, every root is compared in its resolved form
    /// (the check of a path's resolved target).
    fn admits_as(&self, role: PathRole, path: &Path, resolve: bool) -> bool {
        let root = |root: &Path| {
            if resolve {
                resolved(root)
            } else {
                root.to_path_buf()
            }
        };
        match role {
            PathRole::WorktreeRoot => path == root(self.worktree),
            PathRole::Worktree => path.starts_with(root(self.worktree)),
            PathRole::AncestorFile(names) => {
                root(self.worktree).ancestors().skip(1).any(|ancestor| {
                    names
                        .iter()
                        .any(|name| plain_relative(name) && ancestor.join(name) == path)
                })
            }
            PathRole::HomeRelative => self.home.is_some_and(|home| path.starts_with(root(home))),
            PathRole::LauncherRoot => self
                .launcher_roots
                .iter()
                .any(|launcher| path.starts_with(root(launcher))),
            PathRole::LauncherRootAncestor { stop_at } => {
                self.launcher_roots.iter().any(|launcher| {
                    root(launcher).ancestors().any(|ancestor| {
                        ancestor.file_name().is_some_and(|name| name == stop_at)
                            && ancestor.parent() == Some(path)
                    })
                })
            }
            PathRole::Fixed(paths) => paths.iter().any(|fixed| root(Path::new(fixed)) == path),
            PathRole::DeveloperDir => self
                .developer_dirs
                .iter()
                .any(|dir| path.starts_with(root(dir))),
            PathRole::Cache => path.starts_with(root(self.cache_dir)),
        }
    }

    /// Whether `roles` admit `path` both as given and as it resolves (a symlink cannot lead out
    /// of its admitted roots); the given spelling is what the run uses, so an interpreter keeps
    /// its invocation path.
    fn admits(&self, roles: &[PathRole], path: &Path) -> bool {
        self.admits_given(roles, path) && self.admits_target(roles, path)
    }

    /// Whether `roles` admit `path` as given.
    fn admits_given(&self, roles: &[PathRole], path: &Path) -> bool {
        normal(path) && roles.iter().any(|role| self.admits_as(*role, path, false))
    }

    /// Whether `roles` admit what `path` resolves to.
    fn admits_target(&self, roles: &[PathRole], path: &Path) -> bool {
        let target = resolved(path);
        roles
            .iter()
            .any(|role| self.admits_as(*role, &target, true))
    }

    /// Whether the host denies reading `path`, as given or as it resolves.
    fn denied(&self, path: &Path) -> bool {
        let target = resolved(path);
        self.read_denies
            .iter()
            .any(|deny| deny.matches(path) || deny.matches(&target))
    }
}

/// Admits the values of one path rule; `Ok(None)` when the parameter is absent.
fn admit_rule(
    rule: &PathRule,
    param: Option<&Param>,
    admission: &Admission<'_>,
) -> Result<Option<Vec<PathBuf>>, Refusal> {
    let values: Vec<&PathBuf> = match param {
        None => return Ok(None),
        Some(Param::Path(path)) => vec![path],
        Some(Param::Paths(paths)) => paths.iter().collect(),
        Some(_) => return Err(Refusal::WrongKind(rule.param.to_owned())),
    };
    let mut admitted = Vec::with_capacity(values.len());
    for path in values {
        if !admission.admits_given(rule.roles, path) {
            return Err(Refusal::OutOfRule(rule.param.to_owned(), path.clone()));
        }
        // An optional file is read exactly when today's in-process check reads it, and never
        // when a deny matches it as given or as resolved; a required path refuses instead.
        if rule.existing_only {
            if !admission.denied(path) && optional_file(path, admission.read_denies) {
                admitted.push(path.clone());
            }
            continue;
        }
        if !admission.admits_target(rule.roles, path) {
            return Err(Refusal::OutOfRule(rule.param.to_owned(), path.clone()));
        }
        if admission.denied(path) {
            return Err(Refusal::Denied(rule.param.to_owned(), path.clone()));
        }
        admitted.push(path.clone());
    }
    Ok(Some(admitted))
}

/// Today's predicate for an optional auxiliary file (an ancestor manifest, a tool's ignore
/// file): with no host denies, a regular file, a symlink to one included; with any deny, a
/// regular file that is not itself a symlink and that no deny matches as given.
fn optional_file(path: &Path, denies: &[ReadDeny]) -> bool {
    if denies.is_empty() {
        return path.is_file();
    }
    !denies.iter().any(|deny| deny.matches(path))
        && std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// Whether `name` is `prefix` + an uppercase identifier + `suffix`.
fn pattern_name(name: &str, prefix: &str, suffix: &str) -> bool {
    name.strip_prefix(prefix)
        .and_then(|rest| rest.strip_suffix(suffix))
        .is_some_and(|middle| {
            !middle.is_empty()
                && middle
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        })
}

/// What a language's `describe` answer for its launcher section contributes to an [`Admission`]:
/// its named programs as executable slots, its launcher roots and its developer-directory
/// overrides. The core takes these from the answer and never parses a section itself; it may
/// append its own platform developer directories.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Described {
    /// Executable slots by name.
    pub programs: Vec<(String, PathBuf)>,
    /// [`PathRole::LauncherRoot`] roots.
    pub launcher_roots: Vec<PathBuf>,
    /// [`PathRole::DeveloperDir`] directories.
    pub developer_dirs: Vec<PathBuf>,
}

impl Described {
    /// The admission inputs of `description`.
    pub fn new(description: &ChecksDescription) -> Self {
        Self {
            programs: description
                .programs
                .iter()
                .map(|program| (program.name.clone(), program.path.clone()))
                .collect(),
            launcher_roots: description.launcher_roots.clone(),
            developer_dirs: description.developer_dirs.clone(),
        }
    }

    /// The admission of one run in `worktree` with the private `cache_dir`, the host
    /// `read_denies`, the real `home` and the request's `timeout`.
    pub fn admission<'a>(
        &'a self,
        worktree: &'a Path,
        cache_dir: &'a Path,
        read_denies: &'a [ReadDeny],
        home: Option<&'a Path>,
        timeout: Duration,
    ) -> Admission<'a> {
        Admission {
            worktree,
            cache_dir,
            read_denies,
            home,
            launcher_roots: &self.launcher_roots,
            developer_dirs: &self.developer_dirs,
            programs: &self.programs,
            timeout,
        }
    }
}

/// Expands `effect` with its recipe from `recipes` under `admission`.
pub fn expand(
    recipes: &[EffectRecipe],
    effect: &EffectRequest,
    admission: &Admission<'_>,
) -> Result<RunSpec, Refusal> {
    let recipe = recipes
        .iter()
        .find(|recipe| recipe.id == effect.recipe)
        .ok_or_else(|| Refusal::UnknownRecipe(effect.recipe.clone()))?;
    let mut declared: Vec<&str> = vec![recipe.program];
    for arg in recipe.args {
        match arg {
            Arg::Param(name) | Arg::Each(name) | Arg::Joined(_, name) => declared.push(name),
            Arg::Literal(_) => {}
        }
    }
    for rule in recipe.env {
        match rule {
            EnvRule::Param { param, .. }
            | EnvRule::Joined { param, .. }
            | EnvRule::Pattern { param, .. }
            | EnvRule::SearchPath { param, .. } => declared.push(param),
            EnvRule::Literal { .. } | EnvRule::Home { .. } => {}
        }
    }
    declared.extend(recipe.paths.iter().map(|rule| rule.param));
    if let Some(undeclared) = effect
        .params
        .keys()
        .find(|name| !declared.contains(&name.as_str()))
    {
        return Err(Refusal::Undeclared(undeclared.clone()));
    }
    let mut paths: BTreeMap<&str, Vec<PathBuf>> = BTreeMap::new();
    let mut read_roots = Vec::new();
    for rule in recipe.paths {
        if let Some(admitted) = admit_rule(rule, effect.params.get(rule.param), admission)? {
            if rule.read_root {
                read_roots.extend(admitted.iter().cloned());
            }
            paths.insert(rule.param, admitted);
        }
    }
    let single = |name: &str| -> Result<Option<String>, Refusal> {
        match effect.params.get(name) {
            None => Ok(None),
            Some(Param::Token(token)) => Ok(Some(token.clone())),
            Some(Param::Scalar(value)) => Ok(Some(value.to_string())),
            Some(Param::Path(_)) => match paths.get(name).map(Vec::as_slice) {
                Some([path]) => Ok(Some(path.display().to_string())),
                Some([]) => Ok(None),
                _ => Err(Refusal::OutOfRule(name.to_owned(), PathBuf::new())),
            },
            Some(_) => Err(Refusal::WrongKind(name.to_owned())),
        }
    };
    let program = match effect.params.get(recipe.program) {
        Some(Param::Executable(slot)) => {
            let declared_slot = recipe
                .executables
                .iter()
                .find(|declared| declared.name == slot)
                .ok_or_else(|| Refusal::UnknownSlot(slot.clone()))?;
            let name = match declared_slot.source {
                SlotSource::Launcher(name) | SlotSource::HomeTool(name) => name,
            };
            let program = admission
                .programs
                .iter()
                .find(|(resolved, _)| resolved == name)
                .map(|(_, path)| path.clone())
                .ok_or_else(|| Refusal::UnknownSlot(slot.clone()))?;
            if admission.denied(&program) {
                return Err(Refusal::Denied(recipe.program.to_owned(), program));
            }
            program
        }
        Some(Param::Path(_)) => single(recipe.program)?
            .map(PathBuf::from)
            .ok_or_else(|| Refusal::Missing(recipe.program.to_owned()))?,
        Some(_) => return Err(Refusal::WrongKind(recipe.program.to_owned())),
        None => return Err(Refusal::Missing(recipe.program.to_owned())),
    };
    let mut args: Vec<OsString> = Vec::new();
    for arg in recipe.args {
        match arg {
            Arg::Literal(token) => args.push(token.into()),
            Arg::Param(name) => args.push(
                single(name)?
                    .ok_or_else(|| Refusal::Missing((*name).to_owned()))?
                    .into(),
            ),
            Arg::Each(name) => args.extend(
                paths
                    .get(name)
                    .into_iter()
                    .flatten()
                    .map(|path| path.clone().into_os_string()),
            ),
            Arg::Joined(prefix, name) => args.push(
                format!(
                    "{prefix}{}",
                    single(name)?.ok_or_else(|| Refusal::Missing((*name).to_owned()))?
                )
                .into(),
            ),
        }
    }
    let mut env = Vec::new();
    for rule in recipe.env {
        match rule {
            EnvRule::Literal { name, value } => env.push(((*name).to_owned(), (*value).to_owned())),
            EnvRule::Param {
                name,
                param,
                optional,
            } => match single(param)? {
                Some(value) => env.push(((*name).to_owned(), value)),
                None if *optional => {}
                None => return Err(Refusal::Missing((*param).to_owned())),
            },
            EnvRule::Joined {
                name,
                prefix,
                param,
            } => {
                if let Some(value) = single(param)? {
                    env.push(((*name).to_owned(), format!("{prefix}{value}")));
                }
            }
            EnvRule::Pattern {
                prefix,
                suffix,
                param,
                roles,
            } => match effect.params.get(*param) {
                None => {}
                Some(Param::Env(entries)) => {
                    for (name, value) in entries {
                        let path = Path::new(value);
                        if !pattern_name(name, prefix, suffix) {
                            return Err(Refusal::BadEnvName(name.clone()));
                        }
                        if !admission.admits(roles, path) {
                            return Err(Refusal::OutOfRule(
                                (*param).to_owned(),
                                path.to_path_buf(),
                            ));
                        }
                        if admission.denied(path) {
                            return Err(Refusal::Denied((*param).to_owned(), path.to_path_buf()));
                        }
                        env.push((name.clone(), value.clone()));
                    }
                }
                Some(_) => return Err(Refusal::WrongKind((*param).to_owned())),
            },
            EnvRule::SearchPath { name, param, fixed } => {
                let entries: Vec<String> = paths
                    .get(param)
                    .into_iter()
                    .flatten()
                    .map(|path| path.display().to_string())
                    .chain(fixed.iter().map(|entry| (*entry).to_owned()))
                    .collect();
                env.push(((*name).to_owned(), entries.join(":")));
            }
            EnvRule::Home { name } => {
                let home = admission
                    .home
                    .ok_or_else(|| Refusal::Missing("home".to_owned()))?;
                env.push(((*name).to_owned(), home.display().to_string()));
            }
        }
    }
    Ok(RunSpec {
        program,
        args,
        cwd: admission.worktree.to_path_buf(),
        env,
        read_roots,
        write_roots: vec![admission.cache_dir.to_path_buf()],
        read_denies: admission.read_denies.to_vec(),
        timeout: admission
            .timeout
            .min(Duration::from_millis(recipe.timeout_ceiling_ms)),
        max_output_bytes: recipe.capture_bytes as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::payload::{ExecutableSlot, RunClass, Stdin};

    /// Ancestor files a nested project check reads.
    const ANCESTOR_FILES: &[&str] = &["Cargo.toml", ".cargo/config.toml", ".cargo/config"];

    /// The project-check recipe shaped like today's compiler check (`cargo check` with the
    /// pinned toolchain, private target and the linker bypass), as descriptor data.
    const CHECK: EffectRecipe = EffectRecipe {
        id: "check",
        program: "cargo",
        args: &[
            Arg::Literal("check"),
            Arg::Literal("--workspace"),
            Arg::Literal("--all-targets"),
            Arg::Literal("--message-format=json"),
            Arg::Literal("--offline"),
            Arg::Literal("--keep-going"),
            Arg::Literal("--locked"),
        ],
        env: &[
            EnvRule::SearchPath {
                name: "PATH",
                param: "toolchain_bin",
                fixed: &["/usr/bin", "/bin"],
            },
            EnvRule::Home { name: "HOME" },
            EnvRule::Param {
                name: "CARGO_HOME",
                param: "cargo_home",
                optional: false,
            },
            EnvRule::Param {
                name: "TMPDIR",
                param: "tmp",
                optional: false,
            },
            EnvRule::Param {
                name: "CARGO_TARGET_DIR",
                param: "target",
                optional: false,
            },
            EnvRule::Literal {
                name: "CARGO_NET_OFFLINE",
                value: "true",
            },
            EnvRule::Pattern {
                prefix: "CARGO_TARGET_",
                suffix: "_LINKER",
                param: "linker",
                roles: &[PathRole::DeveloperDir],
            },
            EnvRule::Joined {
                name: "RUSTFLAGS",
                prefix: "-Clinker=",
                param: "linker_flag",
            },
            EnvRule::Param {
                name: "CC",
                param: "cc",
                optional: true,
            },
            EnvRule::Param {
                name: "CXX",
                param: "cxx",
                optional: true,
            },
            EnvRule::Param {
                name: "AR",
                param: "ar",
                optional: true,
            },
            EnvRule::Param {
                name: "RANLIB",
                param: "ranlib",
                optional: true,
            },
            EnvRule::Param {
                name: "SDKROOT",
                param: "sdkroot",
                optional: true,
            },
        ],
        paths: &[
            PathRule {
                param: "worktree",
                roles: &[PathRole::WorktreeRoot],
                existing_only: false,
                read_root: true,
            },
            PathRule {
                param: "toolchain",
                roles: &[PathRole::LauncherRoot],
                existing_only: false,
                read_root: true,
            },
            PathRule {
                param: "cargo_home",
                roles: &[PathRole::LauncherRoot, PathRole::HomeRelative],
                existing_only: false,
                read_root: true,
            },
            PathRule {
                param: "rustup_home",
                roles: &[
                    PathRole::LauncherRootAncestor {
                        stop_at: "toolchains",
                    },
                    PathRole::HomeRelative,
                ],
                existing_only: false,
                read_root: true,
            },
            PathRule {
                param: "etc",
                roles: &[PathRole::Fixed(&["/private/etc"])],
                existing_only: false,
                read_root: true,
            },
            // The module names exactly the developer roots that exist, as today's resolver does.
            PathRule {
                param: "developer",
                roles: &[PathRole::DeveloperDir],
                existing_only: false,
                read_root: true,
            },
            PathRule {
                param: "ancestors",
                roles: &[PathRole::AncestorFile(ANCESTOR_FILES)],
                existing_only: true,
                read_root: true,
            },
            PathRule {
                param: "git_exclude",
                roles: &[PathRole::HomeRelative],
                existing_only: true,
                read_root: true,
            },
            PathRule {
                param: "toolchain_bin",
                roles: &[PathRole::LauncherRoot],
                existing_only: false,
                read_root: false,
            },
            PathRule {
                param: "tmp",
                roles: &[PathRole::Cache],
                existing_only: false,
                read_root: false,
            },
            PathRule {
                param: "target",
                roles: &[PathRole::Cache],
                existing_only: false,
                read_root: false,
            },
            PathRule {
                param: "linker_flag",
                roles: &[PathRole::DeveloperDir],
                existing_only: false,
                read_root: false,
            },
            PathRule {
                param: "cc",
                roles: &[PathRole::DeveloperDir],
                existing_only: false,
                read_root: false,
            },
            PathRule {
                param: "cxx",
                roles: &[PathRole::DeveloperDir],
                existing_only: false,
                read_root: false,
            },
            PathRule {
                param: "ar",
                roles: &[PathRole::DeveloperDir],
                existing_only: false,
                read_root: false,
            },
            PathRule {
                param: "ranlib",
                roles: &[PathRole::DeveloperDir],
                existing_only: false,
                read_root: false,
            },
            PathRule {
                param: "sdkroot",
                roles: &[PathRole::DeveloperDir],
                existing_only: false,
                read_root: false,
            },
        ],
        executables: &[ExecutableSlot {
            name: "cargo",
            source: SlotSource::Launcher("cargo"),
        }],
        stdin: Stdin::Null,
        class: RunClass::Background,
        timeout_ceiling_ms: 900_000,
        capture_bytes: 64 << 20,
    };

    /// A scratch layout: an outer project holding the worktree, a home with a toolchain and a git
    /// exclude file, and a developer directory.
    struct Layout {
        /// Removed on drop.
        base: PathBuf,
        /// Canonical worktree.
        worktree: PathBuf,
        /// Home.
        home: PathBuf,
        /// Toolchain directory.
        toolchain: PathBuf,
        /// Developer directory.
        developer: PathBuf,
        /// Private cache.
        cache: PathBuf,
    }

    impl Layout {
        /// Creates the layout on disk.
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("recipe-{tag}-{}", std::process::id()));
            let worktree = base.join("outer/ws");
            let home = base.join("home");
            let toolchain = home.join(".rustup/toolchains/stable-aarch64-apple-darwin");
            let developer = base.join("dev");
            for dir in [
                &worktree,
                &toolchain.join("bin"),
                &developer,
                &home.join(".config/git"),
            ] {
                std::fs::create_dir_all(dir).unwrap();
            }
            std::fs::write(base.join("outer/Cargo.toml"), "").unwrap();
            std::fs::write(home.join(".config/git/ignore"), "").unwrap();
            Self {
                cache: base.join("cache"),
                base,
                worktree,
                home,
                toolchain,
                developer,
            }
        }

        /// The module's parameters for this layout.
        fn effect(&self) -> EffectRequest {
            let ancestors = self
                .worktree
                .ancestors()
                .skip(1)
                .flat_map(|ancestor| ANCESTOR_FILES.iter().map(move |name| ancestor.join(name)))
                .collect();
            let clang = self.developer.join("usr/bin/clang");
            EffectRequest {
                recipe: "check".into(),
                params: BTreeMap::from([
                    ("cargo".into(), Param::Executable("cargo".into())),
                    ("worktree".into(), Param::Path(self.worktree.clone())),
                    ("toolchain".into(), Param::Path(self.toolchain.clone())),
                    (
                        "toolchain_bin".into(),
                        Param::Paths(vec![self.toolchain.join("bin")]),
                    ),
                    ("cargo_home".into(), Param::Path(self.home.join(".cargo"))),
                    ("rustup_home".into(), Param::Path(self.home.join(".rustup"))),
                    ("etc".into(), Param::Path("/private/etc".into())),
                    (
                        "developer".into(),
                        Param::Paths(vec![self.developer.clone()]),
                    ),
                    ("ancestors".into(), Param::Paths(ancestors)),
                    (
                        "git_exclude".into(),
                        Param::Path(self.home.join(".config/git/ignore")),
                    ),
                    ("tmp".into(), Param::Path(self.cache.join("tmp"))),
                    ("target".into(), Param::Path(self.cache.join("target"))),
                    (
                        "linker".into(),
                        Param::Env(BTreeMap::from([(
                            "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER".into(),
                            clang.display().to_string(),
                        )])),
                    ),
                    ("cc".into(), Param::Path(clang)),
                ]),
            }
        }

        /// The core's admission for this layout.
        fn admission<'a>(
            &'a self,
            programs: &'a [(String, PathBuf)],
            roots: &'a [PathBuf],
            dev: &'a [PathBuf],
        ) -> Admission<'a> {
            Admission {
                worktree: &self.worktree,
                cache_dir: &self.cache,
                read_denies: &[],
                home: Some(&self.home),
                launcher_roots: roots,
                developer_dirs: dev,
                programs,
                timeout: Duration::from_secs(1200),
            }
        }
    }

    impl Drop for Layout {
        /// Removes the scratch tree.
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    /// The expansion equals the hand-built run specification today's in-process check builds for
    /// the same inputs: program, arguments, environment order, read roots (existing ancestor
    /// files only), private cache, timeout ceiling and capture.
    /// The admission comes from the module's `describe` answer for the section (Describe →
    /// Admission → expansion): named programs, launcher roots, developer directories.
    #[test]
    fn expansion_equals_the_in_process_specification() {
        let layout = Layout::new("equal");
        let described = Described::new(&ChecksDescription {
            valid: true,
            programs: vec![crate::modules::payload::NamedProgram {
                name: "cargo".into(),
                path: layout.toolchain.join("bin/cargo"),
                interpreter: None,
            }],
            launcher_roots: vec![layout.toolchain.clone()],
            developer_dirs: vec![layout.developer.clone()],
        });
        let spec = expand(
            &[CHECK],
            &layout.effect(),
            &described.admission(
                &layout.worktree,
                &layout.cache,
                &[],
                Some(&layout.home),
                Duration::from_secs(1200),
            ),
        )
        .unwrap();
        let clang = layout.developer.join("usr/bin/clang").display().to_string();
        let expected = RunSpec {
            program: layout.toolchain.join("bin/cargo"),
            args: [
                "check",
                "--workspace",
                "--all-targets",
                "--message-format=json",
                "--offline",
                "--keep-going",
                "--locked",
            ]
            .into_iter()
            .map(OsString::from)
            .collect(),
            cwd: layout.worktree.clone(),
            env: vec![
                (
                    "PATH".into(),
                    format!("{}/bin:/usr/bin:/bin", layout.toolchain.display()),
                ),
                ("HOME".into(), layout.home.display().to_string()),
                (
                    "CARGO_HOME".into(),
                    layout.home.join(".cargo").display().to_string(),
                ),
                (
                    "TMPDIR".into(),
                    layout.cache.join("tmp").display().to_string(),
                ),
                (
                    "CARGO_TARGET_DIR".into(),
                    layout.cache.join("target").display().to_string(),
                ),
                ("CARGO_NET_OFFLINE".into(), "true".into()),
                (
                    "CARGO_TARGET_AARCH64_APPLE_DARWIN_LINKER".into(),
                    clang.clone(),
                ),
                ("CC".into(), clang),
            ],
            read_roots: vec![
                layout.worktree.clone(),
                layout.toolchain.clone(),
                layout.home.join(".cargo"),
                layout.home.join(".rustup"),
                PathBuf::from("/private/etc"),
                layout.developer.clone(),
                layout.base.join("outer/Cargo.toml"),
                layout.home.join(".config/git/ignore"),
            ],
            write_roots: vec![layout.cache.clone()],
            read_denies: Vec::new(),
            timeout: Duration::from_secs(900),
            max_output_bytes: 64 << 20,
        };
        assert_eq!(spec, expected);
    }

    /// A root outside its rule, an undeclared parameter, an undeclared slot and a linker name
    /// outside its pattern are each refused; nothing runs.
    #[test]
    fn out_of_rule_requests_are_refused() {
        let layout = Layout::new("refuse");
        let programs = [("cargo".to_owned(), layout.toolchain.join("bin/cargo"))];
        let roots = [layout.toolchain.clone()];
        let dev = [layout.developer.clone()];
        let admission = layout.admission(&programs, &roots, &dev);
        let mut outside = layout.effect();
        outside
            .params
            .insert("cargo_home".into(), Param::Path("/etc/ssh".into()));
        assert!(matches!(
            expand(&[CHECK], &outside, &admission),
            Err(Refusal::OutOfRule(name, _)) if name == "cargo_home"
        ));
        let mut smuggled = layout.effect();
        smuggled
            .params
            .insert("extra_root".into(), Param::Path(layout.worktree.clone()));
        assert_eq!(
            expand(&[CHECK], &smuggled, &admission),
            Err(Refusal::Undeclared("extra_root".into()))
        );
        let mut slot = layout.effect();
        slot.params
            .insert("cargo".into(), Param::Executable("sh".into()));
        assert_eq!(
            expand(&[CHECK], &slot, &admission),
            Err(Refusal::UnknownSlot("sh".into()))
        );
        let mut env = layout.effect();
        env.params.insert(
            "linker".into(),
            Param::Env(BTreeMap::from([(
                "LD_PRELOAD".into(),
                layout.developer.join("x").display().to_string(),
            )])),
        );
        assert_eq!(
            expand(&[CHECK], &env, &admission),
            Err(Refusal::BadEnvName("LD_PRELOAD".into()))
        );
        let mut traversal = layout.effect();
        traversal.params.insert(
            "ancestors".into(),
            Param::Paths(vec![layout.worktree.join("../../outside/Cargo.toml")]),
        );
        assert!(matches!(
            expand(&[CHECK], &traversal, &admission),
            Err(Refusal::OutOfRule(name, _)) if name == "ancestors"
        ));
    }

    /// A required path through a symlink inside its admitted root that leads outside it is
    /// refused; an optional ancestor file follows today's rule (a symlink is read with no deny,
    /// not read with any deny); a read deny on the given or resolved path wins (environment
    /// values included). A cache path that does not exist yet is still admitted through its
    /// existing parents.
    #[test]
    fn symlinks_cannot_leave_their_roots() {
        let layout = Layout::new("links");
        let outside = layout.base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("Cargo.toml"), "").unwrap();
        std::os::unix::fs::symlink(&outside, layout.home.join(".cargo")).unwrap();
        std::fs::create_dir_all(layout.base.join("outer/.cargo")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("Cargo.toml"),
            layout.base.join("outer/.cargo/config.toml"),
        )
        .unwrap();
        std::fs::create_dir_all(layout.developer.join("usr/bin")).unwrap();
        std::os::unix::fs::symlink(&outside, layout.developer.join("usr/bin/clang")).unwrap();
        let programs = [("cargo".to_owned(), layout.toolchain.join("bin/cargo"))];
        let roots = [layout.toolchain.clone()];
        let dev = [layout.developer.clone()];
        let admission = layout.admission(&programs, &roots, &dev);
        assert!(
            matches!(
                expand(&[CHECK], &layout.effect(), &admission),
                Err(Refusal::OutOfRule(name, _)) if name == "cargo_home"
            ),
            "a home symlink leading outside the home is refused"
        );
        let mut inside = layout.effect();
        inside.params.insert(
            "cargo_home".into(),
            Param::Path(layout.toolchain.join("cargo-home")),
        );
        inside.params.remove("cc");
        inside.params.remove("linker");
        let spec = expand(&[CHECK], &inside, &admission).unwrap();
        assert!(
            spec.read_roots
                .contains(&layout.base.join("outer/.cargo/config.toml")),
            "with no deny a symlinked ancestor file is read, exactly as today"
        );
        let unrelated = [ReadDeny::Path(layout.base.join("unrelated"))];
        let spec = expand(
            &[CHECK],
            &inside,
            &Admission {
                read_denies: &unrelated,
                ..admission.clone()
            },
        )
        .unwrap();
        assert!(
            !spec
                .read_roots
                .contains(&layout.base.join("outer/.cargo/config.toml")),
            "with any deny a symlinked ancestor file is not read, exactly as today"
        );
        let mut env = inside.clone();
        env.params.insert(
            "linker".into(),
            Param::Env(BTreeMap::from([(
                "CARGO_TARGET_X_LINKER".into(),
                layout.developer.join("usr/bin/clang").display().to_string(),
            )])),
        );
        assert!(
            matches!(
                expand(&[CHECK], &env, &admission),
                Err(Refusal::OutOfRule(name, _)) if name == "linker"
            ),
            "an environment value resolving outside its root is refused"
        );
        let denies = [ReadDeny::Path(layout.toolchain.join("cargo-home"))];
        let denied = Admission {
            read_denies: &denies,
            ..admission.clone()
        };
        assert!(
            matches!(
                expand(&[CHECK], &inside, &denied),
                Err(Refusal::Denied(name, _)) if name == "cargo_home"
            ),
            "a read deny wins"
        );
    }

    /// An optional ancestor file is a file: a directory named like one is not read. A named
    /// program the host denies is refused before any specification exists.
    #[test]
    fn optional_files_are_files_and_denied_programs_refuse() {
        let layout = Layout::new("files");
        std::fs::create_dir_all(layout.base.join("outer/.cargo/config.toml")).unwrap();
        let programs = [("cargo".to_owned(), layout.toolchain.join("bin/cargo"))];
        let roots = [layout.toolchain.clone()];
        let dev = [layout.developer.clone()];
        let admission = layout.admission(&programs, &roots, &dev);
        let spec = expand(&[CHECK], &layout.effect(), &admission).unwrap();
        assert!(
            !spec
                .read_roots
                .contains(&layout.base.join("outer/.cargo/config.toml")),
            "a directory named like an ancestor file is not read"
        );
        assert!(
            spec.read_roots
                .contains(&layout.base.join("outer/Cargo.toml"))
        );
        let denies = [ReadDeny::Path(layout.toolchain.join("bin/cargo"))];
        assert!(
            matches!(
                expand(
                    &[CHECK],
                    &layout.effect(),
                    &Admission {
                        read_denies: &denies,
                        ..admission.clone()
                    }
                ),
                Err(Refusal::Denied(name, _)) if name == "cargo"
            ),
            "a denied named program is refused"
        );
    }

    /// No count ceiling: deep worktrees with 145 and 1,210 ancestor-file read roots expand whole
    /// (rules checked lexically; existence is not required for this variant).
    #[test]
    fn deep_worktrees_keep_every_ancestor_root() {
        const DEEP: EffectRecipe = EffectRecipe {
            id: "deep",
            program: "cargo",
            args: &[],
            env: &[],
            paths: &[PathRule {
                param: "ancestors",
                roles: &[PathRole::AncestorFile(ANCESTOR_FILES)],
                existing_only: false,
                read_root: true,
            }],
            executables: &[ExecutableSlot {
                name: "cargo",
                source: SlotSource::Launcher("cargo"),
            }],
            stdin: Stdin::Null,
            class: RunClass::Background,
            timeout_ceiling_ms: 900_000,
            capture_bytes: 64 << 20,
        };
        for (depth, expected) in [(48usize, 144usize), (403, 1209)] {
            let worktree: PathBuf = std::iter::once("/".to_owned())
                .chain((0..depth).map(|_| "a".to_owned()))
                .collect();
            let roots: Vec<PathBuf> = worktree
                .ancestors()
                .skip(1)
                .flat_map(|ancestor| ANCESTOR_FILES.iter().map(move |name| ancestor.join(name)))
                .collect();
            assert_eq!(roots.len(), expected);
            let effect = EffectRequest {
                recipe: "deep".into(),
                params: BTreeMap::from([
                    ("cargo".into(), Param::Executable("cargo".into())),
                    ("ancestors".into(), Param::Paths(roots.clone())),
                ]),
            };
            let programs = [("cargo".to_owned(), PathBuf::from("/bin/true"))];
            let admission = Admission {
                worktree: &worktree,
                cache_dir: Path::new("/c"),
                read_denies: &[],
                home: None,
                launcher_roots: &[],
                developer_dirs: &[],
                programs: &programs,
                timeout: Duration::from_secs(1),
            };
            let spec = expand(&[DEEP], &effect, &admission).unwrap();
            assert_eq!(spec.read_roots, roots, "{depth} levels");
            assert!(spec.read_roots.len() > 128);
        }
    }
}
