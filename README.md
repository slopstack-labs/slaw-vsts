# slaw-vsts
### *Beyond Deterministic Signal Processing*

> "We didn't reinvent the plugin. We asked a plugin to attend more meetings."
> — slaw-vsts Engineering Blog, Issue 1 (Final)

**slaw-vsts** is the official third-party first-party plugin portfolio for [slaw](https://github.com/slopstack-labs/slaw), the AI-native DAW. Four VST3 plugins, written in Rust, that bring enterprise governance to your signal chain: dynamics that hit their quarterly targets, a chorus that adds stakeholders, a synth that manages its own headcount, and a sidechain ducker that knows who has the floor.

Unlike slaw itself, these plugins are **fully deterministic**. Legal insisted. Every sample out is a pure function of the samples in, the knobs, and the host's tempo. We are as surprised as you are.

**Key differentiators:**
- Written in Rust, the language of memory safety, so the only thing leaking is the reverb tail
- MIT-licensed all the way down: the VST3 bindings are the MIT `vst3` crate, not a GPL framework with a subscription to your source code
- One shared substrate (`synergy-substrate`) speaks VST3 so the plugins only speak DSP
- No GUIs. Your host draws the knobs. slaw draws excellent knobs
- Works in slaw, and in any other VST3 host, which we consider an aggressive go-to-market strategy
- Blazing-fast time-to-gain-reduction
- Sidechain-as-a-service: a real stereo aux bus, for hosts that route one, and a tempo pump and MIDI trigger for hosts (and org charts) that do not

**slaw-vsts is the only plugin suite built on the insight that your mix doesn't need *mixing* — it needs *alignment*.**

## Roadmap

See the [Signal Chain of Command](ROADMAP.md).

## Contributing

See the [Contributor Excellence Framework](CONTRIBUTING.md).

## The portfolio

| Plugin | Kind | Pitch | Parameters (VST3 id: title) |
|--------|------|-------|-----------------------------|
| **KPI Compressor** | effect · `Fx\|Dynamics` | Compresses your dynamic range to meet quarterly targets. Feed-forward, stereo-linked peak compressor with a soft knee and a parallel mix. | p0 Threshold (dB) · p1 Ratio (:1) · p2 Knee (dB) · p3 Attack (ms) · p4 Release (ms) · p5 Makeup (dB) · p6 Mix (%) · p7 Gain Reduction (dB, read-only meter) |
| **Stakeholder Chorus** | effect · `Fx\|Modulation` | More voices in the room, none of them listening. Up to four modulated delay taps, each heard at a different phase left and right, so mono leaves wider than it came in. | p0 Rate (Hz) · p1 Depth (ms) · p2 Stakeholders (1–4 voices) · p3 Width (%) · p4 Feedback (%) · p5 Mix (%) |
| **Headcount** | instrument · `Instrument\|Synth` | Scales headcount up to 8 voices and lays them off on note-off. Polyphonic subtractive synth: two PolyBLEP oscillators, a resonant zero-delay-feedback lowpass with an envelope sweep, ADSR, velocity, oldest-first voice stealing. | p0 Osc 1 Wave (Saw/Square) · p1 Osc 2 Wave · p2 Osc 2 Detune (ct) · p3 Osc 2 Octave (−2…+2) · p4 Osc Mix (%) · p5 Cutoff (Hz) · p6 Resonance (%) · p7 Filter Envelope (%) · p8 Attack · p9 Decay · p10 Sustain · p11 Release · p12 Headcount (1–8 voices) · p13 Volume (dB) · p14 Voices Employed (read-only meter) |
| **Executive Override** | effect · `Fx\|Dynamics` | When the kick speaks, everyone else stops talking. A ducker keyed by a sidechain, by the host's tempo, or by MIDI notes. | p0 Mode (Sidechain/Tempo pump/MIDI trigger) · p1 Threshold (dB) · p2 Depth (dB) · p3 Attack (ms) · p4 Release (ms) · p5 Hold (ms) · p6 Curve (%) · p7 Rate (1/4, 1/8, 1/2, 1 bar) · p8 Sidechain HPF (Hz) · p9 Mix (%) · p10 Gain Reduction (dB, read-only meter) |

Parameter ids are forever: projects and automation refer to them, so new parameters get new ids.

### Executive Override: the sidechain contract

| Bus | Direction | Type | Channels | Name | Default |
|-----|-----------|------|----------|------|---------|
| 0 | audio in | `kMain` | 2 (stereo) | `Input` | active |
| 1 | audio in | `kAux` | 2 (stereo; mono accepted and copied to both sides) | `Sidechain` | **inactive** (`flags = 0`) |
| 0 | audio out | `kMain` | 2 (stereo) | `Output` | active |
| 0 | event in | `kMain` | 16 | `Note Input` | active |

- **Sidechain** mode keys off bus 1 after an optional highpass (*Sidechain HPF*, to listen only to the kick's low end). A host that leaves the bus inactive, gives it zero channels or null buffers, is treated as silence: nothing ducks.
- **Tempo pump** mode needs no routing at all. It reads the host's `ProcessContext` (tempo, `projectTimeMusic`, `barPositionMusic`, time signature, playing) and ducks on every 1/4, 1/8, 1/2 or whole bar from the bar line; with no transport it free-runs at 120 BPM.
- **MIDI trigger** mode ducks on every note-on arriving on the event bus.

All three share one shape: down to *Depth* over *Attack*, held for *Hold*, back up over *Release* along *Curve* (0 % linear, 100 % snappy). *Gain Reduction* reports the deepest duck of each block.

## Install

### From a release

Download the zip for your platform from [Releases](https://github.com/slopstack-labs/slaw-vsts/releases) and unpack the `.vst3` bundles into a VST3 folder:

| OS | Folder |
|----|--------|
| Linux | `~/.vst3` |
| Windows | `C:\Program Files\Common Files\VST3` |
| macOS | `~/Library/Audio/Plug-Ins/VST3` |

### In slaw

slaw scans `~/.vst3`, `/usr/lib/vst3` and `/usr/local/lib/vst3` (Windows: `Common Files\VST3`) on its own. To use a build straight from this repository instead:

- **Studio:** *Settings → Plugins → Add folder…* and pick `target/bundles`, then **Rescan**. The plugins appear under **Plugins (VST3)** in the browser, in the instrument dropdown and in *+ Effect → VST3*.
- **Command line / MCP:** `slaw studio --plugin-folder /path/to/slaw-vsts/target/bundles`, or `slaw mcp song.slaw --plugin-folder …`, or set `VST3_PATH=/path/to/slaw-vsts/target/bundles`.

Agents can find them with the MCP `list_plugins` tool (vendor *slopstack labs*) and see their parameters with `describe_plugin`.

### In any other VST3 host

Copy the bundles into the folder above for your OS and rescan. The plugins have no editor of their own; hosts show their generic parameter panel.

## Build

Requires Rust 1.85+ (`rustup` picks the stable toolchain from `rust-toolchain.toml`).

```sh
cargo test --workspace                 # DSP and VST3 contract tests
cargo xtask bundle --release           # → target/bundles/<Name>.vst3
cargo xtask bundle --release headcount # just one plugin
```

CI builds the Windows and macOS bundles natively. Cross-building Windows bundles from Linux should also work with the GNU toolchain (not yet exercised by a stakeholder):

```sh
rustup target add x86_64-pc-windows-gnu   # plus mingw-w64 from your package manager
cargo xtask bundle --release --target x86_64-pc-windows-gnu
```

| Platform | Bundle layout |
|----------|---------------|
| Linux | `<Name>.vst3/Contents/x86_64-linux/<Name>.so` |
| Windows | `<Name>.vst3/Contents/x86_64-win/<Name>.vst3` |
| macOS | `<Name>.vst3/Contents/MacOS/<Name>` + `Info.plist` + `PkgInfo` (unsigned: Gatekeeper will want a word) |

## Layout

```
crates/synergy-substrate/   the VST3 layer: factory, single-component processor+controller, buses, state, shared DSP
plugins/kpi-compressor/     KPI Compressor
plugins/stakeholder-chorus/ Stakeholder Chorus
plugins/headcount/          Headcount
plugins/executive-override/ Executive Override
xtask/                      `cargo xtask bundle`
docs/                       the landing page
```

A plugin is one `SynergyPlugin` implementation: a `PluginDescription` (name, class id, kind, sub-categories, parameters, buses) and a `process` function over plain slices. The substrate turns it into a VST3 class with `export_synergy_plugin!(YourPlugin)`.

## Track record

Measured in slaw's own VST3 host (headless `slaw mcp`, 44.1 kHz, 120 BPM) with the bundles from this repository:

| Plugin | Test | Result |
|--------|------|--------|
| all four | `list_plugins` | listed with vendor *slopstack labs*, roles synth / dynamics / modulation / dynamics; meters hidden from the knob panel as read-only |
| Headcount → KPI Compressor → Stakeholder Chorus | render a 4-bar chord progression | audible (RMS −38 dBFS after compression), no stray output on stdout |
| KPI Compressor | threshold −42 dB, 8:1, vs. mix 0 % | RMS −48.9 vs −18.5 dBFS; crest factor 12.1 vs 14.4 dB |
| Stakeholder Chorus | fully wet vs. mix 0 % | L/R correlation 0.51 vs 0.96 |
| Stakeholder Chorus | Mix automated 0 → 100 % as `fx.1.p5` | L/R correlation 0.94 in bar 1 → 0.37 in bar 4 |
| Executive Override | Tempo pump, Depth 12 dB, 1/4 | −9.2 to −10.4 dB, 20 ms after every quarter note; 0.0 dB by ¾ of the beat |
| Executive Override | Sidechain mode, slaw leaves the aux bus inactive | output bit-identical to bypass |

*Stakeholders consulted: 4. Stakeholders listened to: 0.*

## License

[MIT](LICENSE). Dependencies: `vst3` and `com-scrape-types` (both MIT OR Apache-2.0). VST is a registered trademark of Steinberg Media Technologies GmbH, who were not consulted either.
