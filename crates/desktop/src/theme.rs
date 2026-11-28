//! Desktop appearance is shared by all windows. The daemon has no theme preference.
use gpui::{App, Global};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Theme {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub text: u32,
    pub muted: u32,
    pub secondary: u32,
    pub accent: u32,
    pub border: u32,
    pub tint: u32,
    pub background: u32,
    pub sidebar: u32,
    pub surface: u32,
    pub warning: u32,
    pub on_accent: u32,
    pub accent_hover: u32,
    pub warning_border: u32,
    pub warning_surface: u32,
    pub blue_surface: u32,
    pub blue_text: u32,
    pub scroll_thumb: u32,
    pub selection: u32,
}
impl Theme {
    pub fn palette(self) -> Palette {
        match self {
            Self::Light => Palette {
                text: 0x23382f,
                muted: 0x596b62,
                secondary: 0x65766e,
                accent: 0x285d48,
                border: 0xdce3dc,
                tint: 0xe6eee6,
                background: 0xfdfdfa,
                sidebar: 0xf3f5ef,
                surface: 0xffffff,
                warning: 0x876423,
                on_accent: 0xffffff,
                accent_hover: 0x36755b,
                warning_border: 0xe6d8b9,
                warning_surface: 0xfaf6ec,
                blue_surface: 0xe5edf3,
                blue_text: 0x536e7b,
                scroll_thumb: 0xa4b2a6,
                selection: 0x77a09355,
            },
            // Base colors from Tomorrow Night, with separate surfaces for the desktop UI.
            Self::Dark => Palette {
                text: 0xc5c8c6,
                muted: 0xb0b2b3,
                secondary: 0x969896,
                accent: 0x81a2be,
                border: 0x42454a,
                tint: 0x32353a,
                background: 0x1d1f21,
                sidebar: 0x191a1c,
                surface: 0x282a2e,
                warning: 0xf0c674,
                on_accent: 0x16181a,
                accent_hover: 0xa0bcd3,
                warning_border: 0x625442,
                warning_surface: 0x302d29,
                blue_surface: 0x303943,
                blue_text: 0xa9c1d5,
                scroll_thumb: 0x777b81,
                selection: 0x4b4e55ff,
            },
        }
    }
    fn other(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Dark,
        }
    }
    fn read(path: &Path) -> anyhow::Result<Self> {
        match fs::read(path) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }
    fn save(self, path: &Path) -> anyhow::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("Invalid appearance settings path"))?;
        fs::create_dir_all(dir)?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        serde_json::to_writer(tmp.as_file_mut(), &self)?;
        tmp.persist(path)?;
        Ok(())
    }
}

#[derive(Default)]
struct Appearance {
    theme: Theme,
    path: Option<PathBuf>,
}
impl Global for Appearance {}

pub(crate) fn initialize(cx: &mut App) {
    let path = crate::local_daemon::config_dir().map(|p| p.join("appearance.json"));
    let theme = path
        .as_deref()
        .map(Theme::read)
        .transpose()
        .unwrap_or_else(|e| {
            eprintln!("Could not load appearance settings: {e}");
            None
        })
        .unwrap_or_default();
    cx.set_global(Appearance { theme, path });
    apply_scrollbar_theme(cx);
}
pub(crate) fn current(cx: &App) -> Theme {
    cx.try_global::<Appearance>()
        .map(|a| a.theme)
        .unwrap_or_default()
}
pub(crate) fn palette(cx: &App) -> Palette {
    current(cx).palette()
}

/// The standalone scrollbar reads its colors from GPUI Component's app-wide theme.
pub(crate) fn ensure_scrollbar_theme(cx: &mut App) {
    if !cx.has_global::<gpui_component::Theme>() {
        apply_scrollbar_theme(cx);
    }
}

fn apply_scrollbar_theme(cx: &mut App) {
    use gpui_component::{Theme as ComponentTheme, ThemeMode, ThemeRegistry};
    if !cx.has_global::<ThemeRegistry>() {
        gpui_component::theme::init(cx);
    }
    let appearance = current(cx);
    let mode = match appearance {
        Theme::Dark => ThemeMode::Dark,
        Theme::Light => ThemeMode::Light,
    };
    ComponentTheme::change(mode, None, cx);
    let colors = appearance.palette();
    let theme = ComponentTheme::global_mut(cx);
    theme.scrollbar_show = gpui_component::scroll::ScrollbarShow::Always;
    theme.scrollbar = gpui::Hsla::transparent_black();
    theme.scrollbar_thumb = gpui::rgb(colors.scroll_thumb).into();
    theme.scrollbar_thumb_hover = gpui::rgb(colors.muted).into();
    theme.border = gpui::rgb(colors.border).into();
}

pub(crate) fn toggle_button(expanded: bool, cx: &App) -> gpui::Stateful<gpui::Div> {
    use crate::{
        icons::Icon,
        style::{hit, label},
    };
    use gpui::{prelude::*, px, rgb};
    let colors = palette(cx);
    let (icon, title) = match current(cx) {
        Theme::Dark => (Icon::Sun, "Light mode"),
        Theme::Light => (Icon::Moon, "Dark mode"),
    };
    hit("toggle-theme")
        .flex_none()
        .h(px(34.))
        .px_3()
        .gap_3()
        .hover(move |s| s.bg(rgb(colors.tint)))
        .child(icon.draw(16., colors.muted))
        .when(expanded, |button| {
            button.child(label(title, 13., colors.text))
        })
        .on_click(|_, _, cx| {
            let theme = current(cx).other();
            let settings = cx.default_global::<Appearance>();
            settings.theme = theme;
            if let Some(path) = &settings.path
                && let Err(error) = theme.save(path)
            {
                eprintln!("Could not save appearance settings: {error}");
            }
            apply_scrollbar_theme(cx);
            cx.refresh_windows();
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_is_dark_and_user_choice_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("appearance.json");
        assert_eq!(Theme::read(&path).unwrap(), Theme::Dark);
        for theme in [Theme::Light, Theme::Dark] {
            theme.save(&path).unwrap();
            assert_eq!(Theme::read(&path).unwrap(), theme);
        }
    }
}
