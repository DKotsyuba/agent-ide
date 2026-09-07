# async-lsp client probe

`cargo run --example lsp_client_probe -- [gopls-executable] [fixture-directory]` starts one fresh `gopls serve` over pipes. The executable defaults to `gopls` resolved through `PATH`; the fixture defaults to `std::env::temp_dir()/agent-ide-lsp-probe-<pid>`. It atomically creates that directory and refuses an existing candidate. It uses disposable `GOCACHE`, `GOMODCACHE`, and `GOPATH` directories, sends `initialize`, `initialized`, and `textDocument/didOpen`, then requires both hover text containing `ProbeTarget` and a definition targeting its declaration. It sends `shutdown` and `exit`, stops its own `async-lsp` loop, waits for normal successful child exit with a deadline, kills only after that deadline, reaps the child, and deletes only that module.

The probe imports LSP types from `async_lsp::lsp_types`; it does not add a separately versioned `lsp-types` dependency. It is a D04 candidate instrument only. It is not a production broker and establishes no D01 or D03 behaviour.

## Bounded evidence

`async-lsp` 0.2.4 supplies `MainLoop::new_client` and `run_buffered` for the stdio transport. Its documented `concurrency` middleware can limit concurrent incoming requests and abort an incoming request after `$/cancelRequest`; this probe does not layer it or send cancellation, so cancellation is untested. `Router` supports request, notification, and event callbacks. The probe registers its local `Stop` event to terminate the client loop and records otherwise-unhandled server notifications while continuing the loop. A successful run prints the methods actually received; the recorded run received `window/showMessage`, `window/logMessage`, and `textDocument/publishDiagnostics`. It does not validate their payloads or implement a typed server callback. `MainLoop::run` documents I/O, deserialization, and protocol errors, including EOF from its input, but this probe ends its loop through `Stop` rather than asserting a particular EOF result.

No application frame-size cap is configured or tested here. `async-lsp` owns the LSP framing in `MainLoop`; any production frame cap, callback policy, cancellation policy, or EOF recovery remains D04-open.
