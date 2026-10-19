# bmessage (bluez-message)

[English](README.md) · **Français**

**SMS et appels d'un iPhone ou d'un Android sur un PC Linux, en Bluetooth uniquement** : aucun serveur tiers, aucun compte Apple ou
Google, aucune application à installer sur le téléphone. Écrit en Rust, interface egui.

Contrairement à KDE Connect / GSConnect, rien ne tourne sur le téléphone : le PC lui parle via les profils
Bluetooth standards (MAP, PBAP, HFP). Fonctionne avec l'**iPhone** comme avec Android.

![Démo](docs/demo.gif) <!-- TODO : GIF : SMS reçu → notification → réponse → appel entrant → décrocher -->

[![Licence : MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Release](https://img.shields.io/github/v/release/morganorix/bluez-message)](https://github.com/morganorix/bluez-message/releases)

- **Assistant de configuration** : active l'adaptateur Bluetooth, trouve et appaire le téléphone (code confirmé
  des deux côtés), puis indique ce qu'il faut activer sur celui-ci ; désappairage d'un appareil depuis l'assistant ;
  plusieurs appareils, chacun avec son propre historique.
- **SMS** : lecture et envoi via le profil Bluetooth MAP (par `bluez-obex`), emojis en couleur,
  réactions, liens cliquables avec aperçus facultatifs (titre et image), renvoi en cas d'échec.
- **Conversations** : onglets SMS et Appels, recherche dans les noms et dans le texte de tous les messages,
  sourdine, archivage, suppression locale, export vers un fichier JSON.
- **Contacts** : synchronisés par PBAP toutes les 6 h (noms dans les conversations, recherche).
- **Appels** : sonnerie, décrocher, refuser, raccrocher, couper le micro, mise en attente, double appel, appels sortants,
  réponses rapides par SMS après un refus. L'audio (mSBC) passe par PipeWire/WirePlumber dans le rôle HFP mains libres.
- **Notifications de bureau** (SMS avec Répondre, appel entrant avec Décrocher / Refuser / Réponse rapide),
  même fenêtre fermée ; niveau de batterie de l'iPhone et voyants de liaison dans l'en-tête.
- **Confidentialité et robustesse** : historique SQLite local (sauvegardé avant chaque migration), journaux sans
  numéros, noms, textes ni adresses, attentes bornées sur chaque échange Bluetooth, service systemd cloisonné,
  interface traduisible (anglais, français), aucune police système requise.

![Conversation](docs/bmessage-conversation.png)
![Réglages](docs/bmessage-settings.png)

## Compatibilité

| | Statut |
|---|---|
| iPhone (iOS) | SMS/MMS, appels, contacts. **Pas d'iMessage** (non exposé par MAP). |
| Android | SMS, appels, contacts. Autoriser le PC à accéder aux messages et aux contacts quand le téléphone le demande (le service MAP n'apparaît qu'une fois le partage autorisé). |
| Testé sur | Arch Linux, Hyprland, iPhone 11 et un téléphone Android |
| Autres distributions / bureaux | Non testé, retours bienvenus <!-- TODO : lister les configurations confirmées --> |

### Limites connues

- iOS n'expose pas iMessage via MAP (seulement les SMS/MMS).
- Les dossiers MAP `sent`/`outbox` sont vides sur l'iPhone : les messages envoyés depuis l'application sont donc enregistrés localement.
- Les réactions (tapbacks) sont envoyées comme de simples textes (« Liked “…” »).
- Si un téléphone oublie le PC, le désappairer aussi depuis l'assistant avant de réappairer.

## Composants

| Binaire | Rôle |
|---|---|
| `bluez-messaged` | Démon (service utilisateur systemd) : MAP, PBAP, appels, notifications, historique, socket pour les clients. |
| `bluez-message` | Fenêtre principale (entrée de menu : **bmessage**), cliente du démon. |
| `bluez-msg-call` | Petite fenêtre d'appel en cours, lancée par le démon quand un appel est décroché. |
| `bluez-msg` | Outil en ligne de commande (tests MAP/HFP, fichiers de traduction). |

## Prérequis

- Linux avec BlueZ, `bluez-obex`, PipeWire et WirePlumber (≥ 0.5, service `org.pipewire.Telephony`).
- Un serveur de notifications (mako, dunst, swaync…) pour les notifications.
- Vulkan recommandé (rendu plus léger), repli sur OpenGL sinon.
- Aucune police à installer : les icônes (Material Design Icons) et les emojis (Twemoji) sont intégrés.
- Pour compiler : Rust ≥ 1.91 (édition 2024).

## Installation

### Arch Linux

AUR : <!-- TODO : une fois publié -->

```sh
yay -S bluez-message
```

Paquet précompilé (x86_64) depuis les [releases](https://github.com/morganorix/bluez-message/releases),
signé avec la clé `E12F0EA82A218B98A786F25C6BF70F0B266A6EB9`. Une seule fois, faire confiance à la clé :

```sh
curl -sL https://github.com/morganorix/bluez-message/releases/download/v1.0.0/morganorix.asc | sudo pacman-key --add -
sudo pacman-key --lsign-key E12F0EA82A218B98A786F25C6BF70F0B266A6EB9
```

Puis installer :

```sh
sudo pacman -U https://github.com/morganorix/bluez-message/releases/download/v1.0.0/bluez-message-1.0.0-1-x86_64.pkg.tar.zst
```

Ou compiler depuis un clone du dépôt :

```sh
cd packaging/arch
makepkg -si
```

Le paquet installe les quatre binaires, le service `bluez-messaged` (activé pour chaque session
utilisateur), l'entrée de menu **bmessage**, l'icône et la configuration WirePlumber. Détails :
[`packaging/arch/README.md`](packaging/arch/README.md).

### Autres distributions (manuel)

```sh
cargo build --release --all-features
install -Dm755 -t ~/.local/bin target/release/{bluez-msg,bluez-messaged,bluez-message,bluez-msg-call}

# Service utilisateur (ExecStart pointé sur ~/.local/bin/bluez-messaged)
install -Dm644 packaging/arch/bluez-messaged.service ~/.config/systemd/user/bluez-messaged.service
sed -i "s#/usr/bin/#$HOME/.local/bin/#" ~/.config/systemd/user/bluez-messaged.service

# Entrée de menu et icône
install -Dm644 packaging/arch/bluez-message.desktop ~/.local/share/applications/bluez-message.desktop
install -Dm644 packaging/arch/bluez-message.svg ~/.local/share/icons/hicolor/scalable/apps/bluez-message.svg

# Audio des appels (rôle HFP mains libres + gain sur l'audio reçu)
install -Dm644 -t ~/.config/wireplumber/wireplumber.conf.d packaging/arch/5{2,3}-*.conf
install -Dm644 -t ~/.local/share/wireplumber/scripts packaging/arch/bluez-message-call-gain.lua

systemctl --user daemon-reload
systemctl --user restart wireplumber
systemctl --user enable --now bluez-messaged
```

Après le redémarrage de WirePlumber, reconnecter l'iPhone (`bluetoothctl disconnect` puis `connect`).

### Volume des appels

L'audio reçu pendant un appel est amplifié de +18 dB (gain linéaire `8.0`), car l'iPhone l'envoie très bas
en HFP (environ -34 dBFS en crête). Pour le modifier, copier le script dans
`~/.local/share/wireplumber/scripts/bluez-message-call-gain.lua` (il est prioritaire sur
`/usr/share/wireplumber/scripts/`), éditer `local GAIN = 8.0` (`1.0` = aucun gain, `4.0` ≈ +12 dB), puis
`systemctl --user restart wireplumber` et reconnecter l'iPhone.

## Premiers pas

1. Ouvrir **bmessage** depuis le menu. Sans appareil configuré, l'assistant de configuration s'ouvre (également
   accessible depuis **Réglages > Appareils > Configurer un appareil**) :
   1. **Adaptateur Bluetooth** : l'activer si nécessaire.
   2. **Appareil** : rechercher, puis appairer le téléphone (confirmer le même code des deux côtés), ou choisir un
      appareil déjà appairé, ou saisir son adresse.
   3. **Réglages du téléphone** : sur iPhone, dans Réglages > Bluetooth > (i) à côté du PC, activer **Afficher
      les notifications** (MAP) et **Synchroniser les contacts** (PBAP) ; sur Android, autoriser l'accès aux messages et aux contacts
      pour le PC dans la demande d'appairage ou dans les réglages de l'appareil Bluetooth ; les voyants Appels (HFP), Messages (MAP) et Contacts (PBAP)
      passent au vert. iOS demande aussi une autorisation au premier accès.
   4. **Terminé**.

   Équivalent manuel : appairer avec `bluetoothctl` (`pair`, `trust`, `connect <adresse>`), écrire l'adresse
   `AA:BB:CC:DD:EE:FF` dans `~/.config/bluez-message/device`, puis `systemctl --user restart bluez-messaged`.

## Utilisation

### Fenêtre principale

- Colonne de gauche, « Contacts » : onglets **SMS** et **Appels** (demi-largeur chacun). Les fixes n'apparaissent que dans Appels,
  sous le nom du contact quand ils figurent dans le carnet d'adresses. Champ « Rechercher » (accents ignorés, format local ou `+33…` ;
  Entrée ouvre le premier résultat). Dès 2 caractères, il cherche aussi dans le texte de tous les messages
  (section « Messages », accents et casse ignorés) : un clic ouvre la conversation sur ce message.
- Clic droit sur une conversation : Sourdine (pas de notification de SMS, les appels sonnent toujours), Archiver (déplacée dans la
  section repliable « Archivées (N) » jusqu'au prochain SMS entrant), Supprimer.
- Fil de discussion : envoi, bouton emoji, bouton d'appel dans l'en-tête ; « Messages plus anciens » en haut charge le reste
  de l'historique. Les liens web (http/https) sont cliquables.
- Clic droit sur une bulle : réaction, Renvoyer, Copier, Supprimer (suppression locale uniquement, jamais sur l'iPhone).
- Un message en attente affiche une barre de progression ; si l'envoi échoue, un bouton le renvoie.
- En-tête : voyants Appels (HFP), Messages (MAP) et Contacts (PBAP), et niveau de batterie de l'iPhone (rouge
  sous 20 %).
- Notification de nouveau SMS : **Répondre** ouvre la fenêtre sur la conversation (lancée si elle était fermée).

Raccourcis : `↑`/`↓` + `Entrée` dans la liste, `Échap`, `Ctrl+F` (recherche), `Ctrl+,` (réglages).

### Appels

Quand le téléphone sonne, une notification propose **Décrocher** / **Refuser** et, pour un numéro de mobile,
**RÉPONSE RAPIDE** : elle refuse l'appel et ouvre la conversation (la fenêtre est lancée si besoin), avec les réponses
rapides sous forme de pastilles au-dessus de la zone de saisie : un clic la remplit, rien n'est envoyé automatiquement. Dans la fenêtre, le
bandeau d'appel propose les mêmes choix. Une fois l'appel établi, la fenêtre
`bluez-msg-call` affiche la durée et les boutons Muet, Attente, Raccrocher (plus Décrocher / Refuser /
Basculer pendant un double appel). Elle se ferme d'elle-même à la fin de l'appel.

### Réglages

Onglets :
- **Général** : langue (appliquée à chaud), pays du numéro de téléphone (Automatique : d'après la langue ou la
  locale ; sert à reconnaître les fixes et à regrouper un numéro écrit `06…` ou `+33…`), Aperçus de liens
  (désactivés par défaut : titre et image sous un lien, récupérés par la fenêtre en https, ce qui révèle votre
  adresse IP au site visé ; adresses privées refusées).
- **Notifications** : nouveaux SMS, appels entrants, masquer le texte des messages.
- **Appareils** : appareil Bluetooth, Connexion automatique, Configurer un appareil.
- **Historique** : durée de conservation (30 / 90 / 365 jours), Exporter l'historique (fichier JSON dans le dossier Téléchargements,
  ou le dossier personnel, lisible par vous seul, seul export de la fenêtre ; messages supprimés exclus), purger les messages supprimés, Effacer l'historique (double
  confirmation, les contacts sont conservés).
