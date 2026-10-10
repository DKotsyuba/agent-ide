# Start index state v1 contract

Revision: start-index-state r3; tracked boundary id `start-index-state`, revision 3. Provider:
activation, writer-lease takeover and language-server state (`assistance/worker.rs` activation,
`project/mod.rs` servers line, the symbol tools' loading replies). Consumer: the teaching and
reply-hygiene surface (start reply layout and budget, signature block, skill, server instructions,
tool descriptions, the no-poll guard test). Related: [symbol and graph item v1](symbol-graph-item-v1.md),
[tools v0.4](tools-v0.4.md).

Purpose: the start reply tells an agent whether navigation answers are ready, and every text
about loading servers or held worktrees is fixed here, so the consumer can teach it and guard it.

## 1. Start reply layout

- Order: the activation line, then the project card with the servers line inside it, then the
  consumer's signature block.
- The provider owns the activation line and the card. The consumer keeps the whole start reply
  within its byte budget.
- The servers line is at most 360 bytes (r3; r1 and r2 said 300, which three server languages exceed).
- Servers are started lazily: `ide.start` launches no language server. The first `ide.symbol` or
  `ide.graph` call of a language starts it.

## 2. Servers line

`servers: <language> <state>[ · <language> <state>]`: one entry per detected language that has a
server, in card order. The state is read from the worktree's live session at render time; nothing
is probed or started for the card.

| State (exact text) | When |
|---|---|
| `ready` | A live session of this worktree answers semantic requests now. |
| `warming; symbol and graph answer approximately until ready` | Rust (exact source outline) while its session loads. |
| `warming; symbol and graph answers follow as facts until ready` | Any other language while its session loads. |
| `not started; outline, read and edit answer from source; the first symbol or graph call starts it and answers approximately at once` | Rust, no session yet. |
| `not started; the first symbol or graph call starts it and its answer follows as a fact` | Any other language, no session yet. |
| `unavailable (<cause>)` | The worktree's last session failed. Causes: `workspace failed to load`, `transport gone`, `no launch configured`. |

The 0.10.8 phrase `wait for the server, which starts on their first use` is retired.

## 3. Loading replies

The provider owns this wording. The consumer's no-poll guard covers it.

- A call that still meets a loading server outside the approximate path (rename; an outline,
  read or symbol edit of a language without an exact source outline; an address missing from a
  lexical outline) answers:
  `error: provider_loading (<detail>); the language server is still loading the workspace, so this call was not answered and nothing was changed. Continue with other work or native tools; ide.symbol and ide.graph answer at once meanwhile`
- The 0.10.8 callers note `callers are not available yet because the language server is still indexing; repeat ide.symbol later`
  is retired. A remaining case reads `callers: unavailable (<server> still indexing)`.
- The provider rewrites the loading statements of `docs/contracts/tools-v0.4.md` (§0 loading
  paragraph) and of the skill (the `provider_loading`, lexical outline and `ide.graph` waits
  paragraph) to this contract's behaviour. Every other part of the skill stays with the consumer.

## 4. Held and taken-over worktrees (exact texts)

The writer slot of a worktree is taken over at `ide.start` only from a holder that is gone:

- its MCP connection closed and nothing arrived from it afterwards (`its session ended`); or
- it has no tool call and no IDE work in flight, and nothing arrived from it for 30 minutes
  (`inactive for <m> min`).

A live holder is never displaced.

a. Successful takeover, the start reply's first line:

```text
activated: epoch <n>; mode: writer; took over the writer from actor <actor> (activation <id>, last activity <time>; <evidence>); baseline: <baseline>
```

`<evidence>` is `its session ended` or `inactive for <m> min`.

b. Refusal while the holder is live (replaces the 0.10.8 text of `start:worktree_held_by_another_actor` with holder facts):

```text
error: conflict (start:worktree_held_by_another_actor: actor <actor> (activation <id>, writer, since <time>, last activity <time>)); another agent's live session holds the only writer slot. Start with {"read_only": true} to read alongside now; the slot frees when that session ends or after 30 min without activity
```

c. The displaced actor's next call. New closed host-binding cause `taken_over`:

```text
unavailable: host_binding (taken_over); another agent took over this worktree's IDE activation <reason>. Call ide.start ({"read_only": true} while the other agent writes), or continue with native tools
```

`<reason>` is `after this session's IDE connection closed` or `after <m> min without activity`.

d. Same-actor conflicts (`worktree_held_by_this_actor`, `actor_owns_another_worktree`) and the
same-actor handover keep their 0.10.8 texts.

## 5. Teaching facts

The consumer teaches these:

1. `ide.symbol` and `ide.graph` never leave the agent waiting. They answer precisely; or
   approximately (Rust while indexing, rows marked `~`); or `pending` (other languages while their
   server starts). The precise answer then arrives by itself as a fact.
2. Before adding a field or changing a constructor, `ide.symbol {"symbol": "<Type>"}` lists the
   construction sites tagged `construction ~`. Before changing a signature,
   `ide.graph {"symbol": "<fn>", "direction": "callers"}` lists the call sites.
3. A bare name with one definition resolves directly. A full address is needed only when the
   name repeats.
4. A worktree left by a finished or crashed agent is taken over by the next `ide.start`. Calling
   `ide.stop` at the end is still required: it frees the writer at once and returns undelivered
   facts.
5. `callers: unavailable (<cause>)` means the server failed that request, not that nothing calls
   the symbol.

## 6. Compatibility checks

Provider:

1. A card render test covers each §2 state.
2. Tests render texts §4a-c from a takeover and from a refusal.
3. A no-poll check runs over the strings of §2-§4.

Consumer:

1. Its no-poll guard test includes these strings.
2. The start reply stays within its budget with the servers line present.
