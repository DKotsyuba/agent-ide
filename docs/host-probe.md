# Host metadata probe

Implementation status: incomplete. The current example compiles, but MCP metadata capture, strict log bounds, privacy projection and executable checks are not yet accepted.

This test instrument is not the Agent IDE product API or a supported release. It establishes which non-secret metadata a real host supplies to MCP requests and hook events before the application relies on actor identity or execution authority.

The `host_probe` example has two modes: `mcp` serves the single diagnostic tool `probe_observe` over local stdio using the official `rmcp` SDK; `hook` reads one bounded JSON input from stdin and exits successfully without model-facing output. The diagnostic tool accepts no identity/authority arguments and returns `binding_unproven`. It does not expose placeholder `ide.*` tools, create activation, touch repository source, start a daemon/provider, or schedule other processes.

A caller-selected private observation file receives bounded records of field paths/types and keyed cryptographic fingerprints of scalar values. The key is supplied in a separate local fixture file. Raw source, arguments, prompts, credentials, authorization headers, environment dumps, and raw scalar values never enter the observation log. Malformed or oversized hook input records only a bounded marker, then exits successfully. Limit recursion, field count, input bytes and log growth; failing to write observations must not block the host. Ordinary stdio protocol EOF ends the MCP mode cleanly.

The control test launches a real host parent and two native subagents with identical diagnostic tool arguments. Compare host-origin metadata between the MCP and hook paths. Parent session IDs, CWD, PID/UID, model inputs, timestamps and proximity alone do not establish actor authority. A positive binding claim requires a documented host-origin relation for the actor, specific invocation and delivery channel, validated with adversarial cross-actor cases. Missing evidence remains `binding_unproven`; the recorder must never automatically infer or grant authority.

Separately, any later daemon/provider launch requires real proof that the chosen execution profile enforces the actual host sandbox contract, including an allowed fixture operation and a genuinely denied operation. This recorder does not perform that launch or claim sandbox propagation.

Verification covers a real stdio MCP handshake, exact one-tool discovery, a call returning unproven binding, clean EOF, and a hook valid/malformed/oversized-input table. Log tests check that secret marker strings are absent and limits hold. Real Codex/Claude host observation is separate from those protocol tests and is recorded with exact host/OS versions.