- **Réponses** : réponses rapides proposées sous forme de pastilles après **RÉPONSE RAPIDE** (5 au maximum, 160 caractères chacune).

### Ligne de commande

```sh
bluez-msg list <adresse> [inbox|sent|outbox|deleted]   # JSON, un message par ligne
bluez-msg send <adresse> <numéro> "<texte>"
bluez-msg watch <adresse> [--interval 5]
bluez-msg serve <adresse>                              # moteur piloté en JSON sur stdin/stdout
bluez-msg hfp <adresse> [--answer]                     # HFP RFCOMM brut, sans audio (hfp_hf désactivé)
bluez-msg export [--key <numéro>]                      # via le démon : tout l'historique ou une conversation
bluez-msg locale template | check <fichier>
bluez-message --open <numéro>                          # fenêtre sur cette conversation
```

## Fichiers

| Chemin | Contenu |
|---|---|
| `~/.config/bluez-message/device` | Adresse Bluetooth de l'iPhone (0600). |
| `~/.config/bluez-message/settings.json` | Réglages (écrits par le démon uniquement). |
| `~/.config/bluez-message/locales/<langue>.json` | Vos propres traductions. |
| `~/.local/share/bluez-message/history.db` | Historique SQLite (SMS, appels, contacts, marques de lecture). Les numéros sont indexés au format international ; une ancienne base est migrée au démarrage, après copie vers `history.db.bak` (format des clés) ou `history.db.v<N>.bak` (schéma). Export : `bluez-message-<date>[-<n>].json` dans le dossier Téléchargements (`-2`…`-9` si le nom est pris). |
| `$XDG_RUNTIME_DIR/bluez-message.sock` | Socket du démon (lignes JSON). |
| `~/.config/control-bar/colors.plm` | Facultatif : palette de couleurs (modèle matugen), rechargée à chaud. |

