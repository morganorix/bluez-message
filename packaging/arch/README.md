# Paquet Arch

Construire (depuis ce dossier ; installe `cargo` si besoin) :

    makepkg -s

Installer, ici ou sur un autre PC Arch (copier le `.pkg.tar.zst`, pacman tire les dépendances) :

    sudo pacman -U bluez-message-*.pkg.tar.zst

Le démon est activé pour tous les utilisateurs : il démarre, avec la config audio, à la prochaine
ouverture de session (refus individuel : `systemctl --user mask bluez-messaged`). Une seule fois :

    bluetoothctl            # pair <adresse>, trust <adresse>

et choisir l'iPhone dans bmessage > Paramètres > Appareil Bluetooth.

Mise à jour : `makepkg -sf`, `sudo pacman -U …`, puis `systemctl --user restart bluez-messaged`.

Les copies utilisateur (`~/.config/systemd/user/bluez-messaged.service`, `~/.local/share/applications/*.desktop`,
`~/.config/wireplumber/wireplumber.conf.d/5{2,3}-*.conf`) passent avant celles du paquet : les retirer.
