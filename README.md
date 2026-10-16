# bmessage (bluez-message)

**English** · [Français](README.fr.md)

iPhone texts and calls on a Linux PC, over Bluetooth only: no third-party server, no Apple account, no app
to install on the phone. Written in Rust, egui interface.

> **"Vibe coded" project**: the code was written by an AI ([Claude Code](https://claude.com/claude-code))
> under the direction of the author, who set its structural, optimisation and security guidelines and
> tested it on his own hardware (iPhone 11, Arch Linux, Hyprland).

- **Texts**: read and send through the Bluetooth MAP profile (via `bluez-obex`).
- **Contacts**: synced over PBAP every 6 h (names in conversations, search).
- **Calls**: ringing, answer, decline, hang up, mute, hold, call waiting, outgoing calls.
  Audio (mSBC) goes through PipeWire/WirePlumber in the HFP hands-free role.
- **Desktop notifications** (texts, incoming call with Answer / Decline buttons), even with the window closed.
- **Local history** in SQLite, local deletion, reactions, translatable interface (English, French).

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
- Recommended fonts: JetBrainsMono Nerd Font (icons), Noto Color Emoji.
- To build: Rust ≥ 1.91 (2024 edition).

## Installation

### Arch Linux

Prebuilt package (x86_64) from the [releases](https://github.com/morganorix/bluez-message/releases),
signed with key `E12F0EA82A218B98A786F25C6BF70F0B266A6EB9`. Once, trust the key:

```sh
curl -sL https://github.com/morganorix/bluez-message/releases/download/v0.1.3/morganorix.asc | sudo pacman-key --add -
sudo pacman-key --lsign-key E12F0EA82A218B98A786F25C6BF70F0B266A6EB9
```

Then install:

```sh
sudo pacman -U https://github.com/morganorix/bluez-message/releases/download/v0.1.3/bluez-message-0.1.3-1-x86_64.pkg.tar.zst
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

## Getting started

1. Pair the iPhone once:
   ```sh
   bluetoothctl
   # power on, scan on, pair <address>, trust <address>, connect <address>
   ```
2. On the iPhone, in Settings > Bluetooth > (i) next to the PC: enable **Show Notifications** (MAP) and
   **Sync Contacts** (PBAP). iOS also asks for permission on first access.
3. Open **bmessage** from the menu, then **Settings > Bluetooth device**: pick the iPhone.
   (Manual equivalent: write the address `AA:BB:CC:DD:EE:FF` to `~/.config/bluez-message/device`, then
   `systemctl --user restart bluez-messaged`.)

## Usage

### Main window

- Left column: conversations, "Name or number" search field (accents ignored, local or `+33…` format;
  Enter opens the first result).
- Thread: send, emoji button, call button in the header.
- Right-click on a bubble: reaction, Resend, Copy, Delete (local deletion only, never on the iPhone).
- A pending message shows a progress bar; if sending fails, a button resends it.

Shortcuts: `↑`/`↓` + `Enter` in the list, `Esc`, `Ctrl+F` (search), `Ctrl+,` (settings).

### Calls

When the phone rings, a notification offers **Answer** / **Decline**. Once the call is up, the
`bluez-msg-call` window shows the duration and the Mute, Hold, Hang up buttons (plus Answer / Decline /
Swap during call waiting). It closes by itself when the call ends.

### Settings

Language (applied live), text / call notifications / hide message text, Bluetooth device, Reconnect,
retention (30 / 90 / 365 days), purge deleted messages, Erase everything (double confirmation, contacts
are kept).

### Command line

```sh
bluez-msg list <address> [inbox|sent|outbox|deleted]   # JSON, one message per line
bluez-msg send <address> <number> "<text>"
bluez-msg watch <address> [--interval 5]
bluez-msg serve <address>                              # engine driven by JSON over stdin/stdout
bluez-msg hfp <address> [--answer]                     # raw RFCOMM HFP, no audio (hfp_hf disabled)
bluez-msg locale template | check <file>
```

## Files

| Path | Content |
|---|---|
| `~/.config/bluez-message/device` | iPhone Bluetooth address (0600). |
| `~/.config/bluez-message/settings.json` | Settings (written by the daemon only). |
| `~/.config/bluez-message/locales/<lang>.json` | Your own translations. |
| `~/.local/share/bluez-message/history.db` | SQLite history (texts, calls, contacts, read marks). |
| `$XDG_RUNTIME_DIR/bluez-message.sock` | Daemon socket (JSON lines). |
| `~/.config/control-bar/colors.plm` | Optional: colour palette (matugen template), reloaded live. |

`BLUEZ_MESSAGE_LANG` variable: forces the language (`fr`, `en`…).
`BLUEZ_MESSAGE_LOG` variable: log level (`info` by default, without numbers, names, texts or addresses;
`debug` for troubleshooting, e.g. `systemctl --user edit bluez-messaged` → `Environment=BLUEZ_MESSAGE_LOG=debug`).

### Hyprland (optional)

Window classes: `bluez-message` (main; suggested: floating, centred, 900×640) and `bluez-msg-call`
(call; suggested: floating, pinned, 420×150, top of the screen).

## Translations

Texts are written in English in the code and translated at run time from JSON files.
To add a language, see [`locales/README.md`](locales/README.md).

## Code layout

```
src/
├── main.rs              bluez-msg CLI
├── bin/
│   ├── bluez-messaged.rs        daemon: engine, notifications, history, socket, link watch
│   ├── bluez-message/           main window (ui, convs, thread, composer, prefs)
│   └── bluez-msg-call/          call window
├── engine.rs            shared engine: MAP polling, sending, calls (Cmd commands / Out events)
├── map.rs, obex.rs      MAP client over bluez-obex (D-Bus)
├── bmsg.rs              bMessage encoding/decoding (SMS)
├── pbap.rs              contacts (vCard)
├── telephony.rs         org.pipewire.Telephony D-Bus client (calls, HFP audio held by PipeWire)
├── hfp.rs               raw RFCOMM hands-free unit (AT commands, CLI only)
├── bluez.rs             device state and reconnection (org.bluez)
├── ipc.rs               JSON-lines protocol daemon ↔ clients
├── history.rs           SQLite database (user_version migrations)
├── settings.rs, config.rs, i18n.rs, phone.rs, stamp.rs, addr.rs, error.rs
├── app/                 shared state (conversations, call), notifications, reactions
└── gui/                 theme, widgets, emojis, egui startup
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
