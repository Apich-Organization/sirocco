//! Palette, type scale and widget styles.
//!
//! One dark theme, hand-tuned. The rules it follows:
//!
//! * **Three text levels, each with a measured contrast.** `TEXT` on `PANEL` is
//!   about 15:1, `MUTED` 8:1, `FAINT` 5:1 — all above AA for their sizes. A
//!   hierarchy you cannot read is not a hierarchy.
//! * **Elevation by surface, not by shadow.** `RAIL` sits below `BG`, cards sit
//!   above it. A drop shadow on a background this dark is invisible anyway, so
//!   the border does the separating and it is visible (about 2:1).
//! * **Colour only where it means something.** One accent, three semantics
//!   (running / done / failed). Nothing is coloured for decoration.

use iced::Border;
use iced::Color;
use iced::Theme;
use iced::widget::button;
use iced::widget::checkbox;
use iced::widget::container;
use iced::widget::pick_list;
use iced::widget::scrollable;
use iced::widget::text_input;

// ---------------------------------------------------------------- surfaces

pub const BG: Color = Color::from_rgb(0.051, 0.055, 0.067);
/// The left rail, a step *below* the page so the content column reads as the
/// foreground.
pub const RAIL: Color = Color::from_rgb(0.043, 0.047, 0.058);
pub const PANEL: Color = Color::from_rgb(0.086, 0.094, 0.110);
pub const PANEL_HI: Color = Color::from_rgb(0.125, 0.137, 0.161);

pub const BORDER: Color = Color::from_rgb(0.270, 0.290, 0.330);
pub const BORDER_HI: Color = Color::from_rgb(0.380, 0.410, 0.470);

// ---------------------------------------------------------------- text

pub const TEXT: Color = Color::from_rgb(0.906, 0.918, 0.941);
pub const MUTED: Color = Color::from_rgb(0.686, 0.714, 0.769);
pub const FAINT: Color = Color::from_rgb(0.522, 0.553, 0.616);

// ---------------------------------------------------------------- semantic

pub const ACCENT: Color = Color::from_rgb(0.345, 0.612, 1.0);
pub const ACCENT_SOFT: Color = Color::from_rgb(0.235, 0.392, 0.639);
pub const OK: Color = Color::from_rgb(0.294, 0.827, 0.549);
pub const WARN: Color = Color::from_rgb(0.980, 0.722, 0.302);
pub const ERR: Color = Color::from_rgb(0.957, 0.380, 0.435);

// ---------------------------------------------------------------- scale

/// Type scale. Five steps, no more: at 10/12/15/20/24 the difference between
/// adjacent sizes is visible, which is the whole point of a scale.
pub const T_BRAND: f32 = 20.0;
pub const T_TITLE: f32 = 15.0;
pub const T_VALUE: f32 = 24.0;
pub const T_BODY: f32 = 12.0;
pub const T_EYEBROW: f32 = 10.0;

pub const R_CARD: f32 = 12.0;
pub const R_CTRL: f32 = 8.0;
pub const R_CHIP: f32 = 6.0;

pub const PAD_CARD: [u16; 2] = [14, 16];

pub fn theme() -> Theme {
    Theme::custom(
        "Sirocco".to_string(),
        iced::theme::Palette {
            background: BG,
            text: TEXT,
            primary: ACCENT,
            success: OK,
            warning: WARN,
            danger: ERR,
        },
    )
}

// ---------------------------------------------------------------- containers

pub fn root(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(BG.into()),
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

/// A card: one step above the page, separated by a visible hairline.
pub fn card(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(PANEL.into()),
        text_color: Some(TEXT),
        border: Border {
            color: BORDER,
            width: 1.0,
            radius: R_CARD.into(),
        },
        ..container::Style::default()
    }
}

/// The left rail.
pub fn rail(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(RAIL.into()),
        text_color: Some(TEXT),
        ..container::Style::default()
    }
}

/// A horizontal rule.
pub fn rule(_t: &Theme) -> container::Style {
    container::Style {
        background: Some(BORDER.into()),
        ..container::Style::default()
    }
}

/// A tinted tile: status pills, row badges.
pub fn chip(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        container::Style {
            background: Some(Color { a: 0.13, ..color }.into()),
            text_color: Some(color),
            border: Border {
                color: Color { a: 0.32, ..color },
                width: 1.0,
                radius: R_CHIP.into(),
            },
            ..container::Style::default()
        }
    }
}

/// A warning or error strip.
pub fn notice(color: Color) -> impl Fn(&Theme) -> container::Style {
    move |_| {
        container::Style {
            background: Some(Color { a: 0.10, ..color }.into()),
            border: Border {
                color: Color { a: 0.34, ..color },
                width: 1.0,
                radius: R_CTRL.into(),
            },
            text_color: Some(color),
            ..container::Style::default()
        }
    }
}

// ---------------------------------------------------------------- buttons

pub fn primary(
    _t: &Theme,
    status: button::Status,
) -> button::Style {
    let (bg, fg) = match status {
        | button::Status::Hovered => (Color::from_rgb(0.42, 0.68, 1.0), BG),
        | button::Status::Pressed => (ACCENT_SOFT, TEXT),
        | button::Status::Disabled => (PANEL_HI, FAINT),
        | button::Status::Active => (ACCENT, BG),
    };
    button::Style {
        background: Some(bg.into()),
        text_color: fg,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: R_CTRL.into(),
        },
        ..button::Style::default()
    }
}

