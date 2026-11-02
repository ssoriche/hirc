# hirc

A native macOS IRC client for [Ergo](https://ergo.chat), written in Rust
with Zed's [gpui](https://gpui.rs). It reads Halloy's config and themes.

## Develop

```sh
flox activate
cargo run -p hirc
```

The first build takes several minutes; dependencies are optimized even in
debug builds, so later builds and `cargo run` stay fast.

## Config

`~/.config/hirc/config.toml` (or `$XDG_CONFIG_HOME/hirc/`) in
[Halloy's format](https://halloy.chat/configuration/), with themes in
`~/.config/hirc/themes/<name>.toml`. Keys hirc doesn't support yet are
ignored.

- `history_page_size` (hirc only, default 100): lines per page of history, never more than the server allows (its `CHATHISTORY` limit). `chathistory = false` turns server history off for that server.

## History

Each window opens on its latest page of history. Scroll to the top for older pages: "loading older messages" shows while one is on its way, and "start of history" once the server has no more. After a reconnect hirc asks only for what it missed. While you were away, queries with new messages open at the end of the window list. Your read position is kept on the server (`draft/read-marker`), so the "new" divider is where you left off on any device.

What hirc has shown is kept in `$XDG_DATA_HOME/hirc/history.db` (else `~/.local/share/hirc/history.db`). Only lines the server gave an id are kept. Nothing from a services query (NickServ and the like), and no config or passwords, ever goes in it. Delete the file to forget it all. A file hirc can't read is renamed to `history.db.bad-<time>`, and a new one is started.

## Windows

The sidebar is an irssi-style flat numbered list of every buffer across all
networks.

- `/window N` (or `/win N`, or `/window goto N`) jumps to window N; `/window move N` moves the
  current window to position N.
- `alt-1` to `alt-9` jump to windows 1 to 9; `alt-0` jumps to window 10.
- The order is saved in `$XDG_STATE_HOME/hirc/layout.toml`, else
  `~/.local/state/hirc/layout.toml`.

## Keys

The input line is vim-style and starts in Insert mode.

- `Esc` enters Normal mode; `i`, `a`, `I`, `A` go back to Insert.
- Motions: `h`, `l`, `0`, `$`, `w`, `b`, `e`.
- Edits: `x`, `D`, `C`, `dd`, `cc`, `dw`, `cw`, `ciw`, `p`, `P`, `u`.
- `Backspace` moves left; `Enter` submits the line from Normal mode too.
- Scrolling the buffer: `j`/`k`, `ctrl-d`/`ctrl-u`, `gg`/`G`.
- Windows: `gt` / `gT` for next / previous.
- `:` opens an ex line: `:N` goes to window N, `:b name` (or `:buffer name`)
  goes to the named buffer, `:q` quits, and anything else is run as the
  slash command `/cmd`. `Esc` on the ex line abandons it and restores the
  unsent draft; ex commands also keep the draft.

## Commands

| Command | Effect |
|---|---|
| `/join #chan [key]` (`/j`) | Join a channel |
| `/part [#chan] [reason]` (`/leave`) | Leave a channel (the current one if omitted) |
| `/msg nick text` (`/privmsg`) | Send a message |
| `/query nick [text]` (`/q`) | Open a DM window, optionally sending a first line |
| `/me text` | Send an action |
| `/nick newnick` | Change nick |
| `/topic [text]` | Show or set the topic |
| `/raw line` (`/quote`) | Send a raw IRC line |
| `/window N`, `/window move N` | Switch or move windows |
| `/quit [reason]` (`/disconnect`) | Disconnect the current network |
| `/reconnect` | Reconnect the current network |

Start a line with `//` to send a message that begins with a slash.

## Wire log

To see every line a server sends, run with the `hirc::wire` trace on:

```fish
env RUST_LOG=warn,hirc::wire=trace NO_COLOR=1 hirc 2> wire.log
```

It logs only what servers send, never what hirc sends, so passwords don't
appear. It does hold your messages: trim it before sharing.

## Tests

```sh
cargo test --workspace
scripts/ergo-test.sh setup        # once: downloads Ergo, makes test certs
cargo test -p hirc-core --features ergo-tests --test ergo
cargo test -p hirc-core --features bouncer-tests --test bouncer
```

If the Ergo tests fail with certificate errors after an Ergo upgrade, delete
`target/ergo/ca.pem` and rerun `scripts/ergo-test.sh setup`.

The bouncer tests run hirc through ZNC (with its playback module) and soju,
each in front of a test Ergo. Both bouncers come from the flox environment
(run the tests inside `flox activate`), which installs them on aarch64-darwin
only: the bouncer tests do not run on other systems.
Add `-- --ignored` to re-record the ZNC and soju transcripts in
`crates/hirc-core/tests/data/`, which the unit tests read.

## Snapshot mode

For reviewing the UI without a server. It runs the real app with canned data,
drives it with a script and saves screenshots. Nothing touches the network.
Two renderers share one fixture and script format:

- Window mode (default): opens the real window and captures it with
  `screencapture`. The screen must be unlocked.
- Offscreen mode (`HIRC_OFFSCREEN=1`): no window and no AppKit app. gpui's
  headless test context draws with Metal into a texture, so it works with no
  window server or unlocked screen (a Metal device is still needed).

```sh
cargo build -p hirc --features snapshot
HIRC_OFFSCREEN=1 \
HIRC_FIXTURE=crates/hirc/fixtures/demo.toml \
HIRC_SCRIPT="window:2;click:rust;key:i;type:hello;shot:typing.png;key:enter" \
HIRC_SCREENSHOT=out.png \
    target/debug/hirc
```

| Variable | Effect |
|---|---|
| `HIRC_FIXTURE=<file.toml>` | Canned servers and lines; enables snapshot mode. One `[[server]]` table per network (`name`, `nick`, optional `echo_message`, `lines` as raw IRC lines, optionally with an `@time=` tag; optional `chathistory`, `read_marker` and `history_page_size` for a server with history, and `[[server.history]]` tables of made-up history per target: `target`, `start`, `count`, optional `nicks` and `events`), plus an optional top-level `base_time`. See `crates/hirc/fixtures/demo.toml` and `crates/hirc/fixtures/history.toml`. |
| `HIRC_SCRIPT="step;step"` | Steps, below. |
| `HIRC_SCREENSHOT=<out.png>` | Final image, taken after the script. |
| `HIRC_OFFSCREEN=1` | Offscreen mode. Exits 0 when done; on failure prints `snapshot: <reason>` and exits 1. |
| `HIRC_SIZE=WxH` | Logical size (default `1200x800`). |
| `HIRC_SCALE=<f>` | Device scale for offscreen output (default `2`). gpui's headless window is fixed at 2x; `1` downsamples that, other values are an error. |
| `HIRC_THEME=<name>` | Overrides the config's `theme` for this run; a missing theme exits 1. |

Steps are separated by `;`. Each is followed by a few frames. Offscreen runs
use a simulated clock, so `wait` costs no real time.

| Step | Effect |
|---|---|
| `window:N` | Go to window N. |
| `key:<ks>` | A keystroke through the normal key path, so vim modes apply: `key:escape`, `key:ctrl-d`, `key:alt-2`, `key:enter`. `cmd+k` is accepted for `cmd-k`. |
| `type:<text>` | Insert text (may contain `:`). Only inserts in Insert mode, so use `key:i` first when needed. |
| `wait:<ms>` | Advance the simulated clock (offscreen) or sleep (window). |
| `shot:<png>` | Save an image now. |
| `click:<Label>` | Click the first sidebar row whose label contains `<Label>`, ignoring case. No match prints `snapshot: no click target <Label>` and continues. |
| `at:x,y` | Primary mouse down and up at logical (x, y). |
| `hover:x,y` | Mouse move to (x, y). |
| `scroll:<px>` | Scroll wheel over the buffer view; negative scrolls down. |

An unknown step prints `snapshot: unknown step <s>` and continues.

Fixture runs never read or write the layout file. Theme and font still come
from the normal config dir.

`crates/hirc/fixtures/history.toml` has a fake server with history. The run below saves the history states: the "new" divider at the server's read marker, "loading older messages" while a page is on its way (`delay:` holds the page back), "start of history", and replayed joins, parts and nick changes:

    HIRC_FIXTURE=crates/hirc/fixtures/history.toml HIRC_OFFSCREEN=1 \
    HIRC_SCRIPT="window:2;shot:target/history-review/1-divider.png;delay:1500;scroll:5000;wait:100;shot:target/history-review/2-loading.png;wait:1600;delay:0;scroll:5000;wait:200;scroll:5000;wait:200;scroll:5000;wait:200;scroll:5000;wait:200;scroll:5000;wait:200;shot:target/history-review/3-start.png;window:3;shot:target/history-review/4-events.png" \
    cargo run -p hirc --features snapshot

The offscreen smoke test needs a Metal device and runs the real binary (AppKit
wants the process main thread), so it is ignored by default:
`cargo test -p hirc --features snapshot --test offscreen -- --ignored`

## macOS app

`scripts/bundle-macos.sh [dir]` builds `hirc.app` into `/Applications` (or
`dir`).
