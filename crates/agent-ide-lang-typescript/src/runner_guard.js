/** Fixed Node prelude: refuse an incompatible environment before the CLI loads as main. */
import { createRequire } from "node:module";
const require = createRequire(process.cwd() + "/agent-ide-runner.cjs");
try {
  const requirements = JSON.parse(process.env.AGENT_IDE_RUNNER_REQUIREMENTS);
  if (!Array.isArray(requirements)) throw new Error("requirements");
  if (requirements.length !== 0) {
    const ts = require(process.env.AGENT_IDE_RUNNER_TYPESCRIPT);
    if (ts.version !== "5.9.3" || process.versions.node !== process.env.AGENT_IDE_RUNNER_NODE_IDENTITY ||
        !requirements.every((pin) => versionMatches(ts, pin, process.versions.node))) throw new Error("incompatible");
  }
} catch {
  process.stderr.write("error: agent-ide test environment unavailable: Node requirement is unsupported, incompatible or unverifiable; prepare a matching configured runtime.\n");
  process.exit(78);
}
