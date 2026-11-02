// Pure, source-only class context selection. The caller supplies its admitted compiler;
// this module performs no filesystem, project, network or type-resolution operations.

/** Select complete instance methods plus their syntactic callers/callees and all surrounding
 * source. Unknown receiver escapes or dynamic access in kept code refuse narrowing. Line
 * spans are zero-based inclusive; null means retain the original context. */
function classContext(ts, file, anchors) {
  const classes = file.statements.filter(ts.isClassDeclaration);
  const line = (offset) => file.getLineAndCharacterOfPosition(offset).line;
  const owner = classes.find((node) => anchors.length > 0 && anchors.every((at) =>
    line(node.getStart(file)) <= at && at <= line(node.end - 1)));
  if (!owner || owner.heritageClauses?.length || ts.getDecorators(owner)?.length) return null;
  const methods = owner.members.filter((node) => (ts.isMethodDeclaration(node)
    || (ts.isPropertyDeclaration(node) && node.initializer && ts.isArrowFunction(node.initializer)))
    && (ts.isIdentifier(node.name) || ts.isPrivateIdentifier(node.name))
    && !node.modifiers?.some((m) => m.kind === ts.SyntaxKind.StaticKeyword)
    && !ts.getDecorators(node)?.length);
  const kept = new Set();
  for (const at of anchors) {
    const method = methods.find((node) => line(node.getStart(file)) <= at && at <= line(node.end - 1));
    if (!method) return null;
    kept.add(method.name.text);
  }
  /** Collect property names, refusing unresolved access through this or an escaping receiver. */
  function references(root, skip) {
    const names = new Set();
    let unknown = false;
    function visit(node) {
      if (skip.has(node)) return;
      if (ts.isPropertyAccessExpression(node)) names.add(node.name.text);
      if (node.kind === ts.SyntaxKind.ThisKeyword
        && !(ts.isPropertyAccessExpression(node.parent) && node.parent.expression === node)) {
        const binding = ts.isVariableDeclaration(node.parent) && node.parent.initializer === node
          && ts.isObjectBindingPattern(node.parent.name) ? node.parent.name : null;
        if (!binding || binding.elements.some((element) => element.dotDotDotToken
          || !ts.isIdentifier(element.propertyName || element.name))) unknown = true;
        else for (const element of binding.elements) names.add((element.propertyName || element.name).text);
      }
      ts.forEachChild(node, visit);
    }
    visit(root);
    return {names, unknown};
  }
  // Every retained field, initializer, constructor, accessor and module statement participates
  // in the forward closure. Reverse edges retain wrappers calling the affected method.
  for (;;) {
    const dropped = new Set(methods.filter((node) => !kept.has(node.name.text)));
    const current = references(file, dropped);
    if (current.unknown) return null;
    const names = current.names;
    let added = false;
    for (const node of methods) {
      if (kept.has(node.name.text)) continue;
      const calls = references(node, new Set());
      if (names.has(node.name.text) || [...calls.names].some((name) => kept.has(name))) {
        kept.add(node.name.text);
        added = true;
      }
    }
    if (!added) break;
  }
  const lines = file.text.split('\n');
  if (lines[lines.length - 1] === '') lines.pop();
  const removed = new Set();
  for (const node of methods) {
    if (kept.has(node.name.text)) continue;
    const start = Math.min(node.getStart(file), ...(node.jsDoc || []).map((doc) => doc.getStart(file)));
    const end = node.end;
    const first = line(start), last = line(end - 1);
    // Never remove a neighboring declaration sharing a physical line.
    const firstOffset = file.getPositionOfLineAndCharacter(first, 0);
    const lastOffset = file.getPositionOfLineAndCharacter(last, 0);
    if (file.text.slice(firstOffset, start).trim() || lines[last].slice(end - lastOffset).trim()) return null;
    for (let at = first; at <= last; at++) removed.add(at);
  }
  if (!removed.size) return null;
  const spans = [];
  for (let at = 0; at < lines.length; at++) {
    if (removed.has(at)) continue;
    const previous = spans[spans.length - 1];
    if (previous && previous[1] + 1 === at) previous[1] = at;
    else spans.push([at, at]);
  }
  return spans;
}
module.exports = classContext;
