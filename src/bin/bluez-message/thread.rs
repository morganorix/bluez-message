//! Conversation thread: header (title, number, Call), bubbles, call lines, reactions.
//! Changes neither the state nor the daemon: returns the requested actions.

use std::borrow::Cow;
use std::sync::Arc;

use bluez_message::app::state::{Conv, Delivery, Msg, Rev};
use bluez_message::app::tapback::{self, Badge};
use bluez_message::engine::{CallKind, Item};
use bluez_message::gui::emoji::{self, Emojis};
use bluez_message::gui::icons::Icon;
use bluez_message::gui::link;
use bluez_message::gui::preview::{Card, Previews};
use bluez_message::gui::theme::{CORNER, Palette, font};
use bluez_message::gui::widgets as w;
use bluez_message::t;
use eframe::egui::{self, Align, Align2, Frame, Layout, Margin, ScrollArea};

/// Action requested in the thread, applied by the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThreadAction {
    Resend(String),
    React(&'static str, String),
    Delete(Item),
    Dial,
}

/// Thread of conversation `c`, from the state at revision `rev`; `calls_only`: call lines only;
/// `previews`: link previews, `None` when disabled.
pub fn conv(ui: &mut egui::Ui, pal: &Palette, emojis: &mut Emojis, mut previews: Option<&mut Previews>, c: &Conv, rev: Rev, calls_only: bool) -> Vec<ThreadAction> {
    let dial = header(ui, pal, &c.name, &c.number);
    if calls_only && c.last_call().is_none() {
        placeholder(ui, pal, t!("No calls"));
        return if dial { vec![ThreadAction::Dial] } else { vec![] };
    }
    let max_w = ui.available_width() * 0.7;
    let mut acts = vec![];
    // First frame of the thread: emojis already present do not animate, only new ones.
    let now = ui.input(|i| i.time);
    let opened = ui.data_mut(|d| *d.get_temp_mut_or_insert_with(egui::Id::new(("thread", &c.key)), || now));
    ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = w::GAP;
        // Reactions folded into a badge on the targeted message: computed again only when the state changes.
        let fold = folded(ui, c, rev);
        // Rank of each emoji message among those with the same text and direction: stable identity
        // despite a deletion, a history loaded earlier or a pending bubble replaced at send time.
        let mut rank: std::collections::HashMap<(&str, bool), usize> = std::collections::HashMap::new();
        for (i, m) in c.msgs.iter().enumerate() {
            if fold.folded[i] || (calls_only && m.call.is_none()) {
                continue;
            }
            if let Some((kind, secs)) = m.call {
                call_line(ui, pal, m, kind, secs, &mut acts);
                continue;
            }
            if emoji::only(&m.text) {
                let n = rank.entry((m.text.as_str(), m.outgoing)).or_default();
                let id = egui::Id::new(("emoji", &c.key, &m.text, m.outgoing, *n));
                *n += 1;
                let seen = ui.data_mut(|d| *d.get_temp_mut_or_insert_with(id, || now));
                big(ui, pal, emojis, m, &fold.badges[i], (seen > opened).then_some(now - seen), &mut acts);
                continue;
            }
            bubble(ui, pal, emojis, previews.as_deref_mut(), m, &fold.badges[i], max_w, &mut acts);
        }
    });
    if dial {
        acts.push(ThreadAction::Dial);
    }
    acts
}

/// [`tapback::fold`] of `c`, kept in egui's memory as long as the state stays at revision `rev`.
fn folded(ui: &egui::Ui, c: &Conv, rev: Rev) -> Arc<tapback::Fold> {
    let id = egui::Id::new(("fold", &c.key));
    if let Some((r, f)) = ui.data(|d| d.get_temp::<(Rev, Arc<tapback::Fold>)>(id))
        && r == rev
    {
        return f;
    }
    let f = Arc::new(tapback::fold(&c.msgs));
    ui.data_mut(|d| d.insert_temp(id, (rev, Arc::clone(&f))));
    f
}

/// Contact or number without a conversation: header only; `true` if Call is clicked.
pub fn contact(ui: &mut egui::Ui, pal: &Palette, name: &str, number: &str, calls_only: bool) -> bool {
    let dial = header(ui, pal, name, number);
    placeholder(ui, pal, if calls_only { t!("No calls yet") } else { t!("No messages yet: write below or call") });
    dial
}

