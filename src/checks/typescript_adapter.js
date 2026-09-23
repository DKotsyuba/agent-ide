/**
 * Run the pinned TypeScript compiler with filesystem reads checked at the call site.
 * The Rust checker embeds this file in its private cache and discards all output on exit 77.
 */
"use strict";

/** @type {typeof import('node:fs')} Node filesystem used only behind the guards below. */
const fs = require("node:fs");
/** @type {typeof import('node:path')} Native path handling for absolute compiler paths. */
const path = require("node:path");
/** @type {Array<{Path?: string, Glob?: {base: string, suffix: string}}>} Host read exclusions. */
const denies = JSON.parse(process.env.CHECK_READ_DENIES);
/** @type {string[]} Explicit compiler read and cache grants from the confined RunSpec. */
const readRoots = JSON.parse(process.env.CHECK_READ_ROOTS).map((root) => path.resolve(root));
/** @type {string} Private directory permitted to receive TypeScript build metadata. */
const cache = path.resolve(process.env.CHECK_CACHE);
/** @type {string} Pinned TypeScript entry used to locate its sibling compiler library. */
const cli = process.env.CHECK_TSC_CLI;
/** @type {typeof import('typescript')} The verified sibling of the configured pinned CLI. */
let ts;
try {
  ts = require(path.join(path.dirname(cli), "typescript.js"));
  if (ts.version !== "5.9.3" || typeof ts.executeCommandLine !== "function" ||
      typeof ts.matchFiles !== "function") throw new Error("unsupported compiler");
} catch { process.exit(1); }
/** @type {typeof ts.sys.readFile} Pinned decoder, including BOM and UTF-16 handling. */
const originalReadFile = ts.sys.readFile.bind(ts.sys);
/** @type {typeof ts.sys.writeFile} Pinned writer, including byte-order-mark handling. */
const originalWriteFile = ts.sys.writeFile.bind(ts.sys);
/** @type {boolean} Sticky proof that one compiler filesystem operation was refused. */
let readRestricted = false;

/**
 * Compare a path to a host exclusion without filesystem access.
 * @param {string} value Absolute candidate path, possibly containing a denied component.
 * @returns {boolean} Whether any configured path or credential glob excludes it.
 */
function denied(value) {
  if (!/^[\x00-\x7f]*$/.test(value)) return denies.length !== 0;
  const candidate = path.resolve(value).toLowerCase();
  return denies.some((rule) => {
    if (rule.Path !== undefined) {
      const root = path.resolve(rule.Path).toLowerCase();
      return candidate === root || candidate.startsWith(root + path.sep);
    }
    const root = path.resolve(rule.Glob.base).toLowerCase();
    if (candidate !== root && !candidate.startsWith(root + path.sep)) return false;
    const parts = path.relative(root, candidate).split(path.sep);
    return parts.some((name) => {
      switch (rule.Glob.suffix) {
        case "Key": return name.endsWith(".key");
        case "Pem": return name.endsWith(".pem");
        case "Env": return name === ".env";
        case "EnvDot": return name.startsWith(".env.");
        default: throw new Error("unsupported read exclusion");
      }
    });
  });
}

/**
 * Require compiler operations to stay within an explicit runner grant.
 * @param {string} value Absolute path after lexical or symlink resolution.
 * @returns {boolean} Whether the path is an admitted read root or descendant.
 */
function granted(value) {
  const candidate = path.resolve(value);
  return readRoots.some((root) => candidate === root || candidate.startsWith(root + path.sep));
}

/**
 * Resolve each symlink without probing its target until the target passes the deny policy.
 * Missing paths remain lexical so ordinary fileExists probes can return false.
 * @param {string} input Compiler-requested absolute or cwd-relative path.
 * @returns {string | undefined} Resolved allowed path, or undefined after recording refusal.
 */
function allowed(input) {
  let pending = path.resolve(input);
  for (let links = 0; links < 40; links++) {
    if (denied(pending) || !granted(pending)) { readRestricted = true; return undefined; }
    const parts = pending.slice(1).split(path.sep).filter(Boolean);
    let prefix = path.sep;
    let changed = false;
    for (let i = 0; i < parts.length; i++) {
      prefix = path.join(prefix, parts[i]);
      if (denied(prefix)) { readRestricted = true; return undefined; }
      let meta;
      try { meta = fs.lstatSync(prefix); }
      catch (error) {
        if (error.code === "ENOENT" || error.code === "ENOTDIR") return pending;
        readRestricted = true;
        return undefined;
      }
      if (meta.isSymbolicLink()) {
        let target;
        try { target = fs.readlinkSync(prefix); }
        catch { readRestricted = true; return undefined; }
        pending = path.resolve(path.dirname(prefix), target, ...parts.slice(i + 1));
        changed = true;
        break;
      }
    }
    if (!changed) return pending;
  }
  readRestricted = true;
  return undefined;
}

/**
 * Convert an ordinary missing-path probe into undefined and all other I/O failures into refusal.
 * @param {unknown} error Filesystem exception.
 * @returns {undefined} Sentinel for guarded System methods.
 */
function failed(error) {
  if (error.code !== "ENOENT" && error.code !== "ENOTDIR") readRestricted = true;
  return undefined;
}

