# CLAUDE.md

slaw-vsts: VST3 plugins in Rust for [slaw](https://github.com/slopstack-labs/slaw) (and any VST3 host), in the slopstack-labs parody voice. The DSP is real; the prose jokes. Read `CONTRIBUTING.md` for the house style (tokenmaxxed identifiers, `remediation_opportunity`, LinkedIn-voice commit messages ending in the `Co-Authored-By` trailer).

## Build, test, bundle

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings   # CI fails on any warning
cargo test --workspace                                   # DSP tests + VST3 COM contract tests
cargo xtask bundle --release                             # → target/bundles/<Name>.vst3
```

## Verify in slaw (the test that matters)

slaw's headless MCP server is a real VST3 host. From a scratch folder (never touch the user's running studio on port 47115):

```sh
dotnet ~/Documents/GitHub/slaw/src/Slaw/bin/Debug/net8.0/slaw.dll mcp scratch.slaw --plugin-folder ~/Documents/GitHub/slaw-vsts/target/bundles
```

Drive it over stdio JSON-RPC: `list_plugins` (vendor "slopstack labs", correct instrument/effect role), `describe_plugin` (titles, units), then a project with the plugin on a track and `render` with `levels: true`. Check stdout carries only JSON-RPC lines (plugins must never print). Measure the WAV for the behaviour the plugin claims.

## Layout

| Path | What |
|------|------|
| `crates/synergy-substrate/src/component.rs` | the single COM object: IComponent + IAudioProcessor + IEditController + IProcessContextRequirements. All host pointers are handled here |
| `crates/synergy-substrate/src/factory.rs` | IPluginFactory 1/2/3 (vendor, sub-categories) |
| `crates/synergy-substrate/src/{parameters,state,dsp}.rs` | parameter tapers/formatting, the state chunk, shared DSP (ZDF filter, PolyBLEP, dB helpers, RBJ biquads, the 4x oversampler) |
| `crates/synergy-substrate/tests/com_contract.rs` | drives a plugin through the real COM interfaces like a host |
| `plugins/<crate>/src/lib.rs` | one `SynergyPlugin` + `export_synergy_plugin!` + its DSP tests |
| `xtask/` | `cargo xtask bundle` |

## Keep these in sync

| When you change… | …also update |
|---|---|
| a plugin's **parameters** (`PARAMETERS`) | README portfolio table (ids, titles, units); the plugin's tests; never reuse or renumber an id (projects and automation store them) |
| a plugin's **name, kind or sub-categories** | `PluginDescription`; `bundle-name` in its `Cargo.toml` (`[package.metadata.slaw-vsts]`); README table; `docs/index.html` card. slaw treats sub-categories containing `Instrument` as instruments and shows the second part as the role |
| **buses** (`sidechain_input`, `note_input`, bus names/flags in `component.rs`) | README "sidechain contract" table (slaw matches the `Sidechain` aux bus by it); `com_contract.rs` bus assertions |
| a **new plugin** | workspace `members`; class id from a fresh UUID; README table + track record; `docs/index.html` card; CI picks it up via `xtask` automatically |
| the **state chunk** format (`state.rs`) | bump `STATE_FORMAT_VERSION` and keep reading the old version, or saved projects lose their settings |
| **bundle layout** (`xtask`) | README "Bundle layout" table; release workflow |

## Rules

- `unsafe` only in `synergy-substrate`, only at the VST3 boundary, with a comment saying why it is sound. Plugin crates have none.
- `process()` never allocates, locks (the substrate uses `try_lock` and emits silence on contention), blocks or prints. Allocate in `new()`.
- Inputs are copied before processing because hosts may process in place; never create a shared and a mutable view of the same host buffer.
- An inactive or missing aux bus means silence, not an error.
- Every plugin ships tests for: no NaN/inf, denormal-free tails, and the behaviour its one-line pitch promises.
- Dependencies must stay MIT/Apache-compatible: no `nih-plug` VST3 export or `vst3-sys` (GPLv3). Check `cargo metadata` licenses when adding a crate.
