# stui

English | [简体中文](README.zh.md)

A TUI for managing GNU Screen sessions, optimized for phone-sized SSH terminals.

Over SSH — especially from a phone — stui replaces the "remember the PID, guess the session, type a long `screen -r` incantation" routine with a single panel you can read and act on. Typical use: keep long-running AI tasks (CodeX / Claude Code) inside Screen so they survive a dropped SSH connection.

## Features

- **Session list**: index, name, status (detached / attached / multi / dead / unreachable), PID; creation time too on Screen 4.6+. Overly wide names are truncated first — the index and status columns are never sacrificed.
- **New session**: sensible defaults first — name from the current directory, directory from `$PWD`, command from `$SHELL` — fits on one screen, expands only when you need to change something.
- **One-key attach**: dispatches `-r` / `-d -r` / `-x` based on session state, so you never memorize the difference. Attached / multi sessions are taken over by default; `x` shares instead.
- **Read-only preview**: `p` snapshots the session via `hardcopy` — never injects a single keystroke into it.
- **Management actions**: detach, kill, wipe, rename — all with a confirmation step; one-key cleanup of dead sessions. Sessions created by stui record their directory and command, so a dead session can be relaunched as-is (`s`).
- **Environment self-check**: `stui doctor` verifies the screen installation and configuration item by item, with actionable fixes.

## Install

On Linux (amd64 / arm64, statically linked with musl), grab a prebuilt binary from [GitHub Releases](https://github.com/vicalloy/screen-tui/releases/latest) (`SHA256SUMS` included):

```sh
curl -LO https://github.com/vicalloy/screen-tui/releases/latest/download/stui-x86_64-linux.tar.gz
tar xzf stui-x86_64-linux.tar.gz && sudo mv stui /usr/local/bin/
```

On other platforms (macOS arm64 / x86_64, etc.), build from source:

```sh
cargo build --release
```

## Usage

```sh
stui                  # open the TUI
stui ls               # plain-text session list (no ANSI, pipe-safe; switches to machine mode when stdout is not a TTY)
stui ls --no-header   # data rows only: no header, tab-separated
stui ls --full        # append the <pid>.<name> address column
stui doctor           # environment self-check report
stui version          # print version
```

`stui ls` exit codes: `0` sessions exist / `1` no sessions / `2` environment problem.

### TUI keys

| Key | Action |
| --- | --- |
| `j`/`k` or `↑`/`↓` | move |
| `Enter` / `1`–`9` | attach (attached sessions are taken over) |
| `x` | shared attach (`-x`) |
| `p` | read-only preview |
| `n` | new session |
| `i` | session details |
| `/` | filter |
| `r` | rename |
| `D` / `K` / `W` | detach / kill / wipe (with confirmation) |
| `s` | restart a dead session (from the recorded directory and command) |
| `X` | clean up metadata of stale sessions (aliases, favorites, etc.) |
| `R` | manual refresh |
| `?` | help |
| `q` | quit |
| `Esc` | back / cancel (inside overlays) |

## Configuration

The config file lives at `~/.config/screen-tui/config.json` (or `$XDG_CONFIG_HOME/screen-tui/` when set, or directly inside `$SCREEN_TUI_HOME` when set). Delete it at any time — the tool leaves no other trace. It stores favorite directories (hot-picked via `1`–`9` in the new-session form), session aliases, the directory/command records used for restarting, and the UI language (`zh` / `en` / `auto`).

Environment variables:

| Variable | Meaning |
| --- | --- |
| `STUI_LANG` | override the UI language (`zh` / `en`) |
| `STUI_SCREEN` | path to the screen executable (defaults to searching `PATH`) |
| `STUI_AUTO_REFRESH` | auto-refresh interval in seconds (positive integer); manual-only refresh by default |
| `SCREEN_TUI_HOME` | override the config directory |

## Compatibility

Works with every GNU Screen from 4.00.03 (2006) onward, including the ancient copy bundled with macOS. It relies only on the veteran interface set `-ls` / `-dmS` / `-r` / `-x` / `-d` / `-X` / `-wipe`; newer capabilities such as `-Q` degrade gracefully when missing — no errors.

Design constraints: non-invasive (never writes input to sessions), leaves your environment alone (never touches `.screenrc`, installs no services), and leaves fields blank rather than inventing data.

## Design documents

Requirements, technical design, Screen capability boundaries, and reference-project analysis live in [`design/`](design/):

- [`design/requirements.md`](design/requirements.md) — requirements (FR / NFR)
- [`design/tech-design.md`](design/tech-design.md) — technical design (Rust / ratatui architecture)
- [`design/screen-capabilities.md`](design/screen-capabilities.md) — Screen capability boundaries
- [`design/reference-projects.md`](design/reference-projects.md) — comparative analysis of reference projects

## License

MIT