/// Text centred in the thread area.
pub fn placeholder(ui: &mut egui::Ui, pal: &Palette, msg: &str) {
    ui.centered_and_justified(|ui| ui.label(w::text(msg, font::BODY, pal.muted())));
}

/// Centred call line, like a `Hint`: 11 at 40 % (missed in danger); deletion on right click.
fn call_line(ui: &mut egui::Ui, pal: &Palette, m: &Msg, kind: CallKind, secs: u64, acts: &mut Vec<ThreadAction>) {
    let color = if kind == CallKind::Missed { pal.danger } else { pal.muted() };
    let r = ui.vertical_centered(|ui| {
        let label = w::text(format!("{}  ·  {}", call_label(kind, secs), short_stamp(&m.timestamp)), font::SMALL, color);
        ui.add(egui::Button::image_and_text(w::small_icon(Icon::Phone, color), label).frame(false))
    });
    r.inner.context_menu(|ui| {
        if w::menu_item(ui, Icon::Delete, t!("Delete")).clicked() {
            acts.push(ThreadAction::Delete(m.item()));
        }
    });
}

/// SMS bubble: pending (progress bar), failed (resend button), right-click menu
/// (react, resend, copy, delete); reactions as badges on the top corner; preview of the first
/// link below the text, once loaded.
#[expect(clippy::too_many_arguments)]
fn bubble(ui: &mut egui::Ui, pal: &Palette, emojis: &mut Emojis, previews: Option<&mut Previews>, m: &Msg, badges: &[Badge], max_w: f32, acts: &mut Vec<ThreadAction>) {
    if !badges.is_empty() {
        ui.add_space(BADGE_R);
    }
    let layout = if m.outgoing { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
    // Height 0: the row takes the bubble's height, not that of the scrolling area.
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 0.0), layout, |ui| {
        // MapInfo inset (`fg` 4 %, 12 margin); mine in accent 15 %, like the chips.
        let fill = match (m.outgoing, m.delivery) {
            (_, Delivery::Failed) => pal.danger.gamma_multiply(0.15),
            (true, _) => pal.accent.gamma_multiply(0.15),
            (false, _) => pal.dim(0.04),
        };
        let ink = if m.delivery == Delivery::Pending { pal.fg.gamma_multiply(0.6) } else { pal.fg };
        let frame = Frame::new().fill(fill).corner_radius(CORNER).inner_margin(Margin::symmetric(12, 8)).show(ui, |ui| {
            ui.set_max_width(max_w);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                let t = emojis.label(ui, &m.text, font::BODY, ink);
                if let Some(p) = previews
                    && let Some(url) = link::links(&m.text).first().copied()
                    && let Some(c) = p.get(ui.ctx(), url)
                {
                    card(ui, pal, url, c);
                }
                match m.delivery {
                    Delivery::Pending => progress(ui, pal, t.rect.width().max(90.0)),
                    Delivery::Failed => drop(ui.label(w::text(t!("Not sent"), font::TINY, pal.danger))),
                    Delivery::Done => drop(ui.label(w::text(short_stamp(&m.timestamp), font::TINY, pal.muted()))),
                }
            });
        });
        actions(ui, pal, emojis, m, &frame.response, badges, acts);
    });
}

/// Height of the thumbnail in a preview card.
const CARD_THUMB: f32 = 48.0;

