# bmessage (bluez-message)

**English** · [Français](README.fr.md)

iPhone texts and calls on a Linux PC, over Bluetooth only: no third-party server, no Apple account, no app
to install on the phone. Written in Rust, egui interface.

> **"Vibe coded" project**: the code was written by an AI ([Claude Code](https://claude.com/claude-code))
> under the direction of the author, who set its structural, optimisation and security guidelines and
> tested it on his own hardware (iPhone 11, Arch Linux, Hyprland).

- **Setup assistant**: turns the Bluetooth adapter on, finds and pairs the iPhone (code confirmed on
  both sides), then checks the iPhone settings; several devices, each with its own history.
- **Texts**: read and send through the Bluetooth MAP profile (via `bluez-obex`), colour emojis,
  reactions, clickable links with optional previews (title and image), resend on failure.
- **Conversations**: SMS and Calls tabs, search in names and in the text of every message,
  mute, archive, local deletion, export to a JSON file.
- **Contacts**: synced over PBAP every 6 h (names in conversations, search).
- **Calls**: ringing, answer, decline, hang up, mute, hold, call waiting, outgoing calls, quick replies
  by text after declining. Audio (mSBC) goes through PipeWire/WirePlumber in the HFP hands-free role.
- **Desktop notifications** (texts with Reply, incoming call with Answer / Decline / Quick reply),
  even with the window closed; iPhone battery level and link lights in the header.
- **Privacy and robustness**: local SQLite history (backed up before each migration), logs without
  numbers, names, texts or addresses, bounded waits on every Bluetooth exchange, sandboxed systemd service,
  translatable interface (English, French), no system font needed.

![Conversation](docs/bmessage-conversation.png)
![Settings](docs/bmessage-settings.png)

> Known limits: iOS does not expose iMessage over MAP (only SMS/MMS text); the MAP `sent`/`outbox` folders
> are empty on the iPhone, so messages sent from the app are recorded locally; reactions (tapbacks) are sent
> as plain texts ("Liked “…”").

## Components

| Binary | Role |
|---|---|
| `bluez-messaged` | Daemon (systemd user service): MAP, PBAP, calls, notifications, history, socket for clients. |
| `bluez-message` | Main window (menu entry: **bmessage**), client of the daemon. |
| `bluez-msg-call` | Small ongoing-call window, started by the daemon when a call is answered. |
| `bluez-msg` | Command-line tool (MAP/HFP tests, translation files). |

## Requirements

- Linux with BlueZ, `bluez-obex`, PipeWire and WirePlumber (≥ 0.5, `org.pipewire.Telephony` service).
- A notification server (mako, dunst, swaync…) for notifications.
- Vulkan recommended (lighter rendering), OpenGL fallback otherwise.
- No font to install: icons (Material Design Icons) and emojis (Twemoji) are embedded.
- To build: Rust ≥ 1.91 (2024 edition).

## Installation

### Arch Linux

Prebuilt package (x86_64) from the [releases](https://github.com/morganorix/bluez-message/releases),
signed with key `E12F0EA82A218B98A786F25C6BF70F0B266A6EB9`. Once, trust the key:

```sh
curl -sL https://github.com/morganorix/bluez-message/releases/download/v1.0.0/morganorix.asc | sudo pacman-key --add -
sudo pacman-key --lsign-key E12F0EA82A218B98A786F25C6BF70F0B266A6EB9
```

Then install:

```sh
sudo pacman -U https://github.com/morganorix/bluez-message/releases/download/v1.0.0/bluez-message-1.0.0-1-x86_64.pkg.tar.zst
```

Or build it from a clone of the repository:

```sh
cd packaging/arch
makepkg -si
```

The package installs the four binaries, the `bluez-messaged` service (enabled for every user
session), the **bmessage** menu entry, the icon and the WirePlumber configuration. Details (French):
[`packaging/arch/README.md`](packaging/arch/README.md).

### Other distributions (manual)

```sh
cargo build --release --all-features
install -Dm755 -t ~/.local/bin target/release/{bluez-msg,bluez-messaged,bluez-message,bluez-msg-call}

# User service (ExecStart pointed at ~/.local/bin/bluez-messaged)
install -Dm644 packaging/arch/bluez-messaged.service ~/.config/systemd/user/bluez-messaged.service
sed -i "s#/usr/bin/#$HOME/.local/bin/#" ~/.config/systemd/user/bluez-messaged.service

# Menu entry and icon
install -Dm644 packaging/arch/bluez-message.desktop ~/.local/share/applications/bluez-message.desktop
install -Dm644 packaging/arch/bluez-message.svg ~/.local/share/icons/hicolor/scalable/apps/bluez-message.svg

# Call audio (HFP hands-free role + gain on received audio)
install -Dm644 -t ~/.config/wireplumber/wireplumber.conf.d packaging/arch/5{2,3}-*.conf
install -Dm644 -t ~/.local/share/wireplumber/scripts packaging/arch/bluez-message-call-gain.lua

systemctl --user daemon-reload
systemctl --user restart wireplumber
systemctl --user enable --now bluez-messaged
```

After restarting WirePlumber, reconnect the iPhone (`bluetoothctl disconnect` then `connect`).

### Call volume

Received call audio is amplified by +18 dB (linear gain `8.0`), because the iPhone sends it very low
over HFP (around -34 dBFS at peak). To change it, copy the script to
`~/.local/share/wireplumber/scripts/bluez-message-call-gain.lua` (it takes precedence over
`/usr/share/wireplumber/scripts/`), edit `local GAIN = 8.0` (`1.0` = no gain, `4.0` ≈ +12 dB), then
`systemctl --user restart wireplumber` and reconnect the iPhone.

## Getting started

1. Open **bmessage** from the menu. Without a configured device, the setup assistant opens (also
   available from **Settings > Devices > Set up a device**):
   1. **Bluetooth adapter**: turn it on if needed.
   2. **Device**: search, then pair the iPhone (confirm the same code on both sides), or pick an
      already paired one, or type its address.
   3. **iPhone settings**: in Settings > Bluetooth > (i) next to the PC, enable **Show Notifications**
      (MAP) and **Sync Contacts** (PBAP); the Calls (HFP), Messages (MAP) and Contacts (PBAP) lights
      turn green. iOS also asks for permission on first access.
   4. **Done**.

   Manual equivalent: pair with `bluetoothctl` (`pair`, `trust`, `connect <address>`), write the address
   `AA:BB:CC:DD:EE:FF` to `~/.config/bluez-message/device`, then `systemctl --user restart bluez-messaged`.

## Usage

### Main window

- Left column, "Contacts": **SMS** and **Calls** tabs (half width each). Landlines only appear in Calls,
  under the contact name when they are in the address book. "Search" field (accents ignored, local or `+33…` format;
  Enter opens the first result). From 2 characters it also searches the text of every message
  ("Messages" section, accents and case ignored): a click opens the conversation on that message.
- Right-click on a conversation: Mute (no SMS notification, calls still ring), Archive (moved to the
  collapsible "Archived (N)" section until the next incoming SMS), Delete.
- Thread: send, emoji button, call button in the header; "Older messages" at the top loads the rest
  of the history. Web links (http/https) are clickable.
- Right-click on a bubble: reaction, Resend, Copy, Delete (local deletion only, never on the iPhone).
- A pending message shows a progress bar; if sending fails, a button resends it.
- Header: Calls (HFP), Messages (MAP) and Contacts (PBAP) lights, and the iPhone battery level (red
  below 20 %).
- New SMS notification: **Reply** opens the window on the conversation (started if closed).

Shortcuts: `↑`/`↓` + `Enter` in the list, `Esc`, `Ctrl+F` (search), `Ctrl+,` (settings).

### Calls

When the phone rings, a notification offers **Answer** / **Decline**, plus, for a mobile number,
**QUICK REPLY**: it declines and opens the conversation (the window is started if needed), with the quick
replies as chips above the input: a click fills it, nothing is sent automatically. In the window, the call
banner offers the same choices. Once the call is up, the
`bluez-msg-call` window shows the duration and the Mute, Hold, Hang up buttons (plus Answer / Decline /
Swap during call waiting). It closes by itself when the call ends.

### Settings

Tabs:
- **General**: language (applied live), phone number country (Automatic: from the language or the
  locale; used to recognise landlines and to group a number written as `06…` or `+33…`), Link previews
  (off by default: title and image under a link, fetched by the window over https, which reveals your
  IP address to the linked site; private addresses refused).
- **Notifications**: new SMS, incoming calls, hide message text.
- **Devices**: Bluetooth device, Connect automatically, Set up a device.
- **History**: retention (30 / 90 / 365 days), Export history (JSON file in the Downloads folder,
  or the home folder, readable by you only, the only export in the window; deleted messages left out), purge deleted messages, Clear history (double
  confirmation, contacts are kept).
- **Replies**: quick replies offered as chips after **QUICK REPLY** (5 at most, 160 characters each).

### Command line

```sh
bluez-msg list <address> [inbox|sent|outbox|deleted]   # JSON, one message per line
bluez-msg send <address> <number> "<text>"
bluez-msg watch <address> [--interval 5]
bluez-msg serve <address>                              # engine driven by JSON over stdin/stdout
bluez-msg hfp <address> [--answer]                     # raw RFCOMM HFP, no audio (hfp_hf disabled)
bluez-msg export [--key <number>]                     # via the daemon: whole history or one conversation
bluez-msg locale template | check <file>
bluez-message --open <number>                          # window on that conversation
```

## Files

| Path | Content |
|---|---|
| `~/.config/bluez-message/device` | iPhone Bluetooth address (0600). |
| `~/.config/bluez-message/settings.json` | Settings (written by the daemon only). |
| `~/.config/bluez-message/locales/<lang>.json` | Your own translations. |
| `~/.local/share/bluez-message/history.db` | SQLite history (texts, calls, contacts, read marks). Numbers are keyed in international format; an older database is migrated at startup, after a copy to `history.db.bak` (key format) or `history.db.v<N>.bak` (schema). Export: `bluez-message-<date>[-<n>].json` in the Downloads folder (`-2`…`-9` if the name is taken). |
| `$XDG_RUNTIME_DIR/bluez-message.sock` | Daemon socket (JSON lines). |
| `~/.config/control-bar/colors.plm` | Optional: colour palette (matugen template), reloaded live. |

`BLUEZ_MESSAGE_LANG` variable: forces the language (`fr`, `en`…).
`BLUEZ_MESSAGE_LOG` variable: log level (`info` by default, without numbers, names, texts or addresses;
`debug` for troubleshooting, e.g. `systemctl --user edit bluez-messaged` → `Environment=BLUEZ_MESSAGE_LOG=debug`).

### Hyprland (optional)

Window classes: `bluez-message` (main; suggested: floating, centred, 900×640) and `bluez-msg-call`
(call; suggested: floating, pinned, 420×150, top of the screen).
When a notification opens a conversation, the window is focused with `hyprctl dispatch` (Lua syntax).

## Translations

Texts are written in English in the code and translated at run time from JSON files.
To add a language, see [`locales/README.md`](locales/README.md).

## Code layout

```
src/
├── main.rs              bluez-msg CLI
├── bin/
│   ├── bluez-messaged.rs        daemon: engine, notifications, history, socket, link watch
│   ├── bluez-message/           main window (ui, convs, thread, composer, prefs, setup assistant)
│   └── bluez-msg-call/          call window
├── engine.rs            shared engine: MAP polling, sending, calls (Cmd commands / Out events)
├── map.rs, obex.rs      MAP client over bluez-obex (D-Bus)
├── bmsg.rs              bMessage encoding/decoding (SMS)
├── pbap.rs              contacts (vCard)
├── telephony.rs         org.pipewire.Telephony D-Bus client (calls, HFP audio held by PipeWire)
├── hfp.rs               raw RFCOMM hands-free unit (AT commands, CLI only)
├── bluez.rs             device state and reconnection (org.bluez)
├── ipc.rs               JSON-lines protocol daemon ↔ clients
├── history.rs, reads.rs SQLite database (user_version migrations), slow reads (search, pages, export)
├── setup.rs             pairing agent and adapter control (setup assistant)
├── serve.rs, log.rs     JSON-lines engine on stdin/stdout, logs with masked addresses
├── settings.rs, config.rs, i18n.rs, phone.rs, numbering.rs, stamp.rs, addr.rs, error.rs
├── app/                 shared state (conversations, call), notifications, reactions
└── gui/                 theme, widgets, icons (MDI), emojis (Twemoji), link previews, egui startup
assets/mdi/              Material Design Icons SVG (white), LICENSE, NOTICE
locales/                 fr.json (embedded), template.json, README
packaging/arch/          PKGBUILD, systemd service, .desktop, icon, WirePlumber config
packaging/aur/           AUR PKGBUILD (sources from the GitHub tag)
```

## Development

```sh
cargo build --all-features
cargo test --all-features
cargo clippy --all-features --all-targets -- -D warnings
```

Every new `t!("…")` string must be added to `locales/template.json` (checked by the
`template_matches_sources` test).

## License

[MIT](LICENSE)

Embedded assets:
- [Material Design Icons](https://pictogrammers.com/library/mdi/) (Apache-2.0), recoloured, see [assets/mdi/NOTICE](assets/mdi/NOTICE).
- [Twemoji](https://github.com/jdecked/twemoji) graphics (CC-BY 4.0) through the `twemoji-assets` crate.
