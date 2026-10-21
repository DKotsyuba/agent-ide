"use strict";
/**
 * Test a bounded constraint using the qualified compiler's own version-range implementation.
 * @param {typeof import('typescript')} compiler Qualified TypeScript runtime.
 * @param {unknown} requested Optional project constraint.
 * @param {string} installed Actual runtime version.
 * @returns {boolean} Whether the constraint is absent or allows the runtime; unknowns refuse.
 */
function versionMatches(compiler, requested, installed) {
  if (requested === undefined) return true;
  const range = typeof requested === "string" && requested.trim().length > 0 && requested.length <= 512
    ? compiler.VersionRange.tryParse(requested.trim()) : undefined;
  return Boolean(range && range.test(new compiler.Version(installed)));
}
