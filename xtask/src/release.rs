//! Release automation in the family shape for the `archive-bundle-v1` delivery profile:
//! `package` seals the deterministic bundle (byte-identical to the former shell packager),
//! `package verify` is the release smoke test plus the manifest ↔ files ↔ `SHA256SUMS` check,
//! `release prepare` performs the local version/CHANGELOG edits, `release manifest` writes the
//! schema-shaped `release-manifest.json`, `release publish` stages a complete draft, verifies it
//! and makes it visible, and `release wait` observes one exact tag/commit/run. Hashing, archiving
//! and GitHub access go through `shasum`, `tar`/`gzip` and `gh`; nothing is reimplemented.
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::{Value, json};

use crate::{Result, read, run, table_value, target_dir};

/// The only qualified target; the archive name and the manifest bundle entry carry it.
const TARGET: &str = "aarch64-apple-darwin";
/// The release manifest asset; it is listed in `SHA256SUMS` but never lists itself (PKG-02).
const MANIFEST: &str = "release-manifest.json";
/// The CI product-acceptance evidence asset, bound to the archive digest.
const EVIDENCE: &str = "acceptance.json";
/// The installer asset users fetch first; the bootstrap looks the tarball up in `SHA256SUMS`.
const INSTALLER: &str = "install.sh";
/// Upper bound for a manifest read from a directory or a published release.
const MANIFEST_MAX_BYTES: u64 = 16384;
/// Checkout files copied verbatim into the bundle (paths are identical on both sides).
const COPIED: [&str; 11] = [
    "install.sh",
    "README.md",
    "docs/release.md",
    ".claude-plugin/plugin.json",
    ".claude-plugin/marketplace.json",
    "agents/ide-reviewer.md",
    ".codex-plugin/plugin.json",
    "hooks/hooks.json",
    "hooks/claude-hook.sh",
    "skills/agent-ide/SKILL.md",
    "skills/agent-ide/agents/openai.yaml",
];
/// Bundle files that are made executable after copying.
const EXECUTABLES: [&str; 3] = ["agent-ide", "install.sh", "hooks/claude-hook.sh"];
/// The complete archive file set, bundle-relative and in byte order.
const BUNDLE_FILES: [&str; 16] = [
    ".agents/plugins/marketplace.json",
    ".claude-plugin/marketplace.json",
    ".claude-plugin/plugin.json",
    ".codex-plugin/plugin.json",
    "COMPLETE",
    "README.md",
    "SHA256SUMS",
    "agent-ide",
    "agents/ide-reviewer.md",
    "docs/release.md",
    "hooks/claude-hook.sh",
    "hooks/hooks.json",
    "install.sh",
    "metadata.json",
    "skills/agent-ide/SKILL.md",
    "skills/agent-ide/agents/openai.yaml",
];
/// The Codex marketplace catalog written into the bundle, byte for byte.
const CODEX_MARKETPLACE: &str = "{\n  \"name\": \"agent-ide\",\n  \"interface\": {\"displayName\": \"Agent IDE\"},\n  \"plugins\": [\n    {\n      \"name\": \"agent-ide\",\n      \"source\": {\"source\": \"local\", \"path\": \"./\"}\n    }\n  ]\n}\n";

/// A uniquely named temporary directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Result<Self> {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path = std::env::temp_dir().join(format!("{label}.{}.{nanos}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Runs `program` and returns its stdout as text; a non-zero exit is an error naming the program.
fn output(program: impl AsRef<std::ffi::OsStr>, args: &[&str]) -> Result<String> {
    let program = program.as_ref();
    let out = Command::new(program)
        .args(args)
        .stderr(Stdio::inherit())
        .output()?;
    if !out.status.success() {
        return Err(format!("{} {} failed", program.to_string_lossy(), args.join(" ")).into());
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// The lowercase SHA-256 of one file, from `shasum -a 256`.
fn sha256(path: &Path) -> Result<String> {
    let text = output(
        "shasum",
        &["-a", "256", path.to_str().ok_or("non-UTF-8 path")?],
    )?;
    let digest = text
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_owned();
    if is_hex(&digest, 64) {
        Ok(digest)
    } else {
        Err("shasum returned no digest".into())
    }
}

fn is_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `vMAJOR.MINOR.PATCH` with an optional `-rc.N`, no leading zeros; returns the ordering key.
fn version_key(version: &str) -> Option<(u64, u64, u64, u64)> {
    let number = |part: &str| {
        (!part.is_empty()
            && part.bytes().all(|b| b.is_ascii_digit())
            && (part == "0" || !part.starts_with('0')))
        .then(|| part.parse().ok())
        .flatten()
    };
    let (core, rc) = match version.split_once("-rc.") {
        Some((core, rc)) => (core, number(rc)?),
        None => (version, u64::MAX),
    };
    let mut parts = core.split('.');
    let key = (
        number(parts.next()?)?,
        number(parts.next()?)?,
        number(parts.next()?)?,
        rc,
    );
    parts.next().is_none().then_some(key)
}

fn repo_valid(repo: &str) -> bool {
    let parts: Vec<_> = repo.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 100
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        })
}

fn asset_name(tag: &str) -> String {
    format!("agent-ide-{tag}-{TARGET}.tar.gz")
}

