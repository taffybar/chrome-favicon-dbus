# Chrome Favicon D-Bus

Publish active Chrome tab metadata (title, URL, favicon) to D-Bus so Hyprland/Wayland/X11 tooling can consume it.

The extension sends JSON to a local HTTP endpoint, and a Rust daemon publishes enriched updates on D-Bus.

## What this gives you

- Chrome extension emits active tab + Chrome window metadata.
- Rust daemon exposes updates over D-Bus signal: `org.imalison.ChromeWindowInfo.Updated`.
- Payload enrichment includes:
  - Hyprland active window and client list (`hyprctl -j activewindow`, `hyprctl -j clients`)
  - Hyprland IPC event state from `.socket2.sock` when available
  - Scored Hyprland candidate windows using focus, title, geometry, and cached continuity
  - X11 active window via `xdotool` when available
- Cached mapping from Chrome `window_id` -> Hyprland window address with confidence/source metadata.

## Layout

- `extension/manifest.json`: Manifest V3 extension
- `extension/background.js`: Active-tab capture and localhost POST
- `bridge-rs/Cargo.toml`: Rust daemon crate
- `bridge-rs/src/main.rs`: HTTP -> D-Bus daemon
- `systemd/chrome-favicon-dbus.service`: Optional user service template

## Extension transport

Extension posts to:

- `http://127.0.0.1:38933/update`

Chrome extension contexts can do outbound localhost `fetch`, but cannot host a local server or D-Bus service themselves.

## Build and install daemon (Nix)

Build standard Linux package:

```bash
cd ~/Projects/chrome-favicon-dbus
nix build .#chrome-favicon-dbus
```

Run the built binary:

```bash
./result/bin/chrome-favicon-dbus --host 127.0.0.1 --port 38933 --path /update
```

Build static Linux package (musl):

```bash
cd ~/Projects/chrome-favicon-dbus
nix build .#chrome-favicon-dbus-static
```

Run without building explicitly:

```bash
cd ~/Projects/chrome-favicon-dbus
nix run .#chrome-favicon-dbus -- --host 127.0.0.1 --port 38933 --path /update
```

Install in profile:

```bash
cd ~/Projects/chrome-favicon-dbus
nix profile install .#chrome-favicon-dbus
```

## Alternative non-Nix build

```bash
cd ~/Projects/chrome-favicon-dbus
cargo install --path bridge-rs --locked
```

## Load extension

```text
Chrome -> chrome://extensions -> Developer mode -> Load unpacked -> ~/Projects/chrome-favicon-dbus/extension
```

Note: Recent Google Chrome builds (including 145.x) reject command-line extension loading flags (`--load-extension`, `--disable-extensions-except`). Use `chrome://extensions` developer mode for unpacked installs.

## Monitor D-Bus updates

```bash
gdbus monitor --session \
  --dest org.imalison.ChromeWindowInfo \
  --object-path /org/imalison/ChromeWindowInfo
```

Fallback if `gdbus` is unavailable:

```bash
dbus-monitor "type='signal',interface='org.imalison.ChromeWindowInfo',member='Updated'"
```

Each `Updated` signal carries one JSON string payload.

## D-Bus contract

- Bus name: `org.imalison.ChromeWindowInfo`
- Object path: `/org/imalison/ChromeWindowInfo`
- Interface: `org.imalison.ChromeWindowInfo`
- Methods:
  - `GetLastPayload() -> s`
  - `GetWindowPayloads() -> s` (JSON object keyed by Hyprland window address)
  - `GetSchema() -> s`
- Signals:
  - `Updated(s payload_json)`
  - `WindowUpdated(s window_id, s payload_json)` (only for payloads with a Hyprland mapping)

## Correlation hints for consumers

Useful fields from signal payload:

- `chrome_window.id`: Chrome internal window id
- `chrome_window.left` / `top` / `width` / `height`: Chrome window bounds used for Hyprland matching
- `bridge.mapped_window.window_id`: Best Hyprland address (when known)
- `bridge.mapped_window.confidence`: Score for the chosen mapping
- `bridge.mapped_window.sources[]`: Signals that contributed to the chosen mapping
- `wm.hyprland_active.address`: Current active Hyprland window address
- `wm.hyprland_event_stream_connected`: Whether the daemon is connected to Hyprland's event socket
- `wm.hyprland_candidates[]`: Top scored Hyprland windows considered for this Chrome update
- `wm.hyprland_title_matches[]`: Hyprland windows with similar Chrome tab titles
- `wm.x11_active.id_decimal` / `wm.x11_active.id_hex`: Active X11 window id

## Optional: user systemd service

```bash
mkdir -p ~/.config/systemd/user
cp ~/Projects/chrome-favicon-dbus/systemd/chrome-favicon-dbus.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now chrome-favicon-dbus.service
```

The provided unit uses `%h/.nix-profile/bin/chrome-favicon-dbus`. If you install with `cargo install` instead, update `ExecStart` accordingly.

## Notes

- The daemon replies `{"ok": true, "mapped": <bool>, "hyprland": <bool>}` after the payload has been enriched and published. The extension only treats a snapshot as delivered when it was mapped (or no Hyprland backend exists), so unmapped windows are retried automatically.
- The extension re-pushes all windows every 30 seconds (`chrome.alarms` heartbeat) and retries failed sends with exponential backoff, so daemon restarts and dropped updates self-heal.
- Normal, popup, and app (PWA) windows are all published.
- If daemon is not running, extension retries with backoff and otherwise fails silently.
- Optional request auth is available with daemon `--token <value>` and extension support for `X-Bridge-Token`.
