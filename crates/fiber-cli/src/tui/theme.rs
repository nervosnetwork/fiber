//! Theme and color palette for the TUI.
//!
//! Provides semantic color roles that adapt to dark and light terminal backgrounds.
//! The palette is selected via auto-detection (reading `COLORFGBG`) or an explicit
//! `--theme` CLI flag.

use ratatui::style::Color;

/// Which color scheme to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    /// Auto-detect from terminal environment.
    Auto,
    /// Dark terminal background.
    Dark,
    /// Light terminal background.
    Light,
}

/// A full semantic color palette consumed by the UI renderer.
///
/// Every color used in the TUI is drawn from this palette so that switching
/// between dark and light themes only requires swapping the palette instance.
#[derive(Debug, Clone, Copy)]
pub struct ThemePalette {
    // ── Text hierarchy ──────────────────────────────────────
    /// Primary text (node names, addresses, form values).
    pub text_primary: Color,
    /// Secondary text (timestamps, help text, inactive tabs).
    pub text_secondary: Color,
    /// Muted text (disabled items, tree branches, empty placeholders).
    pub text_muted: Color,

    // ── Labels & headers ────────────────────────────────────
    /// Labels in the header, dashboard, forms, table headers.
    pub label: Color,
    /// Active/selected tab or sub-tab labels.
    pub label_active: Color,

    // ── Status colors ───────────────────────────────────────
    /// Success / connected / ready / open.
    pub success: Color,
    /// Error / disconnected / closed / cancelled / failed.
    pub error: Color,
    /// Warning / inflight / shutting-down / pending (accent).
    pub warning: Color,
    /// Info / counts / identifiers / paid / cursor marker.
    pub info: Color,
    /// Pending count highlights / received amounts.
    pub accent: Color,

    // ── Channel states (more specific) ──────────────────────
    pub channel_negotiating: Color,
    pub channel_collaborating: Color,
    pub channel_signing: Color,
    pub channel_await_ready: Color,
    pub channel_await_tx_sig: Color,

    // ── Backgrounds ─────────────────────────────────────────
    /// Table row highlight background.
    pub highlight_bg: Color,
    /// Badge foreground (text on colored background badges).
    pub badge_fg: Color,
    /// Popup / overlay background.
    pub popup_bg: Color,
    /// Popup / overlay foreground (block border).
    pub popup_fg: Color,

    // ── Full-screen background ───────────────────────────────
    /// Forced background for the entire TUI area.
    /// Dark theme uses black, light theme uses white, so colors
    /// render consistently regardless of the terminal's own background.
    pub bg: Color,
    /// Default border/box-drawing color.  Softer than `text_primary` so
    /// that frames recede behind the content.
    pub border: Color,

    // ── Misc ────────────────────────────────────────────────
    /// Separator lines in forms.
    pub separator: Color,
    /// Gauge unfilled portion.
    pub gauge_unfilled: Color,
}

impl ThemePalette {
    /// Palette for dark terminal backgrounds.
    pub fn dark() -> Self {
        Self {
            text_primary: Color::White,
            text_secondary: Color::Gray,
            text_muted: Color::DarkGray,

            label: Color::Yellow,
            label_active: Color::Yellow,

            success: Color::Green,
            error: Color::Red,
            warning: Color::Yellow,
            info: Color::Cyan,
            accent: Color::Magenta,

            channel_negotiating: Color::LightBlue,
            channel_collaborating: Color::LightCyan,
            channel_signing: Color::Magenta,
            channel_await_ready: Color::Cyan,
            channel_await_tx_sig: Color::LightMagenta,

            highlight_bg: Color::DarkGray,
            badge_fg: Color::Black,
            popup_bg: Color::Black,
            popup_fg: Color::White,

            bg: Color::Black,
            border: Color::DarkGray,

            separator: Color::Indexed(236),
            gauge_unfilled: Color::Red,
        }
    }

    /// Palette for light terminal backgrounds.
    pub fn light() -> Self {
        Self {
            text_primary: Color::Black,
            text_secondary: Color::Indexed(236), // #303030 — dark enough on white
            text_muted: Color::Indexed(240),     // #585858

            label: Color::Blue,
            label_active: Color::Blue,

            success: Color::Indexed(22), // dark green
            error: Color::Red,
            warning: Color::Indexed(172), // dark orange/yellow
            info: Color::Indexed(25),     // dark cyan/teal
            accent: Color::Indexed(128),  // dark magenta

            channel_negotiating: Color::Blue,
            channel_collaborating: Color::Indexed(25),
            channel_signing: Color::Indexed(128),
            channel_await_ready: Color::Indexed(25),
            channel_await_tx_sig: Color::Indexed(128),

            highlight_bg: Color::Indexed(254), // very light gray
            badge_fg: Color::Indexed(255),     // bright white — for text on dark-colored badges
            popup_bg: Color::Indexed(255),     // near-white popup background
            popup_fg: Color::Black,

            bg: Color::White,
            border: Color::Indexed(249), // medium-light gray

            separator: Color::Indexed(249),      // medium-light gray
            gauge_unfilled: Color::Indexed(203), // light red
        }
    }
}

/// Detect whether the terminal has a light background.
///
/// Uses the `COLORFGBG` environment variable (set by many terminals including
/// xterm, rxvt, iTerm2, and others). The format is `fg;bg` where `bg` is the
/// background color index. A high index (>= 8, or specifically 15 for white)
/// typically indicates a light background.
///
/// Falls back to dark if the variable is not set or cannot be parsed.
pub fn detect_theme() -> Theme {
    if let Ok(val) = std::env::var("COLORFGBG") {
        if let Some(bg_str) = val.rsplit(';').next() {
            if let Ok(bg) = bg_str.trim().parse::<u8>() {
                // Standard terminal color indices:
                // 0-7 are dark colors, 8-15 are bright colors.
                // A light background typically has bg >= 9 (bright colors).
                // Common: 15 = white, 7 = silver/light gray.
                if bg >= 7 {
                    return Theme::Light;
                }
                return Theme::Dark;
            }
        }
    }
    // Default to dark — the most common terminal configuration.
    Theme::Dark
}

/// Resolve a `Theme` choice into a concrete `ThemePalette`.
pub fn resolve_palette(theme: Theme) -> ThemePalette {
    match theme {
        Theme::Dark => ThemePalette::dark(),
        Theme::Light => ThemePalette::light(),
        Theme::Auto => {
            let detected = detect_theme();
            match detected {
                Theme::Light => ThemePalette::light(),
                _ => ThemePalette::dark(),
            }
        }
    }
}
