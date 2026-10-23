# Test runner parallelism

The launcher accepts an optional restart-only section:

```json
"test_execution": { "workers": 4 }
```

Use 1..256 or `"max"`. Omission retains project defaults. This controls generated
`ide.test` selections, not development `cargo xtask check`, background type checks
or explicit custom test commands. One live IDE run per worktree remains; the
engine distributes its tests. Shared resources still require test isolation.

`max` uses available logical CPUs minus half the one-minute OS load, rounded
up: `CPUs - ceil(load / 2)`, bounded to 1..256. The smoothed load is discounted
because it is not a direct count of currently occupied CPUs; no extra CPU is reserved. Missing samples use one. Existing runs
are not resized. This is a per-invocation estimate, not a global CPU reservation,
memory limit or guarantee of optimal performance.

Rust receives `RUST_TEST_THREADS`; explicit runner CLI arguments retain precedence.
Python receives `PYTEST_ADDOPTS=-n N` and requires `pytest-xdist` in the selected
project interpreter. IDE does not install it automatically. A missing plugin
returns the actual runner error, without a silent serial fallback.
Vitest/Jest receive `--maxWorkers=N`; Node receives `--test-concurrency=N`.
Versions must support those options. Actual command/environment replay records
the selected count. Projects that require serial tests must retain that policy.

Module mode admits the setting through declared recipes before execution;
native mode applies the same language-owned option to the configured tool.