Variable `BLUEZ_MESSAGE_LANG` : force la langue (`fr`, `en`…).
Variable `BLUEZ_MESSAGE_LOG` : niveau de journalisation (`info` par défaut, sans numéros, noms, textes ni adresses ;
`debug` pour le dépannage, par ex. `systemctl --user edit bluez-messaged` → `Environment=BLUEZ_MESSAGE_LOG=debug`).

### Hyprland (facultatif)

Classes de fenêtres : `bluez-message` (principale ; suggéré : flottante, centrée, 900×640) et `bluez-msg-call`
(appel ; suggéré : flottante, épinglée, 420×150, haut de l'écran).
Quand une notification ouvre une conversation, la fenêtre reçoit le focus via `hyprctl dispatch` (syntaxe Lua).

## Traductions

Les textes sont écrits en anglais dans le code et traduits à l'exécution depuis des fichiers JSON.
Pour ajouter une langue, voir [`locales/README.md`](locales/README.md).

## Organisation du code

```
src/
├── main.rs              CLI bluez-msg
├── bin/
│   ├── bluez-messaged.rs        démon : moteur, notifications, historique, socket, surveillance de la liaison
│   ├── bluez-message/           fenêtre principale (ui, convs, thread, composer, prefs, assistant de configuration)
│   └── bluez-msg-call/          fenêtre d'appel
├── engine.rs            moteur partagé : interrogation MAP, envoi, appels (commandes Cmd / événements Out)
├── map.rs, obex.rs      client MAP via bluez-obex (D-Bus)
├── bmsg.rs              encodage/décodage bMessage (SMS)
├── pbap.rs              contacts (vCard)
├── telephony.rs         client D-Bus org.pipewire.Telephony (appels, audio HFP géré par PipeWire)
├── hfp.rs               unité mains libres RFCOMM brute (commandes AT, CLI uniquement)
├── bluez.rs             état de l'appareil et reconnexion (org.bluez)
├── ipc.rs               protocole lignes JSON démon ↔ clients
├── history.rs, reads.rs base SQLite (migrations user_version), lectures lentes (recherche, pages, export)
├── setup.rs             agent d'appairage et contrôle de l'adaptateur (assistant de configuration)
├── serve.rs, log.rs     moteur lignes JSON sur stdin/stdout, journaux avec adresses masquées
├── settings.rs, config.rs, i18n.rs, phone.rs, numbering.rs, stamp.rs, addr.rs, error.rs
├── app/                 état partagé (conversations, appel), notifications, réactions
└── gui/                 thème, widgets, icônes (MDI), emojis (Twemoji), aperçus de liens, démarrage egui
assets/mdi/              SVG Material Design Icons (blancs), LICENSE, NOTICE
locales/                 fr.json (intégré), template.json, README
packaging/arch/          PKGBUILD, service systemd, .desktop, icône, configuration WirePlumber
packaging/aur/           PKGBUILD AUR (sources depuis le tag GitHub)
```

## Développement

```sh
cargo build --all-features
cargo test --all-features
cargo clippy --all-features --all-targets -- -D warnings
```

Chaque nouvelle chaîne `t!("…")` doit être ajoutée à `locales/template.json` (vérifié par le test
`template_matches_sources`).

## À propos de ce projet

« Vibe coded » : le code a été écrit par une IA ([Claude Code](https://claude.com/claude-code))
sous la direction de l'auteur, qui a fixé ses lignes directrices de structure, d'optimisation et de sécurité, et
l'a testé sur son propre matériel (iPhone 11 et un téléphone Android, Arch Linux, Hyprland).
Les rapports de bugs et retours sont bienvenus dans les [issues](https://github.com/morganorix/bluez-message/issues).

## Licence

[MIT](LICENSE)

Ressources intégrées :
- [Material Design Icons](https://pictogrammers.com/library/mdi/) (Apache-2.0), recolorées, voir [assets/mdi/NOTICE](assets/mdi/NOTICE).
- Graphismes [Twemoji](https://github.com/jdecked/twemoji) (CC-BY 4.0) via la crate `twemoji-assets`.