/// Preview card of `url`: thumbnail, bold title, description on 2 lines, domain; a click opens `url`.
fn card(ui: &mut egui::Ui, pal: &Palette, url: &str, c: &Card) {
    let r = Frame::new().fill(pal.dim(0.06)).corner_radius(CORNER).inner_margin(Margin::same(6)).show(ui, |ui| {
        ui.horizontal(|ui| {
            if let Some(tex) = &c.image {
                let [tw, th] = tex.size().map(|v| v.max(1) as f32);
                let size = egui::vec2((tw * CARD_THUMB / th).min(2.0 * CARD_THUMB), CARD_THUMB);
                ui.add(egui::Image::new((tex.id(), size)).corner_radius(CORNER / 2));
            }
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                let wrap = ui.available_width();
                let rows = |s: &str, font: egui::FontId, color, max_rows| {
                    let mut job = egui::text::LayoutJob::simple(s.to_owned(), font, color, wrap);
                    job.wrap.max_rows = max_rows;
                    job
                };
                let title = rows(&c.title, w::bold_font(font::BODY), pal.fg, 2);
                ui.add(egui::Label::new(title).selectable(false));
                if !c.description.is_empty() {
                    let desc = rows(&c.description, w::regular(font::SMALL), pal.muted(), 2);
                    ui.add(egui::Label::new(desc).selectable(false));
                }
                ui.add(egui::Label::new(w::text(&c.domain, font::TINY, pal.muted())).selectable(false));
            });
        });
    });
    let r = r.response.interact(egui::Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
    r.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Link, true, &c.title));
    if r.clicked() {
        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
    }
}

/// After the bubble (or emojis) of `m`: badges, resend button on failure, right-click menu
/// (react, resend, copy, delete); no menu while sending.
fn actions(ui: &mut egui::Ui, pal: &Palette, emojis: &mut Emojis, m: &Msg, shown: &egui::Response, badges: &[Badge], acts: &mut Vec<ThreadAction>) {
    badges_on(ui, pal, emojis, shown.rect, m.outgoing, badges);
    if m.delivery == Delivery::Failed && w::icon_button(ui, pal, Icon::Refresh).on_hover_text(t!("Resend")).clicked() {
        acts.push(ThreadAction::Resend(m.text.clone()));
    }
    if m.delivery == Delivery::Pending {
        return;
    }
    shown.interact(egui::Sense::click()).context_menu(|ui| {
        if !m.outgoing {
            ui.horizontal(|ui| {
                for (emoji, _) in tapback::KINDS {
                    if ui.add(emojis.button(ui.ctx(), emoji, 20.0, pal.fg).frame(false)).on_hover_text(tapback::label(emoji)).clicked() {
                        acts.push(ThreadAction::React(emoji, m.text.clone()));
                        ui.close();
                    }
                }
            });
            ui.separator();
        }
        if m.outgoing && w::menu_item(ui, Icon::Refresh, t!("Resend")).clicked() {
            acts.push(ThreadAction::Resend(m.text.clone()));
        }
        if w::menu_item(ui, Icon::Copy, t!("Copy")).clicked() {
            ui.ctx().copy_text(m.text.clone());
        }
        if w::menu_item(ui, Icon::Delete, t!("Delete")).clicked() {
            acts.push(ThreadAction::Delete(m.item()));
        }
    });
}

/// Duration of an emoji message's appearance (bounce then sway), in seconds.
const POP: f64 = 1.2;

/// Emoji-only message: large, frameless, timestamp (or send state) below;
/// `age`: seconds since it appeared on screen, `None` if it was there when the thread opened.
fn big(ui: &mut egui::Ui, pal: &Palette, emojis: &mut Emojis, m: &Msg, badges: &[Badge], age: Option<f64>, acts: &mut Vec<ThreadAction>) {
    if !badges.is_empty() {
        ui.add_space(BADGE_R);
    }
    let layout = if m.outgoing { Layout::right_to_left(Align::Min) } else { Layout::left_to_right(Align::Min) };
    ui.allocate_ui_with_layout(egui::vec2(ui.available_width(), 0.0), layout, |ui| {
        let ink = if m.delivery == Delivery::Pending { pal.fg.gamma_multiply(0.6) } else { pal.fg };
        // Stuck-together colour emojis (spaces removed); without the font, the text as is.
        let compact: String = m.text.split_whitespace().collect();
        let text = m.text.trim();
        let col = Layout::top_down(if m.outgoing { Align::Max } else { Align::Min });
        let shown = ui
            .with_layout(col, |ui| {
                ui.spacing_mut().item_spacing.y = 3.0;
                let wide = emojis.width(ui.ctx(), &compact, font::EMOJI);
                let wide = wide.unwrap_or_else(|| ui.painter().layout_no_wrap(text.to_owned(), w::regular(font::EMOJI), ink).size().x);
                // Glyph height, without the font's line spacing: timestamp close to the emojis.
                let (rect, resp) = ui.allocate_exact_size(egui::vec2(wide, font::EMOJI), egui::Sense::hover());
                pop(ui, emojis, rect, &compact, text, ink, age);
                match m.delivery {
                    Delivery::Pending => progress(ui, pal, rect.width().max(40.0)),
                    Delivery::Failed => drop(ui.label(w::text(t!("Not sent"), font::TINY, pal.danger))),
                    Delivery::Done => drop(ui.label(w::text(short_stamp(&m.timestamp), font::TINY, pal.muted()))),
                }
                resp
            })
            .inner;
        actions(ui, pal, emojis, m, &shown, badges, acts);
    });
}

