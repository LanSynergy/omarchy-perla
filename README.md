# Perla

**Give Omarchy a voice.**

Perla does not just dictate into a text box. Talk naturally and it can
understand what you want, act across your Omarchy desktop, and talk back when
the work is done.

![Perla turns spoken intent into Omarchy desktop action and talks back](preview.png)

| Keyboard | Your voice |
|---|---|
| ~45 words/min | ~150 words/min |

Perla lives in the Omarchy bar, while a local Rust daemon handles the realtime
voice session. Voxtype remains your dictation tool; Perla is for conversations,
desktop actions, and spoken results.

## Install

Perla currently needs a one-time manual setup because it includes a native Rust
daemon and an optional computer-use harness.

```sh
git clone https://github.com/inawafalm/omarchy-perla.git ~/.local/share/perla
cd ~/.local/share/perla

# Omarchy/Arch dependencies
omarchy-pkg-add rust alsa-lib pkgconf wlrctl wtype grim python-uv

# Voice daemon
cargo install --locked --root "$HOME/.local" \
  --path perla-voice/crates/perla-cli --bin perla-d
install -Dm644 perla-voice/packaging/perla.service \
  "$HOME/.config/systemd/user/perla.service"
systemctl --user daemon-reload
systemctl --user enable --now perla.service

# Computer-use harness
uv tool install ./omarchy-harness
mkdir -p "$HOME/.grok/skills"
cp -R omarchy-harness/skills/omarchy-harness "$HOME/.grok/skills/"

# Bar plugin
omarchy plugin add https://github.com/inawafalm/omarchy-perla --enable
```

Run `perla-d ping`, then right-click the Perla orb. Choose OpenAI or Grok,
choose a realtime model, paste your own API key into the password field, and
save. Perla never ships with a shared API key.

Optional user hotkeys:

```ini
bind = SUPER ALT, SPACE, exec, perla-d toggle-listen
bind = SUPER SHIFT, BackSpace, exec, sh -c 'perla-d mute; omarchy-harness stop'
```

The second binding is a panic switch: it mutes Perla and stops computer use.
Perla does not take Voxtype's F9 or Super+Ctrl+X bindings.

## Privacy and security

- There are no API keys, credentials, or Perla relay servers in this repository.
- Your key is sent to `perla-d` over stdin, never a process argument, and stored
  only in `~/.config/perla-voice/config.toml` with mode `0600`.
- The control socket, runtime state, and local transcript/tool log are restricted
  to your user. Public state exposes only booleans such as `has_openai_key`.
- Microphone audio goes directly from your machine to the provider you select
  (OpenAI or xAI). Your own provider account pays the API usage.
- Computer use can capture the screen, click, and type as your Linux user.
  Plugins run as unsandboxed QML inside `omarchy-shell`; review the source and
  use the panic switch if anything looks wrong.
- The local debug trail contains what you said, Perla's replies, and tool calls.
  It is never uploaded by Perla; `Copy debug log` shares it only when you ask.

Cost controls are enabled by default: the efficient realtime model, bounded
context, cache-friendly truncation, short spoken replies, one-turn screenshot
cleanup, completion-only progress, idle shutdown, and pre-cap session rotation.

## Controls

- Left-click the orb: compact listen/mute controls.
- Right-click: settings and model drawer.
- Voice model: pick a provider model without editing config files.
- Spoken progress: completion only, major milestones, or every step.
- Reply language: automatic or pinned to your preferred language.

Provider, model, progress mode, and start-muted changes reload the daemon. If a
session was active, Perla reconnects automatically.

## Remove

```sh
omarchy plugin remove nawaf.perla --yes || omarchy plugin remove nawaf.perla
systemctl --user disable --now perla.service
rm "$HOME/.config/systemd/user/perla.service"
systemctl --user daemon-reload
rm -f "$HOME/.local/bin/perla-d" "$HOME/.local/bin/omarchy-harness"
```

Your key and local history are intentionally kept. To erase them too, remove
`~/.config/perla-voice`, `~/.local/state/perla`, and `~/.local/share/perla`.

## Development

```sh
node test-model.js
cargo test --manifest-path perla-voice/Cargo.toml --workspace
uv run --with pytest --with ./omarchy-harness python -m pytest omarchy-harness/tests
```

License: MIT. Copyright 2026 Nawaf.
