//! Material Design Icons (MDI 7.4.47, Apache-2.0) embedded as SVG, see `assets/mdi/NOTICE`.
//! Each file is white (`fill="#fff"`) so that `Image::tint` gives it any colour.

use std::borrow::Cow;

use eframe::egui::{self, ImageSource, load::Bytes};

macro_rules! icons {
    ($($variant:ident => $file:literal,)*) => {
        /// Icon of the interface, drawn by egui's SVG loader (`theme::apply` installs it).
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Icon {
            $($variant,)*
        }

        impl Icon {
            /// Every icon, for the tests.
            pub const ALL: &[Icon] = &[$(Icon::$variant,)*];

            fn source(self) -> ImageSource<'static> {
                match self {
                    $(Icon::$variant => ImageSource::Bytes {
                        uri: Cow::Borrowed(concat!("bytes://mdi/", $file, ".svg")),
                        bytes: Bytes::Static(include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/mdi/", $file, ".svg"))),
                    },)*
                }
            }
        }
    };
}

icons! {
    Account => "account",
    Alert => "alert",
    Archive => "archive",
    Unarchive => "archive-arrow-up",
    Battery => "battery",
    Battery20 => "battery-20",
    Battery50 => "battery-50",
    Battery80 => "battery-80",
    BatteryAlert => "battery-alert",
    Bell => "bell",
    BellOff => "bell-off",
    Bluetooth => "bluetooth",
    BluetoothConnect => "bluetooth-connect",
    BluetoothOff => "bluetooth-off",
    Broom => "broom",
    Chat => "chat",
    Check => "check",
    ChevronDown => "chevron-down",
    ChevronUp => "chevron-up",
    Close => "close",
    Cog => "cog",
    Contacts => "contacts",
    Copy => "content-copy",
    Delete => "delete",
    Earth => "earth",
    EyeOff => "eye-off",
    Font => "format-font",
    History => "history",
    Info => "information-outline",
    Link => "link-variant",
    Magnify => "magnify",
    Message => "message",
    MessageText => "message-text",
    MicOff => "microphone-off",
    Phone => "phone",
    PhoneBluetooth => "phone-bluetooth",
    Plus => "plus",
    Refresh => "refresh",
    Emoji => "sticker-emoji",
    Translate => "translate",
}

impl Icon {
    /// Image of the icon, to size and tint at the call site.
    pub fn image(self) -> egui::Image<'static> {
        egui::Image::new(self.source())
    }

    /// Battery icon for a level `percent` (0..=100), in 5 tiers.
    #[must_use]
    pub fn battery(percent: u8) -> Self {
        match percent {
            0..10 => Self::BatteryAlert,
            10..35 => Self::Battery20,
            35..65 => Self::Battery50,
            65..90 => Self::Battery80,
            _ => Self::Battery,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_tiers() {
        use Icon::{Battery, Battery20, Battery50, Battery80, BatteryAlert};
        let tiers = [0, 9, 10, 34, 35, 64, 65, 89, 90, 100].map(Icon::battery);
        assert_eq!(tiers, [BatteryAlert, BatteryAlert, Battery20, Battery20, Battery50, Battery50, Battery80, Battery80, Battery, Battery]);
    }

    #[test]
    fn icons_are_white_svg() {
        for icon in Icon::ALL {
            let ImageSource::Bytes { uri, bytes } = icon.source() else { panic!("{icon:?}") };
            let svg = std::str::from_utf8(&bytes).unwrap();
            assert!(uri.ends_with(".svg"), "{icon:?}");
            assert!(svg.starts_with("<svg fill=\"#fff\""), "{icon:?}");
        }
    }
}