/// Draws `compact` as colour emojis (otherwise `text`) large at the centre of `rect`; during [`POP`] after its arrival: pops up
/// slightly overshooting its size, then sways with damping.
fn pop(ui: &egui::Ui, emojis: &mut Emojis, rect: egui::Rect, compact: &str, text: &str, ink: egui::Color32, age: Option<f64>) {
    let (scale, angle) = match age {
        Some(a) if a < POP => {
            ui.ctx().request_repaint();
            let a = a as f32;
            // easeOutBack over 0.45 s: from 0 to 1 passing through ~1.1.
            let (c1, x) = (1.701_58_f32, (a / 0.45).min(1.0) - 1.0);
            let grow = 1.0 + (c1 + 1.0) * x.powi(3) + c1 * x.powi(2);
            let damp = 1.0 - a / POP as f32;
            (grow.max(0.1), 0.2 * (a * std::f32::consts::TAU * 2.5).sin() * damp)
        }
        _ => (1.0, 0.0),
    };
    if emojis.paint(ui, compact, rect.center(), font::EMOJI * scale, angle, ink) {
        return;
    }
    // Size rounded to 2 px: few distinct sizes to rasterise in the glyph atlas.
    let size = (font::EMOJI * scale / 2.0).round().max(1.0) * 2.0;
    let g = ui.painter().layout_no_wrap(text.to_owned(), w::regular(size), ink);
    // Rotation around the centre: TextShape rotates around its top-left corner.
    let pos = rect.center() - egui::emath::Rot2::from_angle(angle) * (g.size() / 2.0);
    ui.painter().add(egui::epaint::TextShape::new(pos, g, ink).with_angle(angle));
}

const BADGE_R: f32 = 11.0;
/// Size of a colour reaction emoji (slightly under the pill's diameter).
const BADGE_EMOJI: f32 = 18.0;

/// Reaction badges straddling the bubble's top corner, like Messages: on the right for a received
/// message, on the left for a sent one; colour emoji without a pill, or as text on a pill
/// (mine in accent, the correspondent's in surface).
fn badges_on(ui: &egui::Ui, pal: &Palette, emojis: &mut Emojis, bubble: egui::Rect, outgoing: bool, badges: &[Badge]) {
    let p = ui.painter();
    let surface = pal.bg.lerp_to_gamma(pal.fg, 0.18);
    for (k, b) in badges.iter().enumerate() {
        // Several badges overlap slightly towards the inside of the bubble.
        let dx = 4.0 + k as f32 * (BADGE_R * 2.0 - 4.0);
        let c = egui::pos2(if outgoing { bubble.left() + dx } else { bubble.right() - dx }, bubble.top() - 2.0);
        let (fill, ink) = if b.mine { (pal.accent, pal.on_accent) } else { (surface, pal.fg) };
        let a = if b.pending { 0.5 } else { 1.0 };
        // Colour emoji placed as is; as text (without the font), on a pill to stay legible.
        if !emojis.paint(ui, b.emoji, c, BADGE_EMOJI, 0.0, ink.gamma_multiply(a)) {
            p.circle(c, BADGE_R, fill.gamma_multiply(a), egui::Stroke::new(2.0, pal.bg));
            p.text(c, egui::Align2::CENTER_CENTER, b.emoji, egui::FontId::proportional(font::BODY), ink.gamma_multiply(a));
        }
    }
}

/// Indeterminate send bar (3 high, accent): a segment that scrolls as long as the transfer lasts.
fn progress(ui: &mut egui::Ui, pal: &Palette, width: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 3.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 1.5, pal.accent.gamma_multiply(0.2));
    let t = (ui.input(|i| i.time) * 0.8).fract() as f32;
    let seg = rect.width() * 0.35;
    let x0 = rect.left() + (rect.width() + seg) * t - seg;
    let bar = egui::Rect::from_x_y_ranges(x0.max(rect.left())..=(x0 + seg).min(rect.right()), rect.y_range());
    p.rect_filled(bar, 1.5, pal.accent);
}

