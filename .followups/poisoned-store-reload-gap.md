# Follow-up: poisoned-Store reload sometimes has no reload source

Observed live (2026-07-30, webix session): after the browser-session-id fix
(1697e6e) and the daemon spawn-lock-race fix landed, `browser` dispatches
against a real page (docs/index.html locally, and the live gh-pages URL)
still occasionally hit:

    plugin_poisoned_store_evicted: prior_dispatch_error="reinstantiation
    failed or no reload source available", reinstantiation_succeeded=false

Traced to `registry.rs::dispatch()` (~line 385-440): on a poisoned Store,
it calls `reinstantiate_plugin_into_pool_slot_if_reload_source_available`,
which is a no-op (`return Ok(())`) whenever `self.reload_source` is `None`
(registry.rs:356). `DispatchHandle::dispatch_handle()` (no-reload variant,
registry.rs:343) currently has ZERO callers -- every real call path already
goes through `dispatch_handle_with_reload(Some((engine, modules_with_hashes())))`
(daemon.rs:1121, 1213). That means when reinstantiation genuinely fails, the
`modules_with_hashes()` snapshot passed into that specific dispatch's
`DispatchHandle` did not contain the `gm` plugin's module at that moment --
worth checking `plugin_modules` construction/timing around daemon.rs:1121
and 1213 (is it built once at daemon start and going stale, or is there a
window where a plugin's module briefly isn't in the map during a hot-reload
poll?). Did not chase further this session -- two other real fixes were
already in flight (browser session-id churn, daemon spawn-lock race) and a
third speculative patch to registry.rs without reproducing the exact
modules_with_hashes() state at crash time risked more than it fixed.

Repro: repeated `browser` dispatches with `url=` navigation + a real page
(not just bare `return 1+1` evals, which never hit this) against the same
project, several in a row within a few minutes. Crash was NOT
100%-reproducible per-call, more like 1-in-3 to 1-in-5 under sustained use.
