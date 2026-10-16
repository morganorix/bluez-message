# Translations

Every text shown by `bluez-msg`, `bluez-messaged`, `bluez-message` and `bluez-msg-call` is written in English in the
source code and translated at run time from a JSON file: `{ "English source": "Translation" }`.

## Which language is used

The first variable set among `BLUEZ_MESSAGE_LANG`, `LC_ALL`, `LC_MESSAGES`, `LANG` gives the language
(`pt_BR.UTF-8` → `pt` then `pt_BR`). For each code, the files are applied in this order, later ones
overriding earlier ones:

1. the translation shipped in the binaries (`locales/<lang>.json`, currently `fr`);
2. your own file, `~/.config/bluez-message/locales/<lang>.json` (`$XDG_CONFIG_HOME` is honoured).

A key that is missing or empty falls back to English. With no file for the language, everything is English.

## Adding a language

```sh
mkdir -p ~/.config/bluez-message/locales
bluez-msg locale template > ~/.config/bluez-message/locales/es.json   # every key, empty values
# translate the values, then:
bluez-msg locale check ~/.config/bluez-message/locales/es.json         # missing / unknown keys, bad markers
```

- Keep the `{0}`, `{1}`… markers: they are replaced by values (number, name, error). You may reorder them
  (`"{1}/{0} {2}:{3}"` is the date `month/day hour:minute`; French uses `"{0}/{1} {2}:{3}"`), but a
  translation whose markers differ from the source is ignored.
- A partial file is fine: it completes or overrides the shipped translation.
- Files are read when each program starts: reopen the app, and run `systemctl --user restart bluez-messaged`
  for notifications and daemon messages.

## For developers

Wrap user-facing text in `t!("…")` (`t!("Sent to {0}", who)` for values). `locales/template.json` must list
exactly the keys used in `src/` and every shipped translation must be complete: `cargo test` checks both.
Logs (`tracing`) stay in English and are not translated.
