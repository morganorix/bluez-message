# bmessage (bluez-message)

**English** · [Français](README.fr.md)

**iPhone and Android texts and calls on a Linux PC, over Bluetooth only**: no third-party server, no Apple or
Google account, no app to install on the phone. Written in Rust, egui interface.

Unlike KDE Connect / GSConnect, nothing runs on the phone: the PC talks to it through standard Bluetooth
profiles (MAP, PBAP, HFP). Works with **iPhone** as well as Android.

![Demo](docs/demo.gif) <!-- TODO: GIF: incoming SMS → notification → reply → incoming call → answer -->

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/morganorix/bluez-message)](https://github.com/morganorix/bluez-message/releases)

- **Setup assistant**: turns the Bluetooth adapter on, finds and pairs the phone (code confirmed on
  both sides), then tells you what to enable on it; unpair a device from the assistant; several devices,
  each with its own history.
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

## Compatibility

| | Status |
|---|---|
| iPhone (iOS) | Texts (SMS/MMS), calls, contacts. **No iMessage** (not exposed over MAP). |
| Android | Texts, calls, contacts. Allow the PC to access messages and contacts when the phone asks (the MAP service only appears once sharing is allowed). |
| Tested on | Arch Linux, Hyprland, iPhone 11 and one Android phone |
| Other distributions / desktops | Untested, reports welcome <!-- TODO: list confirmed setups --> |

### Known limits

- iOS does not expose iMessage over MAP (only SMS/MMS text).
- The MAP `sent`/`outbox` folders are empty on the iPhone, so messages sent from the app are recorded locally.
- Reactions (tapbacks) are sent as plain texts ("Liked “…”").
- If a phone forgets the PC, unpair it from the assistant too before pairing again.

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

AUR: <!-- TODO: once published -->

```sh
yay -S bluez-message
```

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
   2. **Device**: search, then pair the phone (confirm the same code on both sides), or pick an
      already paired one, or type its address.
   3. **Phone settings**: on iPhone, in Settings > Bluetooth > (i) next to the PC, enable **Show
      Notifications** (MAP) and **Sync Contacts** (PBAP); on Android, allow access to messages and contacts
      for the PC in
