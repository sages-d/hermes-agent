//! Hermes visual tokens for GPUI, matching `apps/shared/src/theme-presets.ts` (`nous`)
//! and `apps/desktop/DESIGN.md` ("flat, not boxed; tokens over literals").

use gpui::{Hsla, rgb};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeMode {
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy)]
pub struct HermesTheme {
    pub mode: ThemeMode,
    pub bg: Hsla,
    pub sidebar_bg: Hsla,
    pub surface: Hsla,
    pub surface_hover: Hsla,
    pub popover: Hsla,
    pub fg: Hsla,
    pub fg_muted: Hsla,
    pub border: Hsla,
    pub accent: Hsla,
    pub accent_fg: Hsla,
    pub accent_subtle: Hsla,
    pub user_bubble: Hsla,
    pub user_bubble_border: Hsla,
    pub code_bg: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub destructive: Hsla,
}

impl HermesTheme {
    /// Default Hermes `nous` dark palette (`apps/shared/src/theme-presets.ts`).
    pub fn nous_dark() -> Self {
        Self {
            mode: ThemeMode::Dark,
            bg: rgb(0x0d1117).into(),
            sidebar_bg: rgb(0x010409).into(),
            surface: rgb(0x161b22).into(),
            surface_hover: rgb(0x1f2631).into(),
            popover: rgb(0x161b22).into(),
            fg: rgb(0xe6edf3).into(),
            fg_muted: rgb(0x7d8590).into(),
            border: rgb(0x30363d).into(),
            accent: rgb(0x4a84fe).into(),
            accent_fg: rgb(0x0d1117).into(),
            accent_subtle: rgb(0x17243a).into(),
            user_bubble: rgb(0x1d2e4f).into(),
            user_bubble_border: rgb(0x2d4675).into(),
            code_bg: rgb(0x090d13).into(),
            success: rgb(0x3fb950).into(),
            warning: rgb(0xd29922).into(),
            destructive: rgb(0xf85149).into(),
        }
    }

    /// Default Hermes `nous` light palette (`apps/shared/src/theme-presets.ts`).
    pub fn nous_light() -> Self {
        Self {
            mode: ThemeMode::Light,
            bg: rgb(0xffffff).into(),
            sidebar_bg: rgb(0xf6f8fa).into(),
            surface: rgb(0xf6f8fa).into(),
            surface_hover: rgb(0xeaeef2).into(),
            popover: rgb(0xffffff).into(),
            fg: rgb(0x1f2328).into(),
            fg_muted: rgb(0x656d76).into(),
            border: rgb(0xd0d7de).into(),
            accent: rgb(0x0053fd).into(),
            accent_fg: rgb(0xffffff).into(),
            accent_subtle: rgb(0xe3edff).into(),
            user_bubble: rgb(0xdae7fd).into(),
            user_bubble_border: rgb(0xb8cef8).into(),
            code_bg: rgb(0xf3f5f7).into(),
            success: rgb(0x1a7f37).into(),
            warning: rgb(0x9a6700).into(),
            destructive: rgb(0xcf222e).into(),
        }
    }

    pub fn toggle(self) -> Self {
        match self.mode {
            ThemeMode::Dark => Self::nous_light(),
            ThemeMode::Light => Self::nous_dark(),
        }
    }

    /// Adopt a hex accent color from `runtime.describe`'s `SkinPayload`, if present.
    pub fn apply_skin(&mut self, skin: &hermes_protocol::SkinPayload) {
        let Some(colors) = skin.colors.as_ref() else {
            return;
        };
        for key in ["banner_accent", "accent", "primary", "midground", "prompt"] {
            if let Some(hex) = colors.get(key)
                && let Some(parsed) = parse_hex_rgb(hex)
            {
                self.accent = rgb(parsed).into();
                return;
            }
        }
    }
}

fn parse_hex_rgb(raw: &str) -> Option<u32> {
    let hex = raw.trim().strip_prefix('#')?;
    if hex.len() == 6 {
        u32::from_str_radix(hex, 16).ok()
    } else {
        None
    }
}
