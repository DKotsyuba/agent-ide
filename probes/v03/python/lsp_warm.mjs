// Warm-route cost of the existing Pyright LSP path: spawn the pinned
// pyright-langserver, complete the initialize handshake, open fixture a.py and
// measure spawn->initialized and spawn->first publishDiagnostics. Then open the
// stale-caller c.py to show that diagnostics for an unopened file arrive only
// on didOpen — the gap a v0.3 project-problem counter would have to close.
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const langserver =
  process.env.AGENT_IDE_PYRIGHT ?? "/opt/homebrew/bin/pyright-langserver";
const pkgDir = path.join(
  path.dirname(fileURLToPath(import.meta.url)),
  "fixture",
  "src",
  "pkg",
);

const t0 = Date.now();
const child = spawn(langserver, ["--stdio"], { stdio: ["pipe", "pipe", "pipe"] });
let buffer = Buffer.alloc(0);
let stage = "boot";

const send = (obj) => {
  const body = Buffer.from(JSON.stringify(obj), "utf8");
  child.stdin.write(`Content-Length: ${body.length}\r\n\r\n`);
  child.stdin.write(body);
};

const finish = (line) => {
  console.log(line);
  child.kill();
  process.exit(0);
};

setTimeout(() => finish(`lsp: TIMEOUT in stage=${stage} after 30s`), 30_000).unref();

child.stdout.on("data", (chunk) => {
  buffer = Buffer.concat([buffer, chunk]);
  for (;;) {
    const sep = buffer.indexOf("\r\n\r\n");
    if (sep < 0) return;
    const len = Number(
      /Content-Length: (\d+)/.exec(buffer.slice(0, sep).toString("utf8"))?.[1],
    );
    if (!len || buffer.length < sep + 4 + len) return;
    const body = buffer.slice(sep + 4, sep + 4 + len).toString("utf8");
    buffer = buffer.slice(sep + 4 + len);
    let msg;
    try {
      msg = JSON.parse(body);
    } catch {
      return;
    }
    if (msg.method && msg.id !== undefined) {
      // Server request: satisfy workspace/configuration with defaults, refuse the rest.
      const result =
        msg.method === "workspace/configuration"
          ? msg.params.items.map(() => null)
          : null;
      send({ jsonrpc: "2.0", id: msg.id, result });
      continue;
    }
    if (msg.id === 1) {
      stage = "initialized";
      console.log(`lsp: initialize handshake completed at ${Date.now() - t0} ms`);
      send({ jsonrpc: "2.0", method: "initialized", params: {} });
      send({
        jsonrpc: "2.0",
        method: "textDocument/didOpen",
        params: {
          textDocument: {
            uri: `file://${path.join(pkgDir, "a.py")}`,
            languageId: "python",
            version: 1,
            text: readFileSync(path.join(pkgDir, "a.py"), "utf8"),
          },
        },
      });
      stage = "awaiting-first-diagnostics";
      continue;
    }
    if (msg.method === "textDocument/publishDiagnostics") {
      const errors = msg.params.diagnostics.filter((d) => d.severity === 1).length;
      const file = msg.params.uri.split("/").pop();
      if (stage === "awaiting-first-diagnostics") {
        stage = "awaiting-c-diagnostics";
        console.log(
          `lsp: FIRST publishDiagnostics (a.py) at ${Date.now() - t0} ms (diagnostics=${msg.params.diagnostics.length}, errors=${errors})`,
        );
        send({
          jsonrpc: "2.0",
          method: "textDocument/didOpen",
          params: {
            textDocument: {
              uri: `file://${path.join(pkgDir, "c.py")}`,
              languageId: "python",
              version: 1,
              text: readFileSync(path.join(pkgDir, "c.py"), "utf8"),
            },
          },
        });
        continue;
      }
      if (stage === "awaiting-c-diagnostics" && file === "c.py") {
        finish(
          `lsp: stale c.py published only after didOpen at +${Date.now() - t0} ms (diagnostics=${msg.params.diagnostics.length}, errors=${errors}, first="${msg.params.diagnostics[0]?.message ?? "none"}")`,
        );
      }
    }
  }
});

send({
  jsonrpc: "2.0",
  id: 1,
  method: "initialize",
  params: {
    processId: process.pid,
    rootUri: `file://${path.join(pkgDir, "..", "..")}`,
    capabilities: {},
  },
});
