# Plugin ABI

One native runner (agentplug-runner) links wasmtime, compiles each plugin's
wasm module once, instantiates one `Store`/`Instance` per (project, plugin)
pair. Plugins never call each other's wasm exports directly -- wasm-to-wasm
calls are not a thing; every cross-plugin call is host-mediated: caller
plugin invokes a host import, the host copies args out of the caller's
linear memory, looks up the target plugin's instance, calls its export,
copies the result back into the caller's linear memory.

## Wire format (unchanged from the existing plugkit-core/gm-runner convention)

- String/bytes in: `(ptr: u32, len: u32)` pair, read via `Memory::read` into
  a host-owned `Vec<u8>`.
- String/bytes out: host allocates in the CALLER's memory via the caller's
  own exported `plugkit_alloc(len: u32) -> u32`, writes the bytes, returns
  `(ptr as u64 & 0xffff_ffff) | (len as u64) << 32`. Zero return = empty/none.
- Fixed-size numeric out-buffers (e.g. embeddings): caller passes
  `(out_ptr: u32, out_len: u32)`, host writes directly into caller memory,
  return value is either the actual written length or a negative/zero
  sentinel on failure -- exact shape of the existing `host_vec_embed`.
- JSON payloads: always the bytes-out convention above, `serde_json::Value`
  serialized to a string first. No binary JSON encoding.

## Exports every plugin module provides

- `memory` (wasm linear memory, implicit export)
- `plugkit_alloc(len: u32) -> u32`
- `plugkit_free(ptr: u32, len: u32)` (optional; host tolerates absence)
- `plugin_call(verb_ptr: u32, verb_len: u32, body_ptr: u32, body_len: u32) -> u64`
  single dispatch entrypoint, same shape as plugkit-core's existing
  `dispatch_verb`. `verb` names the capability being invoked; `body` is a
  JSON string.

## Host imports every plugin module can use (module `env`)

Identical set to `wasm_host.rs::register_env_imports` today
(`host_fs_*`, `host_log`, `host_now_ms`, `host_env_get`, `host_fetch`,
`host_kv_*`, `host_exec_js`, `host_git`) PLUS one new one:

- `host_plugin_call(plugin_ptr, plugin_len, verb_ptr, verb_len, body_ptr, body_len) -> u64`
  routes to another loaded plugin's `plugin_call` export. `plugin` is the
  target plugin's registered name (e.g. `"bert"`, `"libsql"`,
  `"treesitter"`). Host looks up that plugin's `Instance` for the SAME
  project the calling instance belongs to (plugins are per-project
  instantiated, same as gm.wasm itself), marshals body in, calls
  `plugin_call`, marshals the result back into the CALLING plugin's memory.
  Missing plugin / not-loaded-for-this-project / trap in the callee all
  collapse to the standard `{"ok":false,"error":"..."}` envelope written
  into the caller's memory -- never a host-side panic.

`host_vec_embed` stays as a convenience wrapper implemented in terms of
`host_plugin_call("bert", "embed", ...)` -- existing callers (plugkit-core's
embed.rs) need zero source changes, since the extern signature is unchanged;
only the host-side implementation swaps from native candle to a routed
wasm-to-wasm call.

## Capability discovery

Each plugin's manifest (`agentplug.toml` at its wasm build root, embedded
into the release asset alongside the `.wasm` file as `<name>.manifest.json`)
declares `{"name": "...", "verbs": ["embed", "parse", "query", ...]}`.
agentplug-runner reads every registered project's plugin list from
`.agentplug/plugins.txt` (one plugin name per line, same registry-file
pattern as gm-runner's existing `daemon-registry.txt`) and loads exactly
those; a `plugin_call` naming an unregistered verb returns
`{"ok":false,"error":"unknown_verb"}` from that plugin's own dispatch, not
a host-level failure.

## Crawl contract (host side)

Two engines serve one request shape. `crawl_cdp` is host-side Rust in
`agentplug-host`, imported by the guest as `env.crawl_cdp`. The lightpanda engine
is the wasm sibling `lightpanda.wasm` (`crates/agentplug-lightpanda-wasm`): its
`crawl` verb calls the import `env.host_lightpanda_crawl`, which the runner
answers with `agentplug_lightpanda::crawl`, the warm native server.

Entry points:

- `agentplug_host::crawl_cdp(cwd, body) -> Value` -- headful Chrome over CDP.
  One Chrome per call, killed when the call returns. Chrome comes from
  GM_BROWSER_CHROME_PATH, then CHROME_PATH, then the per-platform install paths
  (Windows install roots, macOS `.app` bundles, Linux packages) and PATH.
- `agentplug_host::set_lightpanda_engine(agentplug_lightpanda::crawl)` -- the
  runner registers the native engine at startup; the import is refused until then.
- `agentplug_lightpanda::crawl(cwd, body)` -- headless lightpanda, one warm
  `serve` per project root, reaped after GM_LIGHTPANDA_IDLE_SECONDS. The binary
  is GM_LIGHTPANDA_PATH, then `<home>/.agentplug/bin/lightpanda-<arch>-<os>`
  (the lightpanda-io/browser release asset names), then PATH. Windows has no
  native lightpanda build; the server runs in WSL2 through `wsl.exe --exec`
  (GM_LIGHTPANDA_WSL_DISTRO, GM_LIGHTPANDA_WSL_BINARY).
  `agentplug_lightpanda::shutdown()` kills every warm process.

Body (UTF-8 text, one step per line):

- First non-blank line may be `engine=cdp` or `engine=lightpanda`. Absent means
  cdp for `crawl_cdp` and lightpanda for the lightpanda plugin. The other name
  is refused.
- `url=<url>` or a bare line starting `http://`, `https://`, `file://`,
  `data:` or `about:blank`: navigate and wait for `document.readyState`
  `complete`. The page record carries the final URL, title and body text
  (capped at 20000 characters).
- `wait=<ms>`: sleep, at most 60000.
- `eval=<js expression>`: evaluate on the current page; the value is returned
  by value.
- Blank lines and lines starting `#` are ignored. At least one step is required.

Reply (one JSON object):

`ok` (bool), `engine` (`"cdp"` or `"lightpanda"`), `headless` (false for cdp,
true for lightpanda), `stdout` and `stderr` (the Node helper's raw output),
`exit_code` (int or null), `duration_ms`, `timed_out` (bool), `target_id`
(CDP target id, or null), `pages` (array of `{op:"goto",url,title,text}`,
`{op:"wait",ms}`, `{op:"eval",code,value}` in step order), `error` (null or a
string naming the failing step or precondition).

Lightpanda lifecycle: binary from GM_LIGHTPANDA_PATH, then PATH; nothing is
downloaded. `lightpanda serve --host 127.0.0.1 --port <p>` starts once per
project root, readiness is polled every 100 ms for up to 15 s, and the process
is reaped after GM_LIGHTPANDA_IDLE_SECONDS (default 300) without a crawl. The
remembered CDP target is reattached on the next crawl. A crawl that is already
running for the project makes a second crawl answer `ok:false` immediately.
Lightpanda has no native Windows build.

## Versioning

No cross-plugin ABI version negotiation in v1 -- all plugins in one
agentplug-runner release are built against the same pinned `docs/ABI.md`
wire format. A breaking wire-format change ships as a new runner major
version; plugins declare their required runner range in their own
manifest (`"runner": ">=1.0.0"`), checked at load time, refused with a
named error if unmet.
