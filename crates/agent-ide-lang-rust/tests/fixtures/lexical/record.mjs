// Records rust-analyzer's `textDocument/documentSymbol` answer for every corpus file next to it
// (`<name>.rs` -> `<name>.json`), with the capabilities the product session announces
// (hierarchical document symbols, server status). The lexical corpus test in
// `src/lexical.rs` compares the lexical outline against these recordings; re-run this after
// adding or changing a corpus file, and after every rust-analyzer upgrade (the recordings are that
// build's answers, and the corpus test then decides whether the lexical outline still equals
// them):
//
//   node record.mjs "$AGENT_IDE_RUST_ANALYZER"
//
// The analyzer's `serverInfo.version` is written to `VERSION` next to the recordings.
//
// The files are opened inside a scratch Cargo package in the system temp directory, so the
// analyzer has a workspace to load; document symbols are syntax-only and do not depend on it.
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const analyzer = process.argv[2];
if (!analyzer) {
  console.error('usage: node record.mjs <rust-analyzer>');
  process.exit(2);
}
const corpus = path.dirname(fileURLToPath(import.meta.url));
const files = fs.readdirSync(corpus).filter((name) => name.endsWith('.rs')).sort();
const root = fs.mkdtempSync(path.join(os.tmpdir(), 'lexical-corpus-'));
fs.writeFileSync(
  path.join(root, 'Cargo.toml'),
  '[package]\nname = "lexical_corpus"\nversion = "0.0.0"\nedition = "2024"\n',
);
fs.mkdirSync(path.join(root, 'src'));
fs.writeFileSync(path.join(root, 'src', 'lib.rs'), '');

const server = spawn(analyzer, [], { cwd: root, stdio: ['pipe', 'pipe', 'inherit'] });
let buffer = Buffer.alloc(0);
let nextId = 0;
const waiting = new Map();
let quiescent;
const ready = new Promise((resolve) => (quiescent = resolve));
const send = (message) => {
  const text = JSON.stringify({ jsonrpc: '2.0', ...message });
  server.stdin.write(`Content-Length: ${Buffer.byteLength(text)}\r\n\r\n${text}`);
};
const request = (method, params) =>
  new Promise((resolve, reject) => {
    const id = ++nextId;
    waiting.set(id, { resolve, reject });
    send({ id, method, params });
  });
server.stdout.on('data', (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  for (;;) {
    const header = buffer.indexOf('\r\n\r\n');
    if (header < 0) return;
    const length = Number(buffer.subarray(0, header).toString().match(/Content-Length: (\d+)/i)[1]);
    if (buffer.length < header + 4 + length) return;
    const message = JSON.parse(buffer.subarray(header + 4, header + 4 + length).toString());
    buffer = buffer.subarray(header + 4 + length);
    if (message.id !== undefined && message.method === undefined) {
      const pending = waiting.get(message.id);
      waiting.delete(message.id);
      if (message.error) pending.reject(new Error(JSON.stringify(message.error)));
      else pending.resolve(message.result);
    } else if (message.method === 'experimental/serverStatus' && message.params.quiescent) {
      quiescent();
    } else if (message.id !== undefined) {
      send({ id: message.id, result: null });
    }
  }
});

const rootUri = pathToFileURL(root).href;
const initialized = await request('initialize', {
  processId: process.pid,
  rootUri,
  workspaceFolders: [{ uri: rootUri, name: 'lexical_corpus' }],
  capabilities: {
    textDocument: { documentSymbol: { hierarchicalDocumentSymbolSupport: true } },
    experimental: { serverStatusNotification: true },
  },
  initializationOptions: { cachePriming: { enable: false } },
});
send({ method: 'initialized', params: {} });
await ready;
const version = initialized.serverInfo?.version ?? 'unknown';
for (const name of files) {
  const text = fs.readFileSync(path.join(corpus, name), 'utf8');
  const uri = pathToFileURL(path.join(root, 'src', name)).href;
  send({
    method: 'textDocument/didOpen',
    params: { textDocument: { uri, languageId: 'rust', version: 1, text } },
  });
  const symbols = await request('textDocument/documentSymbol', { textDocument: { uri } });
  const target = path.join(corpus, name.replace(/\.rs$/, '.json'));
  fs.writeFileSync(target, `${JSON.stringify(symbols, null, 1)}\n`);
  console.log(`${name}: ${symbols.length} top-level symbols (rust-analyzer ${version})`);
}
fs.writeFileSync(path.join(corpus, 'VERSION'), `${version}\n`);
await request('shutdown', null);
send({ method: 'exit' });
fs.rmSync(root, { recursive: true, force: true });