/// Thread header, then rule: the name with the number on the right (or the number alone) and the Call button;
/// `true` if that button is clicked.
fn header(ui: &mut egui::Ui, pal: &Palette, name: &str, number: &str) -> bool {
    let (title, sub) = if name.is_empty() { (number, "") } else { (name, number) };
    let rect = w::header(ui, pal, Icon::Chat, title);
    let dial = w::header_button(ui, pal, rect, Icon::Phone).on_hover_text(t!("Call"));
    ui.painter().text(dial.rect.left_center() - egui::vec2(12.0, 0.0), Align2::RIGHT_CENTER, sub, w::regular(font::SMALL), pal.muted());
    w::rule(ui, pal);
    ui.add_space(12.0);
    dial.clicked()
}

/// Conversation preview: call or reaction as a translated label, otherwise the first line of the text.
pub fn preview(m: &Msg) -> Cow<'_, str> {
    if let Some((k, s)) = m.call {
        return Cow::Owned(call_label(k, s));
    }
    match tapback::parse(&m.text) {
        Some((e, q, cut)) => Cow::Owned(t!("{0} “{1}”", tapback::label(e), if cut { format!("{q}…") } else { q.to_owned() })),
        None => Cow::Borrowed(m.text.lines().next().unwrap_or("")),
    }
}

/// "Incoming call · 2 min 13", "Missed call".
fn call_label(kind: CallKind, secs: u64) -> String {
    let what = match kind {
        CallKind::In => t!("Received call"),
        CallKind::Out => t!("Placed call"),
        CallKind::Missed => return t!("Missed call").to_owned(),
    };
    match (secs / 60, secs % 60) {
        (0, s) => t!("{0} · {1} s", what, s),
        (m, s) => t!("{0} · {1} min {2}", what, m, format!("{s:02}")),
    }
}

/// `20261015T103000` → `15/10 10:30`; other formats returned as is.
pub fn short_stamp(t: &str) -> String {
    if t.len() >= 13 && t.is_ascii() && t.as_bytes()[8] == b'T' {
        // Day {0}, month {1}, hour {2}, minutes {3}: order depends on the language.
        t!("{1}/{0} {2}:{3}", &t[6..8], &t[4..6], &t[9..11], &t[11..13])
    } else {
        t.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_card_opens_its_link() {
        use egui::OutputCommand;
        use egui_kittest::kittest::Queryable;
        let c = Card { title: "Example title".into(), description: "Some text".into(), domain: "example.com".into(), image: None };
        // Fonts set at the first frame are bound from the next one only (`bold` family of the card).
        let mut fonts = false;
        let mut harness = egui_kittest::Harness::new_ui(|ui| {
            if !std::mem::replace(&mut fonts, true) {
                ui.ctx().set_fonts(bluez_message::gui::theme::fonts());
                ui.ctx().request_repaint();
                return;
            }
            card(ui, &Palette::default(), "https://example.com/a", &c);
        });
        harness.get_by_role_and_label(egui::accesskit::Role::Link, "Example title").click();
        harness.step();
        let opened = harness.output().platform_output.commands.iter().any(|c| matches!(c, OutputCommand::OpenUrl(u) if u.url == "https://example.com/a"));
        assert!(opened);
    }

    #[test]
    // Expected values built with `t!`: tests pass whatever the locale.
    fn stamps() {
        assert_eq!(short_stamp("20261015T103000"), t!("{1}/{0} {2}:{3}", "15", "10", "10", "30"));
        assert_eq!(short_stamp("hier"), "hier");
    }

    #[test]
    fn call_labels() {
        assert_eq!(call_label(CallKind::In, 133), t!("{0} · {1} min {2}", t!("Received call"), 2, "13"));
        assert_eq!(call_label(CallKind::Out, 9), t!("{0} · {1} s", t!("Placed call"), 9));
        assert_eq!(call_label(CallKind::Missed, 0), t!("Missed call"));
    }
}