fn json_file(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

// ---------------------------------------------------------------------------------------------
// package

/// Builds the deterministic bundle from one already-built executable and writes the tarball and
/// its `SHA256SUMS` into `output`; prints the tarball path last. Byte-identical to the former
/// `scripts/package-release.sh` for the same binary, tag and checkout.
pub fn package(root: &Path, binary: &Path, tag: &str, output_dir: &Path) -> Result<PathBuf> {
    let binary_meta = fs::metadata(binary)?;
    if !binary_meta.is_file() || binary_meta.permissions().mode() & 0o111 == 0 {
        return Err("release binary must be an executable regular file".into());
    }
    let version = tag
        .strip_prefix('v')
        .filter(|v| version_key(v).is_some())
        .ok_or("release tag must have vMAJOR.MINOR.PATCH form")?;
    for (file, pointer) in [
        (".codex-plugin/plugin.json", "/version"),
        (".claude-plugin/plugin.json", "/version"),
        (".claude-plugin/marketplace.json", "/plugins/0/version"),
    ] {
        if json_file(&root.join(file))?
            .pointer(pointer)
            .and_then(Value::as_str)
            != Some(version)
        {
            return Err(format!("{file} does not carry version {version}").into());
        }
    }
    let name = format!("agent-ide-{tag}");
    let asset = asset_name(tag);
    let tmp = TempDir::new("agent-ide-package")?;
    let bundle = tmp.0.join(&name);
    fs::create_dir_all(bundle.join(".agents/plugins"))?;
    fs::copy(binary, bundle.join("agent-ide"))?;
    for file in COPIED {
        let target = bundle.join(file);
        fs::create_dir_all(target.parent().ok_or("bundle file without a parent")?)?;
        fs::copy(root.join(file), target)?;
    }
    fs::write(
        bundle.join(".agents/plugins/marketplace.json"),
        CODEX_MARKETPLACE,
    )?;
    for file in EXECUTABLES {
        fs::set_permissions(bundle.join(file), fs::Permissions::from_mode(0o755))?;
    }
    // The seal: metadata names the version, SHA256SUMS covers every regular file except itself,
    // and COMPLETE carries the exact bytes whose digest SHA256SUMS records. COMPLETE is written
    // before hashing here; its digest and the resulting SHA256SUMS bytes are the same.
    fs::write(
        bundle.join("metadata.json"),
        format!("{{\n  \"version\": \"{version}\",\n  \"format\": 1\n}}\n"),
    )?;
    fs::write(bundle.join("COMPLETE"), "complete\n")?;
    let mut files = Vec::new();
    collect_files(&bundle, &bundle, &mut files)?;
    files.retain(|f| Path::new(f).file_name().is_some_and(|n| n != "SHA256SUMS"));
    files.sort();
    let mut shasum = Command::new("shasum");
    shasum.current_dir(&bundle).args(["-a", "256"]).args(&files);
    let sums = shasum.output()?;
    if !sums.status.success() {
        return Err("shasum over the bundle failed".into());
    }
    fs::write(bundle.join("SHA256SUMS"), sums.stdout)?;
    run(
        root,
        "find",
        &[
            bundle.to_str().ok_or("non-UTF-8 path")?,
            "-exec",
            "touch",
            "-t",
            "197001010000",
            "{}",
            "+",
        ],
    )?;
    fs::create_dir_all(output_dir)?;
    let archive = output_dir.join(&asset);
    let mut tar = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .arg("-cf")
        .arg("-")
        .arg("-C")
        .arg(&tmp.0)
        .arg(&name)
        .stdout(Stdio::piped())
        .spawn()?;
    let gzip = Command::new("gzip")
        .arg("-n")
        .stdin(tar.stdout.take().ok_or("tar stdout unavailable")?)
        .stdout(fs::File::create(&archive)?)
        .status()?;
    if !tar.wait()?.success() || !gzip.success() {
        return Err("tar | gzip failed".into());
    }
    fs::write(
        output_dir.join("SHA256SUMS"),
        format!("{}  {asset}\n", sha256(&archive)?),
    )?;
    println!("{}", archive.display());
    Ok(archive)
}

/// Collects `base`-relative paths of every regular file below `dir`.
fn collect_files(base: &Path, dir: &Path, out: &mut Vec<String>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_files(base, &entry.path(), out)?;
        } else if kind.is_file() {
            out.push(
                entry
                    .path()
                    .strip_prefix(base)?
                    .to_str()
                    .ok_or("non-UTF-8 path")?
                    .to_owned(),
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// package verify

/// Parses a `SHA256SUMS` file into name → digest; names must be plain relative paths.
fn read_sums(path: &Path) -> Result<BTreeMap<String, String>> {
    let mut sums = BTreeMap::new();
    for line in fs::read_to_string(path)?.lines() {
        let (digest, name) = line
            .split_once("  ")
            .or_else(|| line.split_once(" *"))
            .ok_or("malformed SHA256SUMS line")?;
        if !is_hex(digest, 64)
            || name.is_empty()
            || name.starts_with('/')
            || name.split('/').any(|part| part == "..")
            || sums.insert(name.to_owned(), digest.to_owned()).is_some()
        {
            return Err(format!("invalid SHA256SUMS entry: {name}").into());
        }
    }
    Ok(sums)
}

/// `shasum -c` semantics: every listed file exists and matches; returns the parsed list.
fn check_sums(dir: &Path) -> Result<BTreeMap<String, String>> {
    let sums = read_sums(&dir.join("SHA256SUMS"))?;
    for (name, digest) in &sums {
        if sha256(&dir.join(name))? != *digest {
            return Err(format!("{name} does not match SHA256SUMS").into());
        }
    }
    Ok(sums)
}

/// Verifies one release archive: its directory's `SHA256SUMS`, the manifest binding when a
/// `release-manifest.json` sits next to it, then everything the former release smoke test
/// checked — exact file set, regular files only, Mach-O arm64 executable, versions, the seal,
/// marketplaces, the executable measurement, the Claude hook and a disposable self-install.
pub fn verify(asset: &Path) -> Result<()> {
    let asset = fs::canonicalize(asset)?;
    let name = asset
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("archive name missing")?
        .to_owned();
    let tag = name
        .strip_prefix("agent-ide-")
        .and_then(|rest| rest.strip_suffix(&format!("-{TARGET}.tar.gz")))
        .filter(|tag| tag.strip_prefix('v').and_then(version_key).is_some())
        .ok_or("unexpected release archive name")?
        .to_owned();
    let version = &tag[1..];
    let dir = asset.parent().ok_or("archive directory missing")?;
    if !check_sums(dir)?.contains_key(&name) {
        return Err(format!("SHA256SUMS does not list {name}").into());
    }
    if dir.join(MANIFEST).exists() {
        let manifest = verify_manifest(dir)?;
        if manifest["tag"] != tag.as_str() {
            return Err("release manifest names another tag".into());
        }
    }
    let bundle_name = format!("agent-ide-{tag}");
    let listing = output("tar", &["-tzf", asset.to_str().ok_or("non-UTF-8 path")?])?;
    let mut actual: Vec<&str> = listing.lines().filter(|l| !l.ends_with('/')).collect();
    actual.sort_unstable();
    let expected: Vec<String> = BUNDLE_FILES
        .iter()
        .map(|f| format!("{bundle_name}/{f}"))
        .collect();
    if actual != expected {
        return Err("release archive contents do not match the plugin bundle contract".into());
    }
    let tmp = TempDir::new("agent-ide-smoke")?;
    let tmp_str = tmp.0.to_str().ok_or("non-UTF-8 path")?;
    run(
        dir,
        "tar",
        &[
            "-xzf",
            asset.to_str().ok_or("non-UTF-8 path")?,
            "-C",
            tmp_str,
        ],
    )?;
    let bundle = tmp.0.join(&bundle_name);
    for file in BUNDLE_FILES {
        if !fs::symlink_metadata(bundle.join(file))?
            .file_type()
            .is_file()
        {
            return Err(format!("bundle member {file} is not a regular file").into());
        }
    }
    let bin = bundle.join("agent-ide");
    let bin_str = bin.to_str().ok_or("non-UTF-8 path")?;
    if fs::metadata(&bin)?.permissions().mode() & 0o111 == 0
        || !output("file", &[bin_str])?.contains("Mach-O 64-bit executable arm64")
    {
        return Err("packaged agent-ide is not a macOS arm64 executable".into());
    }
    for file in [".codex-plugin/plugin.json", ".claude-plugin/plugin.json"] {
        if json_file(&bundle.join(file))?["version"] != version {
            return Err(format!("{file} does not carry version {version}").into());
        }
    }
    if json_file(&bundle.join("metadata.json"))? != json!({"version": version, "format": 1}) {
        return Err("metadata.json does not name this version in format 1".into());
    }
    if fs::read(bundle.join("COMPLETE"))? != b"complete\n" {
        return Err("release seal COMPLETE is not exactly \"complete\\n\"".into());
    }
    check_sums(&bundle)?;
    if json_file(&bundle.join(".claude-plugin/marketplace.json"))?["plugins"]
        != json!([{"name": "agent-ide", "source": "./", "description": "Agent IDE coding-companion skill and Claude lifecycle hooks.", "version": version}])
        || json_file(&bundle.join(".agents/plugins/marketplace.json"))?["plugins"]
            != json!([{"name": "agent-ide", "source": {"source": "local", "path": "./"}}])
    {
        return Err("bundle marketplaces do not select the root plugin".into());
    }
    let identity = format!("agent-ide-{tag}");
    let measurement: Value = serde_json::from_str(&output(
        &bin,
        &["evidence", "executable", "--identity", &identity, bin_str],
    )?)?;
    if measurement["identity"] != identity.as_str()
        || measurement["path"] != bin_str
        || measurement["blake3"].as_str().map(str::len) != Some(64)
    {
        return Err("packaged executable measurement mismatch".into());
    }
    let mut hook = Command::new(bundle.join("hooks/claude-hook.sh"))
        .env("AGENT_IDE_BIN", &bin)
        .env("CLAUDE_PROJECT_DIR", &tmp.0)
        .stdin(Stdio::piped())
        .spawn()?;
    hook.stdin
        .take()
        .ok_or("hook stdin unavailable")?
        .write_all(b"{}\n")?;
    if !hook.wait()?.success() {
        return Err("packaged Claude hook failed".into());
    }
    // The bundle must install itself into disposable dirs: managed shim, both `current` links and
    // the payload binary behind the launcher.
    let at = |part: &str| tmp.0.join(part);
    let mut install = Command::new(&bin);
    install
        .args(["self-install", "--release"])
        .arg(&bundle)
        .args(["--version", version, "--home"])
        .arg(at("home"))
        .arg("--prefix")
        .arg(at("prefix"))
        .arg("--bin-dir")
        .arg(at("bin"))
        .arg("--share-dir")
        .arg(at("share"));
    if !install.status()?.success() {
        return Err("packaged self-install failed".into());
    }
    if fs::read_link(at("prefix/current"))? != Path::new(&format!("releases/{version}"))
        || fs::read_link(at("share/plugin/current"))? != Path::new(version)
        || output(at("bin/agent-ide"), &["--version"])?.trim_end() != format!("agent-ide {version}")
        || !fs::read_to_string(at("bin/agent-ide"))?
            .contains(&format!("exec '{tmp_str}/prefix/current/agent-ide'"))
    {
        return Err("self-installed layout does not launch this version".into());
    }
    println!("package verify: {name} passed");
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// release manifest

/// Checks a manifest against `schemas/release-manifest.schema.json` of the family template
/// (closed objects, required fields, patterns, enums, bounds), field by field.
fn validate_manifest(m: &Value) -> Result<()> {
    let keys = |v: &Value| -> BTreeSet<String> {
        v.as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default()
    };
    let set =
        |names: &[&str]| -> BTreeSet<String> { names.iter().map(|s| s.to_string()).collect() };
    let text = |v: &Value| {
        v.as_str()
            .filter(|s| !s.is_empty() && s.len() <= 256)
            .is_some()
    };
    let positive = |v: &Value| v.as_u64().is_some_and(|n| n >= 1);
    let version = m["version"].as_str().unwrap_or_default();
    let workflow = &m["workflow"];
    let path = workflow["path"].as_str().unwrap_or_default();
    let valid_path = path
        .strip_prefix(".github/workflows/")
        .and_then(|f| f.strip_suffix(".yml").or_else(|| f.strip_suffix(".yaml")))
        .is_some_and(|stem| {
            !stem.is_empty()
                && stem
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        });
    let top = set(&[
        "schema_version",
        "repository",
        "repository_id",
        "product",
        "version",
        "tag",
        "commit",
        "workflow",
        "standard_version",
        "devkit_version",
        "baseline",
        "trust_profile",
        "artifacts",
    ]);
    if keys(m) != top
        || m["schema_version"] != 1
        || !m["repository"].as_str().is_some_and(repo_valid)
        || !positive(&m["repository_id"])
        || m["product"] != "agent-ide"
        || version_key(version).is_none()
        || m["tag"] != format!("v{version}").as_str()
        || !m["commit"].as_str().is_some_and(|c| is_hex(c, 40))
        || keys(workflow) != set(&["id", "path", "run_id", "run_attempt"])
        || !positive(&workflow["id"])
        || !valid_path
        || !positive(&workflow["run_id"])
        || !positive(&workflow["run_attempt"])
        || !text(&m["standard_version"])
        || !text(&m["devkit_version"])
        || !text(&m["baseline"])
        || !matches!(
            m["trust_profile"].as_str(),
            Some("github-authenticated" | "github-attestation")
        )
    {
        return Err("release manifest does not match the family schema".into());
    }
    let artifacts = m["artifacts"]
        .as_array()
        .ok_or("manifest artifacts missing")?;
    if artifacts.is_empty() || artifacts.len() > 64 {
        return Err("manifest artifacts out of bounds".into());
    }
    let allowed = set(&["name", "kind", "size", "sha256", "target"]);
    for a in artifacts {
        let k = keys(a);
        let name = a["name"].as_str().unwrap_or_default();
        let target_ok = match a.get("target") {
            None => a["kind"] != "bundle",
            Some(t) => t.as_str().is_some_and(|t| {
                !t.is_empty()
                    && t.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            }),
        };
        if !k.is_subset(&allowed)
            || !["name", "kind", "size", "sha256"]
                .iter()
                .all(|r| k.contains(*r))
            || !name
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric())
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            || !matches!(
                a["kind"].as_str(),
                Some("bundle" | "installer" | "evidence" | "source" | "sbom" | "provenance")
            )
            || !a["size"]
                .as_u64()
                .is_some_and(|s| (1..=536_870_912).contains(&s))
            || !a["sha256"].as_str().is_some_and(|s| is_hex(s, 64))
            || !target_ok
        {
            return Err(
                format!("release manifest artifact {name} does not match the schema").into(),
            );
        }
    }
    Ok(())
}

/// Verifies a payload directory: the manifest is schema-valid and lists exactly the bundle,
/// `install.sh` and the acceptance evidence; every listed file has the recorded size and digest;
/// `SHA256SUMS` lists exactly those files plus the manifest with the same digests; the evidence
/// is a product pass of the manifest commit naming the bundle's exact digest.
fn verify_manifest(dir: &Path) -> Result<Value> {
    let path = dir.join(MANIFEST);
    if fs::metadata(&path)?.len() > MANIFEST_MAX_BYTES {
        return Err("release manifest too large".into());
    }
    let m = json_file(&path)?;
    validate_manifest(&m)?;
    let tag = m["tag"].as_str().unwrap_or_default();
    let bundle = asset_name(tag);
    let sums = read_sums(&dir.join("SHA256SUMS"))?;
    let artifacts = m["artifacts"]
        .as_array()
        .ok_or("manifest artifacts missing")?;
    let names: BTreeSet<&str> = artifacts
        .iter()
        .filter_map(|a| a["name"].as_str())
        .collect();
    let listed: BTreeSet<&str> = sums.keys().map(String::as_str).collect();
    if names != BTreeSet::from([bundle.as_str(), INSTALLER, EVIDENCE])
        || listed != BTreeSet::from([bundle.as_str(), INSTALLER, EVIDENCE, MANIFEST])
        || artifacts.len() != 3
    {
        return Err("manifest, SHA256SUMS and the payload name different files".into());
    }
    if sums[MANIFEST] != sha256(&path)? {
        return Err("SHA256SUMS does not match the release manifest".into());
    }
    for a in artifacts {
        let name = a["name"].as_str().unwrap_or_default();
        let file = dir.join(name);
        let expected_kind = match name {
            INSTALLER => "installer",
            EVIDENCE => "evidence",
            _ => "bundle",
        };
        let digest = sha256(&file)?;
        if a["kind"] != expected_kind
            || (expected_kind == "bundle") != (a["target"] == TARGET)
            || a["size"].as_u64() != Some(fs::metadata(&file)?.len())
            || a["sha256"] != digest.as_str()
            || sums[name] != digest
        {
            return Err(format!("{name} differs from the release manifest").into());
        }
    }
    let evidence = json_file(&dir.join(EVIDENCE))?;
    if evidence["route"] != "product"
        || evidence["status"] != "product_pass"
        || evidence["revision"] != m["commit"]
        || evidence["payload"] != json!({"name": bundle, "sha256": sums[&bundle]})
    {
        return Err("acceptance evidence is not a product pass of this commit and payload".into());
    }
    Ok(m)
}

/// The GitHub Actions identity of the running workflow.
struct Ci {
    repository: String,
    ref_type: String,
    ref_name: String,
    sha: String,
    run_id: u64,
    run_attempt: u64,
}

fn env(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| format!("{name} is required").into())
}

fn ci_from_env() -> Result<Ci> {
    Ok(Ci {
        repository: env("GITHUB_REPOSITORY")?,
        ref_type: env("GITHUB_REF_TYPE")?,
        ref_name: env("GITHUB_REF_NAME")?,
        sha: env("GITHUB_SHA")?,
        run_id: env("GITHUB_RUN_ID")?.parse()?,
        run_attempt: env("GITHUB_RUN_ATTEMPT")?.parse()?,
    })
}

/// Writes `release-manifest.json` and the aggregate `SHA256SUMS` (tarball, `install.sh`,
/// evidence, manifest) into a payload directory from the CI identity, then verifies the result.
/// `RELEASE_WORKFLOW_ID` carries the numeric workflow id the workflow resolved through the API.
pub fn manifest(root: &Path, dir: &Path) -> Result<()> {
    let ci = ci_from_env()?;
    let workflow_ref = env("GITHUB_WORKFLOW_REF")?;
    let workflow_path = workflow_ref
        .strip_prefix(&format!("{}/", ci.repository))
        .and_then(|rest| rest.split('@').next())
        .ok_or("GITHUB_WORKFLOW_REF does not name a workflow of this repository")?
        .to_owned();
    let head = output(
        "git",
        &[
            "-C",
            root.to_str().ok_or("non-UTF-8 path")?,
            "rev-parse",
            "HEAD",
        ],
    )?;
    if head.trim() != ci.sha {
        return Err("the checkout is not GITHUB_SHA".into());
    }
    let m = build_manifest(
        root,
        dir,
        &ci,
        env("GITHUB_REPOSITORY_ID")?.parse()?,
        env("RELEASE_WORKFLOW_ID")?.parse()?,
        &workflow_path,
    )?;
    fs::write(
        dir.join(MANIFEST),
        format!("{}\n", serde_json::to_string_pretty(&m)?),
    )?;
    let mut sums = String::new();
    for name in [
        m["artifacts"][0]["name"].as_str().unwrap_or_default(),
        INSTALLER,
        EVIDENCE,
        MANIFEST,
    ] {
        sums.push_str(&format!("{}  {name}\n", sha256(&dir.join(name))?));
    }
    fs::write(dir.join("SHA256SUMS"), sums)?;
    verify_manifest(dir)?;
    println!("release manifest: {} written", dir.join(MANIFEST).display());
    Ok(())
}

/// Assembles the manifest value for the payload in `dir`; the tag is `v` + the workspace version
/// and must equal the pushed tag when the run is a tag push.
fn build_manifest(
    root: &Path,
    dir: &Path,
    ci: &Ci,
    repository_id: u64,
    workflow_id: u64,
    workflow_path: &str,
) -> Result<Value> {
    let family = read(root, "family.toml")?;
    let field = |section: &str, key: &str| {
        table_value(&family, section, key).ok_or(format!("family.toml lacks {key}"))
    };
    let version = workspace_version(root)?;
    let tag = format!("v{version}");
    if ci.ref_type == "tag" && ci.ref_name != tag {
        return Err(format!(
            "tag {} does not match workspace version {version}",
            ci.ref_name
        )
        .into());
    }
    if ci.repository != field("", "repository")? {
        return Err("GITHUB_REPOSITORY is not the family repository".into());
    }
    let bundle = asset_name(&tag);
    let mut artifacts = Vec::new();
    for (name, kind) in [
        (bundle.as_str(), "bundle"),
        (INSTALLER, "installer"),
        (EVIDENCE, "evidence"),
    ] {
        let path = dir.join(name);
        let mut entry = json!({"name": name, "kind": kind, "size": fs::metadata(&path)?.len(), "sha256": sha256(&path)?});
        if kind == "bundle" {
            entry["target"] = json!(TARGET);
        }
        artifacts.push(entry);
    }
    let m = json!({
        "schema_version": 1,
        "repository": ci.repository,
        "repository_id": repository_id,
        "product": field("", "product")?,
        "version": version,
        "tag": tag,
        "commit": ci.sha,
        "workflow": {"id": workflow_id, "path": workflow_path, "run_id": ci.run_id, "run_attempt": ci.run_attempt},
        "standard_version": field("", "standard_version")?,
        "devkit_version": field("", "devkit_version")?,
        "baseline": field("", "baseline")?,
        "trust_profile": field("release", "trust_profile")?,
        "artifacts": artifacts,
    });
    validate_manifest(&m)?;
    Ok(m)
}

fn workspace_version(root: &Path) -> Result<String> {
    table_value(&read(root, "Cargo.toml")?, "workspace.package", "version")
        .ok_or("workspace version missing".into())
}

// ---------------------------------------------------------------------------------------------
// release prepare

/// One planned local file edit and its human-readable description.
struct Edit {
    file: &'static str,
    content: String,
    what: String,
}

/// Replaces the `version` line of `[workspace.package]`, preserving everything else.
fn bump_workspace_version(source: &str, from: &str, to: &str) -> Option<String> {
    let mut in_package = false;
    let mut bumped = false;
    let mut out = source
        .lines()
        .map(|line| {
            if line.trim_start().starts_with('[') {
                in_package = line.trim() == "[workspace.package]";
            }
            if in_package && !bumped && line.trim() == format!("version = \"{from}\"") {
                bumped = true;
                format!("version = \"{to}\"")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    if source.ends_with('\n') {
        out.push('\n');
    }
    bumped.then_some(out)
}

/// Bumps the `version` of every `[[package]]` whose name starts with `agent-ide` and whose version
/// is `from`; third-party crates sharing the version string are untouched. Returns the new text
/// and the bumped package names.
fn bump_lock(source: &str, from: &str, to: &str) -> (String, Vec<String>) {
    let mut current: Option<String> = None;
    let mut bumped = Vec::new();
    let mut out = source
        .lines()
        .map(|line| {
            if line == "[[package]]" {
                current = None;
            } else if let Some(name) = line
                .strip_prefix("name = \"")
                .and_then(|rest| rest.strip_suffix('"'))
            {
                current = Some(name.to_owned());
            } else if line == format!("version = \"{from}\"")
                && let Some(name) = current.take().filter(|n| n.starts_with("agent-ide"))
            {
                bumped.push(name);
                return format!("version = \"{to}\"");
            }
            line.to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n");
    if source.ends_with('\n') {
        out.push('\n');
    }
    (out, bumped)
}

/// Previews (or with `apply`, performs) the local release edits: workspace version, the three
/// plugin manifests, the `agent-ide*` Cargo.lock entries and the CHANGELOG heading. Never commits,
/// tags or pushes; `--apply` requires a clean checkout and ends with a locked metadata check.
pub fn prepare(root: &Path, version: &str, apply: bool, date: &str) -> Result<()> {
    if version.contains('-') {
        return Err(
            "agent-ide versions are MAJOR.MINOR.PATCH: self-install refuses pre-release \
                    versions (docs/qualification.md)"
                .into(),
        );
    }
    let old = workspace_version(root)?;
    let (Some(new_key), Some(old_key)) = (version_key(version), version_key(&old)) else {
        return Err("version must be MAJOR.MINOR.PATCH or MAJOR.MINOR.PATCH-rc.N".into());
    };
    if new_key <= old_key {
        return Err(format!("version must increase beyond {old}").into());
    }
    let mut edits = Vec::new();
    edits.push(Edit {
        file: "Cargo.toml",
        content: bump_workspace_version(&read(root, "Cargo.toml")?, &old, version)
            .ok_or("expected the version line in [workspace.package]")?,
        what: format!("[workspace.package] version {old} -> {version}"),
    });
    for (file, pointer) in [
        (".codex-plugin/plugin.json", "/version"),
        (".claude-plugin/plugin.json", "/version"),
        (".claude-plugin/marketplace.json", "/plugins/0/version"),
    ] {
        let text = read(root, file)?;
        let needle = format!("\"version\": \"{old}\"");
        let content = text.replacen(&needle, &format!("\"version\": \"{version}\""), 1);
        let parsed: Value = serde_json::from_str(&content)?;
        if text.matches(&needle).count() != 1
            || parsed.pointer(pointer).and_then(Value::as_str) != Some(version)
        {
            return Err(format!("{file}: expected exactly one {needle} at {pointer}").into());
        }
        edits.push(Edit {
            file,
            content,
            what: format!(
                "{} {old} -> {version}",
                pointer.trim_start_matches('/').replace('/', ".")
            ),
        });
    }
    let (content, names) = bump_lock(&read(root, "Cargo.lock")?, &old, version);
    if !names.iter().any(|n| n == "agent-ide") {
        return Err("Cargo.lock has no agent-ide entry at the current version".into());
    }
    edits.push(Edit {
        file: "Cargo.lock",
        content,
        what: format!("version {old} -> {version} for {}", names.join(", ")),
    });
    let changelog = read(root, "CHANGELOG.md")?;
    if changelog.lines().filter(|l| *l == "## Unreleased").count() != 1 {
        return Err("CHANGELOG.md needs exactly one `## Unreleased` heading".into());
    }
    let section = changelog
        .lines()
        .skip_while(|l| *l != "## Unreleased")
        .skip(1)
        .take_while(|l| !l.starts_with("## "))
        .collect::<String>();
    if section.trim().is_empty() {
        return Err("CHANGELOG.md `## Unreleased` section is empty".into());
    }
    let heading = format!("## {version} — {date}");
    edits.push(Edit {
        file: "CHANGELOG.md",
        content: changelog.replacen(
            "## Unreleased\n",
            &format!("## Unreleased\n\n{heading}\n"),
            1,
        ),
        what: format!("`## Unreleased` -> `{heading}` under a new empty `## Unreleased`"),
    });
    let mode = if apply { "apply" } else { "preview" };
    println!("release prepare ({mode}): {old} -> {version}; no commit, tag or push");
    for edit in &edits {
        println!("  {}: {}", edit.file, edit.what);
    }
    if !apply {
        println!("re-run with --apply on a clean checkout to write these edits");
        return Ok(());
    }
    if !output(
        "git",
        &[
            "-C",
            root.to_str().ok_or("non-UTF-8 path")?,
            "status",
            "--porcelain",
        ],
    )?
    .is_empty()
    {
        return Err("--apply requires a clean checkout".into());
    }
    for edit in &edits {
        fs::write(root.join(edit.file), &edit.content)?;
    }
    // A full offline resolution under --locked fails if the edited lock no longer matches.
    output(
        "cargo",
        &[
            "metadata",
            "--locked",
            "--offline",
            "--format-version",
            "1",
            "--manifest-path",
            root.join("Cargo.toml").to_str().ok_or("non-UTF-8 path")?,
        ],
    )?;
    println!(
        "edits written; review the diff, run `cargo xtask check`, commit, then tag v{version}"
    );
    Ok(())
}

/// Today's local date as `YYYY-MM-DD`.
pub fn today() -> Result<String> {
    Ok(output("date", &["+%Y-%m-%d"])?.trim().to_owned())
}

// ---------------------------------------------------------------------------------------------
// release publish

fn gh_json(gh: &Path, args: &[&str]) -> Result<Value> {
    Ok(serde_json::from_str(&output(gh, args)?)?)
}

/// Publishes a verified CI payload: identity checks against the CI run and the checked-out tag,
/// refusal when any release or draft exists for the tag, a draft with every asset and generated
/// notes, a download-back verification against the manifest, then publication and a final check
/// that the visible release carries exactly the verified assets. A failure after the draft exists
/// leaves the draft for inspection.
fn publish(gh: &Path, dir: &Path, ci: &Ci, tag_commit: &str, work: &Path) -> Result<()> {
    let m = verify_manifest(dir)?;
    let tag = m["tag"].as_str().unwrap_or_default();
    let repo = m["repository"].as_str().unwrap_or_default();
    if ci.repository != repo
        || ci.ref_type != "tag"
        || ci.ref_name != tag
        || ci.sha != m["commit"]
        || tag_commit != ci.sha
        || m["workflow"]["run_id"] != ci.run_id
        || m["workflow"]["run_attempt"] != ci.run_attempt
    {
        return Err("CI, tag, source or run identity differs from the release manifest".into());
    }
    let existing = output(
        gh,
        &[
            "api",
            "--paginate",
            &format!("repos/{repo}/releases"),
            "--jq",
            &format!(".[] | select(.tag_name == \"{tag}\") | .id"),
        ],
    )?;
    if !existing.trim().is_empty() {
        return Err(
            format!("a release or draft for {tag} already exists; refusing to overwrite").into(),
        );
    }
    let mut assets: Vec<String> = m["artifacts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a["name"].as_str().map(str::to_owned))
        .collect();
    assets.extend([MANIFEST.to_owned(), "SHA256SUMS".to_owned()]);
    let paths: Vec<String> = assets
        .iter()
        .map(|a| {
            dir.join(a)
                .to_str()
                .map(str::to_owned)
                .ok_or("non-UTF-8 path")
        })
        .collect::<std::result::Result<_, _>>()?;
    let title = format!("Agent IDE {tag}");
    let mut create = vec!["release", "create", tag];
    create.extend(paths.iter().map(String::as_str));
    create.extend([
        "--repo",
        repo,
        "--verify-tag",
        "--draft",
        "--generate-notes",
        "--title",
        title.as_str(),
    ]);
    let prerelease = tag.contains("-rc.");
    if prerelease {
        create.push("--prerelease");
    }
    output(gh, &create)?;
    let left = |what: &str| format!("{what}; draft {tag} left unpublished for inspection");
    fs::create_dir(work)?;
    let work_str = work.to_str().ok_or("non-UTF-8 path")?;
    output(
        gh,
        &[
            "release", "download", tag, "--repo", repo, "--dir", work_str,
        ],
    )
    .map_err(|e| left(&e.to_string()))?;
    let mut downloaded = Vec::new();
    collect_files(work, work, &mut downloaded)?;
    downloaded.sort();
    let mut expected = assets.clone();
    expected.sort();
    if downloaded != expected
        || fs::read(work.join(MANIFEST))? != fs::read(dir.join(MANIFEST))?
        || fs::read(work.join("SHA256SUMS"))? != fs::read(dir.join("SHA256SUMS"))?
    {
        return Err(left("the draft's assets differ from the verified payload").into());
    }
    verify_manifest(work).map_err(|e| left(&e.to_string()))?;
    output(
        gh,
        &["release", "edit", tag, "--repo", repo, "--draft=false"],
    )
    .map_err(|e| left(&e.to_string()))?;
    let release = gh_json(gh, &["api", &format!("repos/{repo}/releases/tags/{tag}")])?;
    let visible: BTreeMap<String, u64> = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| Some((a["name"].as_str()?.to_owned(), a["size"].as_u64()?)))
        .collect();
    let mut sizes = BTreeMap::new();
    for asset in &assets {
        sizes.insert(asset.clone(), fs::metadata(dir.join(asset))?.len());
    }
    if release["draft"] != false || release["prerelease"] != prerelease || visible != sizes {
        return Err(
            format!("published release {tag} does not carry exactly the verified assets").into(),
        );
    }
    println!(
        "{}",
        json!({"status": "published", "tag": tag, "commit": m["commit"], "artifact_integrity": "verified", "provenance_verification": "not_performed"})
    );
    Ok(())
}

/// `release publish <dir>` from inside the release workflow.
pub fn publish_from_env(root: &Path, dir: &Path) -> Result<()> {
    let ci = ci_from_env()?;
    let dir = fs::canonicalize(dir)?;
    let tag = format!("refs/tags/{}^{{commit}}", ci.ref_name);
    let tag_commit = output(
        "git",
        &[
            "-C",
            root.to_str().ok_or("non-UTF-8 path")?,
            "rev-parse",
            &tag,
        ],
    )?;
    let work = target_dir(root).join(format!("release-publish-verify-{}", std::process::id()));
    publish(Path::new("gh"), &dir, &ci, tag_commit.trim(), &work)
}

// ---------------------------------------------------------------------------------------------
// release wait

/// One observation scope for `release wait`.
pub struct Wait {
    pub repo: String,
    pub tag: String,
    pub commit: String,
    pub run_id: Option<u64>,
    pub timeout: Duration,
    pub interval: Duration,
    pub result_file: Option<PathBuf>,
    pub work: PathBuf,
}

/// Parses `--repo R --tag T --commit SHA [--run-id N] [--timeout S] [--result-file PATH]`.
pub fn wait_args(root: &Path, args: &[String]) -> Result<Wait> {
    let mut flags = BTreeMap::new();
    for pair in args.chunks(2) {
        match pair {
            [flag, value]
                if ["--repo", "--tag", "--commit", "--run-id", "--timeout", "--result-file"]
                    .contains(&flag.as_str()) =>
            {
                if flags.insert(flag.as_str(), value.clone()).is_some() {
                    return Err(format!("{flag} given twice").into());
                }
            }
            _ => return Err("usage: cargo xtask release wait --repo OWNER/NAME --tag vX.Y.Z --commit SHA [--run-id N] [--timeout SECONDS] [--result-file PATH]".into()),
        }
    }
    let required = |flag: &str| {
        flags
            .get(flag)
            .cloned()
            .ok_or(format!("{flag} is required"))
    };
    Ok(Wait {
        repo: required("--repo")?,
        tag: required("--tag")?,
        commit: required("--commit")?,
        run_id: flags.get("--run-id").map(|v| v.parse()).transpose()?,
        timeout: Duration::from_secs(flags.get("--timeout").map_or(Ok(1800), |v| v.parse())?),
        interval: Duration::from_secs(15),
        result_file: flags.get("--result-file").map(PathBuf::from),
        work: target_dir(root).join(format!("release-wait-{}", std::process::id())),
    })
}

/// Observes one exact tag → commit → manifest → workflow run/attempt, downloads every asset the
/// manifest names and verifies it, then prints (and with `--result-file`, creates exclusively,
/// mode 0600) the result. It never installs or wakes anything: `installed` is always false.
pub fn wait(gh: &Path, w: &Wait) -> Result<()> {
    let tag_version = w.tag.strip_prefix('v').unwrap_or_default();
    if !repo_valid(&w.repo)
        || !is_hex(&w.commit, 40)
        || version_key(tag_version).is_none()
        || w.timeout.is_zero()
        || w.timeout > Duration::from_secs(86400)
    {
        return Err("invalid release wait scope or deadline".into());
    }
    let deadline = Instant::now() + w.timeout;
    let pause = || -> Result<()> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("release wait deadline exceeded".into());
        }
        thread::sleep(w.interval.min(left));
        Ok(())
    };
    let api = |path: String| gh_json(gh, &["api", "--hostname", "github.com", &path]);
    let (repo, tag, commit) = (w.repo.as_str(), w.tag.as_str(), w.commit.as_str());
    loop {
        if Instant::now() >= deadline {
            return Err("release wait deadline exceeded".into());
        }
        let Ok(release) = api(format!("repos/{repo}/releases/tags/{tag}")) else {
            pause()?;
            continue;
        };
        if release["draft"] != false {
            pause()?;
            continue;
        }
        if release["prerelease"] != tag.contains("-rc.") {
            return Err("release stable/prerelease mark does not match the tag".into());
        }
        let assets = release["assets"]
            .as_array()
            .ok_or("release asset inventory absent")?;
        let meta = assets
            .iter()
            .find(|a| a["name"] == MANIFEST)
            .ok_or("published manifest absent")?;
        if meta["size"].as_u64().is_none_or(|s| s > MANIFEST_MAX_BYTES) {
            return Err("published manifest too large".into());
        }
        let meta_id = meta["id"].as_u64().ok_or("manifest asset id missing")?;
        let bytes = output(
            gh,
            &[
                "api",
                "--hostname",
                "github.com",
                &format!("repos/{repo}/releases/assets/{meta_id}"),
                "-H",
                "Accept: application/octet-stream",
            ],
        )?;
        let m: Value = serde_json::from_str(&bytes)?;
        validate_manifest(&m)?;
        let run_id = m["workflow"]["run_id"].as_u64().unwrap_or_default();
        if m["commit"] != commit
            || m["tag"] != tag
            || m["repository"] != repo
            || w.run_id.is_some_and(|id| id != run_id)
        {
            return Err("release manifest identity mismatch".into());
        }
        let run = api(format!("repos/{repo}/actions/runs/{run_id}"))?;
        if run["head_sha"] != commit
            || run["run_attempt"] != m["workflow"]["run_attempt"]
            || run["workflow_id"] != m["workflow"]["id"]
            || run["event"] != "push"
            || run["path"] != m["workflow"]["path"]
        {
            return Err("release workflow identity mismatch".into());
        }
        if run["status"] != "completed" {
            pause()?;
            continue;
        }
        if run["conclusion"] != "success" {
            return Err("release workflow failed".into());
        }
        let mut object = api(format!("repos/{repo}/git/ref/tags/{tag}"))?["object"].clone();
        for _ in 0..4 {
            if object["type"] != "tag" {
                break;
            }
            let sha = object["sha"].as_str().ok_or("tag object sha missing")?;
            object = api(format!("repos/{repo}/git/tags/{sha}"))?["object"].clone();
        }
        if object["type"] != "commit" || object["sha"] != commit {
            return Err("tag does not resolve to the expected commit".into());
        }
        let mut patterns = vec![MANIFEST.to_owned(), "SHA256SUMS".to_owned()];
        for a in m["artifacts"].as_array().into_iter().flatten() {
            let name = a["name"].as_str().unwrap_or_default();
            if !assets
                .iter()
                .any(|x| x["name"] == name && x["size"] == a["size"])
            {
                return Err(format!("published asset {name} is absent or has another size").into());
            }
            patterns.push(name.to_owned());
        }
        fs::create_dir_all(w.work.parent().ok_or("work parent missing")?)?;
        fs::create_dir(&w.work)?;
        let work = w.work.to_str().ok_or("non-UTF-8 path")?;
        let mut download = vec!["release", "download", tag, "--repo", repo, "--dir", work];
        for p in &patterns {
            download.extend(["--pattern", p.as_str()]);
        }
        output(gh, &download)?;
        if fs::read(w.work.join(MANIFEST))? != bytes.as_bytes() {
            return Err("downloaded manifest differs from the observed one".into());
        }
        verify_manifest(&w.work)?;
        let event = json!({
            "schema_version": 1, "status": "published", "repo": repo, "tag": tag, "commit": commit,
            "run_id": run_id, "run_attempt": m["workflow"]["run_attempt"], "verified_assets": patterns,
            "artifact_integrity": "verified", "provenance_verification": "not_performed",
            "installed": false, "agent_awakened": false,
        });
        let text = serde_json::to_string(&event)?;
        if let Some(path) = &w.result_file {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        println!("{text}");
        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
    const TAG: &str = "v0.7.0";

    fn ci() -> Ci {
        Ci {
            repository: "DKotsyuba/agent-ide".to_owned(),
            ref_type: "tag".to_owned(),
            ref_name: TAG.to_owned(),
            sha: COMMIT.to_owned(),
            run_id: 42,
            run_attempt: 1,
        }
    }

    /// A payload directory: tarball stand-in, installer, product evidence bound to the tarball,
    /// the manifest and the aggregate SHA256SUMS, as `release manifest` writes them.
    fn payload(dir: &Path) -> Result<Value> {
        let bundle = asset_name(TAG);
        fs::write(dir.join(&bundle), "archive bytes")?;
        fs::write(dir.join(INSTALLER), "#!/bin/sh\n")?;
        let digest = sha256(&dir.join(&bundle))?;
        fs::write(
            dir.join(EVIDENCE),
            serde_json::to_string(
                &json!({"route": "product", "status": "product_pass", "revision": COMMIT, "payload": {"name": bundle, "sha256": digest}}),
            )?,
        )?;
        let mut m = json!({
            "schema_version": 1, "repository": "DKotsyuba/agent-ide", "repository_id": 7,
            "product": "agent-ide", "version": "0.7.0", "tag": TAG, "commit": COMMIT,
            "workflow": {"id": 9, "path": ".github/workflows/release.yml", "run_id": 42, "run_attempt": 1},
            "standard_version": "1.0.0-rc.2", "devkit_version": "0.2.0", "baseline": "b",
            "trust_profile": "github-authenticated", "artifacts": [],
        });
        for (name, kind) in [
            (bundle.as_str(), "bundle"),
            (INSTALLER, "installer"),
            (EVIDENCE, "evidence"),
        ] {
            let path = dir.join(name);
            let mut a = json!({"name": name, "kind": kind, "size": fs::metadata(&path)?.len(), "sha256": sha256(&path)?});
            if kind == "bundle" {
                a["target"] = json!(TARGET);
            }
            m["artifacts"].as_array_mut().ok_or("artifacts")?.push(a);
        }
        fs::write(dir.join(MANIFEST), serde_json::to_string_pretty(&m)?)?;
        let mut sums = String::new();
        for name in [bundle.as_str(), INSTALLER, EVIDENCE, MANIFEST] {
            sums.push_str(&format!("{}  {name}\n", sha256(&dir.join(name))?));
        }
        fs::write(dir.join("SHA256SUMS"), sums)?;
        Ok(m)
    }

    /// Writes a fake `gh` that logs its argv and answers from files in `fixtures`: API paths map
    /// to `<last path segment>.json`, `release create` copies the assets into `uploaded/`,
    /// `release download` copies them back, and `existing.txt` answers the release listing.
    fn fake_gh(fixtures: &Path) -> Result<PathBuf> {
        let script = format!(
            r#"#!/bin/sh
f='{f}'
printf '%s\n' "$*" >>"$f/log"
case "$1 $2" in
  "release create") shift 3; mkdir -p "$f/uploaded"
    for a in "$@"; do case "$a" in /*) cp "$a" "$f/uploaded/";; esac; done ;;
  "release download") while [ $# -gt 0 ]; do [ "$1" = --dir ] && dir=$2; shift; done
    cp "$f/uploaded/"* "$dir/" ;;
  "release edit") : ;;
  *) case "$*" in
       *releases\ --jq*) cat "$f/existing.txt" 2>/dev/null || : ;;
       *releases/assets/*) cat "$f/uploaded/release-manifest.json" ;;
       *) for a in "$@"; do last=$a; done; cat "$f/$(basename "$last").json" ;;
     esac ;;
esac
"#,
            f = fixtures.display()
        );
        let path = fixtures.join("gh");
        fs::write(&path, script)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }

    fn release_json(dir: &Path, draft: bool) -> Result<Value> {
        let mut assets = Vec::new();
        for (id, name) in [
            asset_name(TAG),
            INSTALLER.into(),
            EVIDENCE.into(),
            MANIFEST.into(),
            "SHA256SUMS".into(),
        ]
        .iter()
        .enumerate()
        {
            assets.push(
                json!({"id": id + 1, "name": name, "size": fs::metadata(dir.join(name))?.len()}),
            );
        }
        Ok(json!({"draft": draft, "prerelease": false, "assets": assets}))
    }

    #[test]
    fn prepare_refuses_pre_release_versions_before_touching_the_checkout() {
        let refused = prepare(Path::new("/nonexistent"), "0.7.1-rc.1", false, "2026-10-02")
            .expect_err("pre-release refused");
        assert!(
            refused
                .to_string()
                .contains("self-install refuses pre-release")
        );
    }

    #[test]
    fn version_keys_order_releases_and_candidates() {
        assert!(version_key("0.7.0") > version_key("0.6.9"));
        assert!(version_key("0.7.0") > version_key("0.7.0-rc.3"));
        assert!(version_key("0.6.10") > version_key("0.6.9"));
        for bad in ["0.7", "v0.7.0", "01.2.3", "1.2.3-beta", "1.2.3.4", ""] {
            assert!(version_key(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn lock_bump_touches_only_agent_ide_entries() {
        let lock = "[[package]]\nname = \"agent-ide\"\nversion = \"0.6.9\"\n\n[[package]]\nname = \"other\"\nversion = \"0.6.9\"\n\n[[package]]\nname = \"agent-ide-core\"\nversion = \"0.6.9\"\n";
        let (out, names) = bump_lock(lock, "0.6.9", "0.7.0");
        assert_eq!(names, ["agent-ide", "agent-ide-core"]);
        assert_eq!(out.matches("0.7.0").count(), 2);
        assert!(out.contains("name = \"other\"\nversion = \"0.6.9\""));
    }

    #[test]
    fn workspace_bump_changes_exactly_one_line() {
        let source = "[workspace]\nmembers = []\n\n[workspace.package]\nversion = \"0.6.9\"\n\n[dependencies]\nx = { version = \"0.6.9\" }\n";
        let bumped = bump_workspace_version(source, "0.6.9", "0.7.0").unwrap_or_default();
        assert_eq!(
            bumped,
            source.replacen("version = \"0.6.9\"", "version = \"0.7.0\"", 1)
        );
        assert!(bump_workspace_version(source, "9.9.9", "1.0.0").is_none());
    }

    #[test]
    fn manifest_verification_binds_files_sums_and_evidence() -> Result<()> {
        let tmp = TempDir::new("xtask-manifest")?;
        let m = payload(&tmp.0)?;
        assert_eq!(verify_manifest(&tmp.0)?, m);
        let mut extra = m.clone();
        extra["unexpected"] = json!(1);
        assert!(validate_manifest(&extra).is_err());
        let mut no_target = m.clone();
        no_target["artifacts"][0]
            .as_object_mut()
            .ok_or("bundle")?
            .remove("target");
        assert!(validate_manifest(&no_target).is_err());
        fs::write(tmp.0.join(INSTALLER), "#!/bin/sh\nexit 1\n")?;
        assert!(
            verify_manifest(&tmp.0).is_err(),
            "a changed installer must fail"
        );
        Ok(())
    }

    #[test]
    fn evidence_must_name_the_payload_digest() -> Result<()> {
        let tmp = TempDir::new("xtask-evidence")?;
        payload(&tmp.0)?;
        let evidence = json!({"route": "product", "status": "product_pass", "revision": COMMIT, "payload": {"name": asset_name(TAG), "sha256": "0".repeat(64)}});
        fs::write(tmp.0.join(EVIDENCE), serde_json::to_string(&evidence)?)?;
        assert!(verify_manifest(&tmp.0).is_err());
        Ok(())
    }

    #[test]
    fn publish_stages_verifies_then_publishes() -> Result<()> {
        let fixtures = TempDir::new("xtask-publish")?;
        let dir = fixtures.0.join("payload");
        fs::create_dir(&dir)?;
        payload(&dir)?;
        fs::write(
            fixtures.0.join(format!("{TAG}.json")),
            serde_json::to_string(&release_json(&dir, false)?)?,
        )?;
        let gh = fake_gh(&fixtures.0)?;
        publish(&gh, &dir, &ci(), COMMIT, &fixtures.0.join("verify"))?;
        let log = fs::read_to_string(fixtures.0.join("log"))?;
        let order: Vec<usize> = [
            "releases --jq",
            "release create v0.7.0",
            "release download",
            "release edit v0.7.0 --repo DKotsyuba/agent-ide --draft=false",
        ]
        .iter()
        .map(|needle| log.find(needle).unwrap_or(usize::MAX))
        .collect();
        assert!(
            order.windows(2).all(|w| w[0] < w[1]) && order[3] != usize::MAX,
            "{log}"
        );
        assert!(log.contains("--draft --generate-notes"));
        Ok(())
    }

    #[test]
    fn publish_refuses_an_existing_release_or_draft_and_wrong_identity() -> Result<()> {
        let fixtures = TempDir::new("xtask-publish-existing")?;
        let dir = fixtures.0.join("payload");
        fs::create_dir(&dir)?;
        payload(&dir)?;
        fs::write(fixtures.0.join("existing.txt"), "123\n")?;
        let gh = fake_gh(&fixtures.0)?;
        assert!(publish(&gh, &dir, &ci(), COMMIT, &fixtures.0.join("verify")).is_err());
        let mut other = ci();
        other.run_attempt = 2;
        assert!(publish(&gh, &dir, &other, COMMIT, &fixtures.0.join("verify")).is_err());
        assert!(!fs::read_to_string(fixtures.0.join("log"))?.contains("release create"));
        Ok(())
    }

    #[test]
    fn publish_leaves_the_draft_when_the_upload_differs() -> Result<()> {
        let fixtures = TempDir::new("xtask-publish-corrupt")?;
        let dir = fixtures.0.join("payload");
        fs::create_dir(&dir)?;
        payload(&dir)?;
        // The fake corrupts the uploaded installer, as a broken transfer would.
        let gh = fake_gh(&fixtures.0)?;
        let script = fs::read_to_string(&gh)?.replace(
            "cp \"$f/uploaded/\"* \"$dir/\" ;;",
            "cp \"$f/uploaded/\"* \"$dir/\"; echo x >>\"$dir/install.sh\" ;;",
        );
        fs::write(&gh, script)?;
        let error = publish(&gh, &dir, &ci(), COMMIT, &fixtures.0.join("verify"))
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(error.contains("left unpublished"), "{error}");
        assert!(!fs::read_to_string(fixtures.0.join("log"))?.contains("release edit"));
        Ok(())
    }

    fn wait_fixture(label: &str, commit: &str) -> Result<(TempDir, PathBuf, Wait)> {
        let fixtures = TempDir::new(label)?;
        let uploaded = fixtures.0.join("uploaded");
        fs::create_dir(&uploaded)?;
        payload(&uploaded)?;
        let write = |name: &str, value: Value| fs::write(fixtures.0.join(name), value.to_string());
        write(
            "42.json",
            json!({"head_sha": commit, "run_attempt": 1, "workflow_id": 9, "event": "push", "path": ".github/workflows/release.yml", "status": "completed", "conclusion": "success"}),
        )?;
        write(&format!("{TAG}.json"), release_json(&uploaded, false)?)?;
        write(
            "annotated.json",
            json!({"object": {"type": "commit", "sha": commit}}),
        )?;
        let gh = fake_gh(&fixtures.0)?;
        // `git/ref/tags/<tag>` and `releases/tags/<tag>` share the basename; the ref answer is an
        // annotated tag object resolved through `git/tags/annotated`.
        let script = fs::read_to_string(&gh)?.replace(
            "*releases/assets/*)",
            "*git/ref/tags/*) printf '%s' '{\"object\":{\"type\":\"tag\",\"sha\":\"annotated\"}}' ;;\n       *releases/assets/*)",
        );
        fs::write(&gh, script)?;
        let w = Wait {
            repo: "DKotsyuba/agent-ide".to_owned(),
            tag: TAG.to_owned(),
            commit: COMMIT.to_owned(),
            run_id: Some(42),
            timeout: Duration::from_secs(30),
            interval: Duration::from_millis(10),
            result_file: Some(fixtures.0.join("result.json")),
            work: fixtures.0.join("work"),
        };
        Ok((fixtures, gh, w))
    }

    #[test]
    fn wait_binds_tag_commit_manifest_and_run_then_writes_a_new_result() -> Result<()> {
        let (fixtures, gh, w) = wait_fixture("xtask-wait", COMMIT)?;
        wait(&gh, &w)?;
        let result = json_file(&fixtures.0.join("result.json"))?;
        assert_eq!(result["status"], "published");
        assert_eq!(result["installed"], false);
        assert_eq!(result["run_id"], 42);
        assert_eq!(
            fs::metadata(fixtures.0.join("result.json"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        // The result file is created exclusively: a second observation never overwrites it.
        let again = Wait {
            work: fixtures.0.join("work2"),
            ..w
        };
        assert!(wait(&gh, &again).is_err());
        Ok(())
    }

    #[test]
    fn wait_rejects_a_run_of_another_commit_or_run_id() -> Result<()> {
        let (_fixtures, gh, w) = wait_fixture("xtask-wait-other", &"f".repeat(40))?;
        assert!(wait(&gh, &w).is_err());
        let (_fixtures, gh, w) = wait_fixture("xtask-wait-run", COMMIT)?;
        assert!(
            wait(
                &gh,
                &Wait {
                    run_id: Some(43),
                    ..w
                }
            )
            .is_err()
        );
        Ok(())
    }
}