pub fn danger(
    _t: &Theme,
    status: button::Status,
) -> button::Style {
    let bg = match status {
        | button::Status::Hovered => Color::from_rgb(1.0, 0.47, 0.50),
        | button::Status::Pressed => Color::from_rgb(0.72, 0.24, 0.30),
        | button::Status::Disabled => PANEL_HI,
        | button::Status::Active => ERR,
    };
    button::Style {
        background: Some(bg.into()),
        text_color: BG,
        border: Border {
            color: Color::TRANSPARENT,
            width: 0.0,
            radius: R_CTRL.into(),
        },
        ..button::Style::default()
    }
}

pub fn ghost(
    _t: &Theme,
    status: button::Status,
) -> button::Style {
    let (bg, fg, border) = match status {
        | button::Status::Hovered => (PANEL_HI, TEXT, BORDER_HI),
        | button::Status::Pressed => (BORDER, TEXT, BORDER_HI),
        | button::Status::Disabled => (Color::TRANSPARENT, FAINT, BORDER),
        | button::Status::Active => (Color::TRANSPARENT, MUTED, BORDER),
    };
    button::Style {
        background: Some(bg.into()),
        text_color: fg,
        border: Border {
            color: border,
            width: 1.0,
            radius: R_CTRL.into(),
        },
        ..button::Style::default()
    }
}

/// A segment of a segmented control.
pub fn tab(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |_t, status| {
        let (bg, fg, border) = if selected {
            (Color { a: 0.18, ..ACCENT }, ACCENT, ACCENT_SOFT)
        } else {
            match status {
                | button::Status::Hovered | button::Status::Pressed => (PANEL_HI, TEXT, BORDER_HI),
                | button::Status::Active | button::Status::Disabled => {
                    (Color::TRANSPARENT, MUTED, BORDER)
                },
            }
        };
        button::Style {
            background: Some(bg.into()),
            text_color: fg,
            border: Border {
                color: border,
                width: 1.0,
                radius: R_CTRL.into(),
            },
            ..button::Style::default()
        }
    }
}

// ---------------------------------------------------------------- inputs

pub fn input(
    _t: &Theme,
    status: text_input::Status,
) -> text_input::Style {
    let border = match status {
        | text_input::Status::Focused { .. } => ACCENT,
        | text_input::Status::Hovered => BORDER_HI,
        | _ => BORDER,
    };
    text_input::Style {
        background: PANEL_HI.into(),
        border: Border {
            color: border,
            width: 1.0,
            radius: R_CTRL.into(),
        },
        icon: MUTED,
        placeholder: FAINT,
        value: TEXT,
        selection: Color { a: 0.35, ..ACCENT },
    }
}

pub fn checkbox_style(
    _t: &Theme,
    status: checkbox::Status,
) -> checkbox::Style {
    let border = match status {
        | checkbox::Status::Hovered { .. } => BORDER_HI,
        | _ => BORDER,
    };
    checkbox::Style {
        background: PANEL_HI.into(),
        border: Border {
            color: border,
            width: 1.0,
            radius: 4.0.into(),
        },
        icon_color: ACCENT,
        text_color: Some(TEXT),
    }
}

/// A slider that does not look like the default.
pub fn slider(
    _t: &Theme,
    _s: iced::widget::slider::Status,
) -> iced::widget::slider::Style {
    iced::widget::slider::Style {
        rail: iced::widget::slider::Rail {
            backgrounds: (ACCENT.into(), PANEL_HI.into()),
            width: 3.0,
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 2.0.into(),
            },
        },
        handle: iced::widget::slider::Handle {
            shape: iced::widget::slider::HandleShape::Circle { radius: 7.0 },
            background: TEXT.into(),
            border_color: Color::TRANSPARENT,
            border_width: 0.0,
        },
    }
}

pub fn pick(
    _t: &Theme,
    status: pick_list::Status,
) -> pick_list::Style {
    let border = match status {
        | pick_list::Status::Opened { .. } => ACCENT,
        | pick_list::Status::Hovered => BORDER_HI,
        | pick_list::Status::Active => BORDER,
    };
    pick_list::Style {
        text_color: TEXT,
        placeholder_color: FAINT,
        handle_color: MUTED,
        background: PANEL_HI.into(),
        border: Border {
            color: border,
            width: 1.0,
            radius: R_CTRL.into(),
        },
    }
}

/// A scrollbar you have to look for. The default one is a light bar that reads as
/// a divider and draws attention to the fact that a panel scrolls, which is
/// information nobody needs.
pub fn scroll(
    _t: &Theme,
    _s: scrollable::Status,
) -> scrollable::Style {
    let rail = scrollable::Rail {
        background: None,
        border: Border::default(),
        scroller: scrollable::Scroller {
            background: Color { a: 0.22, ..MUTED }.into(),
            border: Border {
                color: Color::TRANSPARENT,
                width: 0.0,
                radius: 3.0.into(),
            },
        },
    };
    scrollable::Style {
        container: container::Style::default(),
        vertical_rail: rail,
        horizontal_rail: rail,
        gap: None,
        // The overlay that appears while auto-scrolling; this app never enables
        // it, so it only has to exist and not draw attention.
        auto_scroll: scrollable::AutoScroll {
            background: Color::TRANSPARENT.into(),
            border: Border::default(),
            shadow: iced::Shadow::default(),
            icon: MUTED,
        },
    }
}
