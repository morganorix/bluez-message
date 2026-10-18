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
    BluetoothConnect => "bluetooth-connect",
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
