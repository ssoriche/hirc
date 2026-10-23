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

## Tests

```sh
cargo test --workspace
scripts/ergo-test.sh setup        # once: downloads Ergo, makes test certs
cargo test -p hirc-core --features ergo-tests --test ergo
```

If the Ergo tests fail with certificate errors after an Ergo upgrade, delete
`target/ergo/ca.pem` and rerun `scripts/ergo-test.sh setup`.

## Snapshot mode

For reviewing the UI without a server. It opens the real window with canned
data, drives it with a script, saves a screenshot and quits. The screen must
be unlocked, and nothing touches the network.

```sh
cargo build -p hirc --features snapshot
HIRC_FIXTURE=crates/hirc/fixtures/demo.toml \
HIRC_SCRIPT="window:2;key:escape;shot:out.png" \
HIRC_SIZE=1200x800 \
    target/debug/hirc
```

| Variable | Effect |
|---|---|
| `HIRC_FIXTURE=<file.toml>` | Canned servers and lines; enables snapshot mode. One `[[server]]` table per network (`name`, `nick`, optional `echo_message`, `lines` as raw IRC lines, optionally with an `@time=` tag), plus an optional top-level `base_time`. See `crates/hirc/fixtures/demo.toml`. |
| `HIRC_SCRIPT="step;step"` | Steps: `window:N`, `key:<keystroke>`, `type:<text>`, `wait:<ms>`, `shot:<path.png>`. `type:` only inserts in Insert mode. |
| `HIRC_SCREENSHOT=<out.png>` | After the script, capture the window and quit. |
| `HIRC_SIZE=WxH` | Window size in logical pixels (default `1200x800`). |

Fixture runs never read or write the layout file. Theme and font still come
from the normal config dir.

## macOS app

`scripts/bundle-macos.sh [dir]` builds `hirc.app` into `/Applications` (or
`dir`).
