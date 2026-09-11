# KnightCode IDE

A desktop IDE built on a fork of Zed, with KnightCode as its only agent,
its only inference path, and its only login. The IDE is Rust. The AI stack
stays in the KnightCode TypeScript packages and ships alongside it as a
headless engine binary.

The engine is a peer of the KnightCode CLI, not a mode of it. It owns
every provider request and every credential, the agent loop, tools, skills,
extensions, compaction, and session persistence. The IDE owns every pixel
and keystroke, buffers, which model is selected, the sign-in presentation,
and the engine process's lifetime. The IDE holds no API key, runs no OAuth
exchange, and stores no credential.

Three seams connect them. The agent panel speaks ACP over stdio. Buffer
inline assist, terminal inline assist, commit messages and thread titles
speak HTTP chat completions on loopback. Tab / next-edit prediction speaks
HTTP fill-in-the-middle. One login serves all three.

## Base

This `main` starts at Zed commit
`a57ba9b17c433ea1ebfdec8f649f4fa5a402d03b` (2026-09-10 17:54:38 UTC),
tagged here as `knightcode-base`.

Nearest upstream tags at that date: `v1.19.2` (stable) and
`v1.20.0-pre`. There was no `v1.21` tag. Rebasing onto `v1.21.0` when it
is cut is a merge of a tag that already contains this base.

Remote `upstream` is https://github.com/zed-industries/zed.

## Merging upstream

```text
git fetch upstream
git merge upstream/v1.21.0
```

Conflicts are expected only in the files below. Everything else should
apply cleanly; if it does not, the extra conflict is a defect in the
fork surface.

| File | Change |
| --- | --- |
| `crates/knightcode_agent/` | new crate, agent panel |
| `crates/knightcode_models/` | new crate, completions and edit prediction |
| `crates/knightcode_engine/` | new crate, process lifecycle and HTTP client |
| `crates/agent_ui/src/agent_ui.rs` | one match arm; command-palette filter |
| `crates/agent_ui/src/agent_panel.rs` | unspent |
| `crates/agent_ui/src/conversation_view.rs` | unspent |
| `crates/agent_ui/src/mention_set.rs` | unspent |
| `crates/agent_ui/Cargo.toml` | one dependency |
| `crates/language_models/src/language_models.rs` | provider registration body |
| `crates/language_models/Cargo.toml` | one dependency |
| `crates/settings_content/src/settings_content.rs` | one section |
| `crates/settings/src/vscode_import.rs` | one field |
| `crates/settings_content/src/language.rs` | one enum variant, two arms |
| `crates/language/src/language_settings.rs` | one arm |
| `crates/edit_prediction/src/edit_prediction.rs` | two arms |
| `crates/edit_prediction_ui/src/edit_prediction_button.rs` | one arm |
| `crates/edit_prediction_ui/Cargo.toml` | one dependency |
| `crates/zed/src/zed/edit_prediction_registry.rs` | one enum variant, three arms |
| `crates/zed/src/main.rs` | no_proxy, engine init, quit, palette, first open |
| `crates/zed/Cargo.toml` | two dependencies; `agent_servers` and `command_palette_hooks` made plain dependencies |
| `crates/agent/src/agent.rs` | one string |
| `crates/release_channel/src/lib.rs` | four strings |
| `assets/settings/default.json` | three keys |
| `Cargo.toml` | three members, three workspace dependencies |
| `Cargo.lock` | lockfile |

Nothing in `editor`, `project`, `workspace`, `terminal`, `git`, `vim`, or
`gpui`.

After every merge, compare `acp_thread::AgentConnection` with the
delegating `impl` in `crates/knightcode_agent/src/connection.rs`. A method
upstream adds with a default body compiles without a delegation, and then
the default silently disables that feature for KnightCode; add the
delegation for every new method.

## Building

```text
cargo build -p zed
```

The first build on Windows is long and the target directory is large.
Windows prerequisites are documented in
[`docs/src/development/windows.md`](docs/src/development/windows.md).
The toolchain is the one pinned in `rust-toolchain.toml`.

## Pointing a development build at an engine

The IDE looks for `knightcode-engine` in this order: the
`knightcode.engine_path` setting (an absolute path), the
`KNIGHTCODE_ENGINE_PATH` environment variable, then
`knightcode-engine.exe` (Windows) or `knightcode-engine` next to the IDE
executable.

Example in the user settings file:

```json
{
  "knightcode": {
    "engine_path": "C:/Users/you/knightcode/packages/cli-win32-x64/bin/knightcode-engine.exe"
  }
}
```

## Licence

The IDE is `GPL-3.0-or-later`, as any Zed fork must be; see
[`LICENSE-GPL`](LICENSE-GPL). The engine is a separate MIT program and is
not linked into this tree.
