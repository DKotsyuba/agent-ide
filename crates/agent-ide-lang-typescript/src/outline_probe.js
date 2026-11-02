// Source-only outline probe: TypeScript's own navigation tree of the source read from stdin,
// converted to LSP DocumentSymbols the way typescript-language-server converts tsserver's
// navtree (its collectDocumentSymbols, shouldIncludeEntry and symbolKindsMapping, lib/cli.mjs).
// argv: [absolute typescript.js, file name]. A syntactic language service over one in-memory
// snapshot: no program, project, configuration, type package or node_modules is read.
// Exit 3: the compiler module did not load; 5: it has no syntactic mode; 4: any other failure.
let ts;
try {
  ts = require(process.argv[1]);
} catch (error) {
  process.exit(3);
}
if (!ts.LanguageServiceMode || ts.LanguageServiceMode.Syntactic === undefined) {
  process.exit(5);
}
const kinds = {
  'enum member': 14, 'JSX attribute': 7, 'local class': 5, 'local function': 12,
  'local var': 13, 'type parameter': 13, alias: 13, class: 5, const: 14, constructor: 9,
  enum: 10, field: 8, file: 1, function: 12, getter: 6, interface: 11, let: 13, method: 6,
  module: 2, parameter: 13, property: 7, setter: 6, var: 13,
};
const chunks = [];
process.stdin.on('data', (chunk) => chunks.push(chunk)).on('end', () => {
  try {
    const text = Buffer.concat(chunks).toString('utf8');
    const argument = process.argv[2] || 'source.ts';
    if (argument.startsWith('{')) {
      const request = JSON.parse(argument);
      if (typeof request.file !== 'string' || !Array.isArray(request.anchors)
        || !request.anchors.length || request.anchors.length > 32
        || request.anchors.some((at) => !Number.isSafeInteger(at) || at < 1)) process.exit(4);
      const file = ts.createSourceFile(request.file, text, ts.ScriptTarget.Latest, true);
      const context = file.parseDiagnostics.length ? null : classContext(ts, file, request.anchors.map((at) => at - 1));
      process.stdout.write(JSON.stringify({v: 1, context}) + '\n');
      return;
    }
    const name = '/__agent_ide_outline__/' + argument;
    const snapshot = ts.ScriptSnapshot.fromString(text);
    const host = {
      getCompilationSettings: () => ({ noResolve: true, noLib: true, types: [], allowJs: true }),
      getScriptFileNames: () => [name],
      getScriptVersion: () => '1',
      getScriptSnapshot: (file) => (file === name ? snapshot : undefined),
      getCurrentDirectory: () => '/__agent_ide_outline__',
      getDefaultLibFileName: () => '/__agent_ide_outline__/lib.d.ts',
      fileExists: (file) => file === name,
      readFile: (file) => (file === name ? text : undefined),
      directoryExists: () => false,
      getDirectories: () => [],
    };
    const service = ts.createLanguageService(
      host, ts.createDocumentRegistry(), ts.LanguageServiceMode.Syntactic);
    const tree = service.getNavigationTree(name);
    const lines = ts.createSourceFile(name, text, ts.ScriptTarget.Latest, false);
    const position = (offset) => {
      const at = lines.getLineAndCharacterOfPosition(offset);
      return { line: at.line, character: at.character };
    };
    const range = (span) => ({ start: position(span.start), end: position(span.start + span.length) });
    const before = (a, b) => a.line < b.line || (a.line === b.line && a.character < b.character);
    const intersects = (a, b) => {
      const start = before(a.start, b.start) ? b.start : a.start;
      const end = before(a.end, b.end) ? a.end : b.end;
      return !before(end, start);
    };
    const include = (item) => item.kind !== 'alias'
      && !!(item.text && item.text !== '<function>' && item.text !== '<class>');
    const collect = (parent, symbols, bounds) => {
      let included = include(parent);
      for (const span of parent.spans) {
        const spanRange = range(span);
        if (!intersects(bounds, spanRange)) {
          continue;
        }
        const children = [];
        for (const child of parent.childItems || []) {
          if (child.spans.some((childSpan) => intersects(spanRange, range(childSpan)))) {
            included = collect(child, children, spanRange) || included;
          }
        }
        let selectionRange = spanRange;
        if (parent.nameSpan) {
          const nameRange = range(parent.nameSpan);
          if (intersects(spanRange, nameRange)) {
            selectionRange = nameRange;
          }
        }
        if (included) {
          symbols.push({
            name: parent.text, detail: '', kind: kinds[parent.kind] || 13,
            range: spanRange, selectionRange, children,
          });
        }
      }
      return included;
    };
    const symbols = [];
    for (const item of tree.childItems || []) {
      collect(item, symbols, {
        start: range(item.spans[0]).start,
        end: range(item.spans[item.spans.length - 1]).end,
      });
    }
    process.stdout.write(JSON.stringify({ v: 1, symbols }) + '\n');
  } catch (error) {
    process.exit(4);
  }
});