/**
 * Enumerate one directory for TypeScript's exported matchFiles walker.
 * Names alone are not reads of their contents; selected entries later pass through allowed().
 * @param {string} directory Directory selected by TypeScript's include/exclude matcher.
 * @returns {{files: string[], directories: string[]}} Immediate child names by kind.
 */
function entries(directory) {
  const resolved = allowed(directory);
  if (!resolved) return { files: [], directories: [] };
  let children;
  try { children = fs.readdirSync(resolved, { withFileTypes: true }); }
  catch (error) { failed(error); return { files: [], directories: [] }; }
  const files = [];
  const directories = [];
  for (const child of children) {
    if (child.isFile()) files.push(child.name);
    else if (child.isDirectory()) directories.push(child.name);
    else if (child.isSymbolicLink()) {
      const target = allowed(path.join(directory, child.name));
      if (!target) continue;
      try {
        const meta = fs.statSync(target);
        if (meta.isDirectory()) directories.push(child.name);
        else if (meta.isFile()) files.push(child.name);
      } catch (error) { failed(error); }
    } else readRestricted = true;
  }
  return { files, directories };
}

/**
 * Admit a write only under the private cache, including through symlinks.
 * @param {string} input Compiler-requested output path.
 * @returns {string | undefined} Safe resolved path or undefined after refusal.
 */
function writable(input) {
  const result = allowed(input);
  if (!result || (result !== cache && !result.startsWith(cache + path.sep))) {
    readRestricted = true;
    return undefined;
  }
  return result;
}

/** @type {typeof ts.sys} Compiler host with guarded reads, enumeration, and writes. */
const system = Object.assign(ts.sys, {
  ...ts.sys,
  /** @param {string} file Requested file. @returns {string | undefined} File text or absence. */
  readFile(file) {
    const target = allowed(file);
    if (!target) return undefined;
    try {
      const text = originalReadFile(target);
      if (text === undefined) {
        try { if (fs.statSync(target).isFile()) readRestricted = true; }
        catch (error) { failed(error); }
      }
      return text;
    }
    catch (error) { return failed(error); }
  },
  /** @param {string} file Requested file. @returns {boolean} Whether it exists and is regular. */
  fileExists(file) {
    const target = allowed(file);
    if (!target) return false;
    try { return fs.statSync(target).isFile(); }
    catch (error) { failed(error); return false; }
  },
  /** @param {string} dir Requested directory. @returns {boolean} Whether it exists. */
  directoryExists(dir) {
    const candidate = path.resolve(dir);
    if (denied(candidate)) { readRestricted = true; return false; }
    // Every ancestor of an admitted root exists by construction; TypeScript probes them.
    if (candidate === path.sep || readRoots.some((root) => root.startsWith(candidate + path.sep))) return true;
    const target = allowed(dir);
    if (!target) return false;
    try { return fs.statSync(target).isDirectory(); }
    catch (error) { failed(error); return false; }
  },
  /** @param {string} dir Requested directory. @returns {string[]} Immediate child directories. */
  getDirectories(dir) { return entries(dir).directories; },
  /**
   * @param {string} root Search root.
   * @param {string[] | undefined} extensions Source extensions.
   * @param {string[] | undefined} excludes Excluded patterns.
   * @param {string[] | undefined} includes Included patterns.
   * @param {number | undefined} depth Maximum search depth.
   * @returns {string[]} Files matched by TypeScript's own traversal semantics.
   */
  readDirectory(root, extensions, excludes, includes, depth) {
    return ts.matchFiles(root, extensions, excludes, includes,
      ts.sys.useCaseSensitiveFileNames, process.cwd(), depth, entries, system.realpath);
  },
  /** @param {string} file Alias path. @returns {string} Resolved path, or original after refusal. */
  realpath(file) { return allowed(file) || file; },
  /** @param {string} file Requested file. @returns {Date | undefined} Modification time or absence. */
  getModifiedTime(file) {
    const target = allowed(file);
    if (!target) return undefined;
    try { return fs.statSync(target).mtime; }
    catch (error) { return failed(error); }
  },
  /** @param {string} file Output path. @param {string} data Text to write. @param {boolean} bom Whether to emit a BOM. @returns {void} */
  writeFile(file, data, bom) {
    const target = writable(file);
    if (!target) return;
    try { originalWriteFile(target, data, bom); }
    catch { readRestricted = true; }
  },
  /** @param {string} dir Output directory. @returns {void} */
  createDirectory(dir) {
    const target = writable(dir);
    if (!target) return;
    try { fs.mkdirSync(target, { recursive: true }); }
    catch { readRestricted = true; }
  },
  /** @param {string} file Output path to delete. @returns {void} */
  deleteFile(file) {
    const target = writable(file);
    if (!target) return;
    try { fs.unlinkSync(target); }
    catch (error) { failed(error); }
  },
  /** @param {number} code Compiler exit status. @returns {void} Sets refusal precedence without truncating buffered output. */
  exit(code) { process.exitCode = readRestricted ? 77 : code; },
});

try {
  ts.executeCommandLine(system, () => {}, process.argv.slice(2));
  if (readRestricted) process.exitCode = 77;
} catch {
  process.exitCode = readRestricted ? 77 : 1;
}
