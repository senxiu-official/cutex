//! Cached display metadata. Reload on profile/settings refresh, never on each cell.
use cutex::profiles::appearance::{Appearance, Provider, TextStyle};
use ratatui::style::{Color, Modifier, Style};
use std::{
    collections::BTreeMap,
    sync::{LazyLock, RwLock},
};
#[derive(Default)]
struct Cache {
    appearance: Appearance,
    profiles: BTreeMap<String, (Provider, TextStyle)>,
}
static CACHE: LazyLock<RwLock<Cache>> = LazyLock::new(|| RwLock::new(Cache::default()));
pub(super) fn refresh() {
    let Ok(config) = cutex::config::store::load_codez_config_checked() else {
        return;
    };
    let Ok(accounts) = super::account_store::load_store_read_only() else {
        return;
    };
    let profiles = accounts
        .accounts
        .into_iter()
        .map(|a| {
            let p = cutex::profiles::appearance::provider(&a);
            let s = config.appearance.profile_style(&a.options, p);
            (a.name, (p, s))
        })
        .collect();
    if let Ok(mut c) = CACHE.write() {
        *c = Cache {
            appearance: config.appearance,
            profiles,
        };
    }
}
pub(super) fn render(s: &TextStyle) -> Style {
    let mut style = Style::new().fg(super::session_tui_layout::text());
    if let Some(color) =
        s.fg.as_deref()
            .and_then(|s| s.strip_prefix('#'))
            .filter(|s| s.len() == 6)
            .and_then(|s| u32::from_str_radix(s, 16).ok())
    {
        style = style.fg(Color::Rgb(
            (color >> 16) as u8,
            (color >> 8) as u8,
            color as u8,
        ));
    }
    for (enabled, modifier) in [
        (s.bold, Modifier::BOLD),
        (s.dim, Modifier::DIM),
        (s.italic, Modifier::ITALIC),
        (s.underlined, Modifier::UNDERLINED),
    ] {
        if enabled {
            style = style.add_modifier(modifier);
        } else {
            style = style.remove_modifier(modifier);
        }
    }
    style
}
pub(super) fn profile(name: &str) -> Style {
    CACHE
        .read()
        .ok()
        .and_then(|c| c.profiles.get(name).map(|(_, s)| render(s)))
        .unwrap_or_else(|| render(&TextStyle::default()))
}
pub(super) fn agent(name: Option<&str>, remote_provider: Option<Provider>, remote: bool) -> Style {
    let Ok(c) = CACHE.read() else {
        return Style::new();
    };
    let p = if remote {
        remote_provider.unwrap_or(Provider::Other)
    } else {
        name.and_then(|n| c.profiles.get(n).map(|v| v.0))
            .unwrap_or(Provider::Other)
    };
    render(&c.appearance.provider_style(p))
}
pub(super) fn remote_profile(p: Option<Provider>, s: Option<&TextStyle>) -> Style {
    s.map(render).unwrap_or_else(|| agent(None, p, true))
}
pub(super) fn host() -> Style {
    CACHE
        .read()
        .map(|c| render(&c.appearance.host_suffix))
        .unwrap_or_default()
}

pub(super) fn profile_options(
    options: &cutex::profiles::appearance::ProfileOptions,
    p: Provider,
) -> TextStyle {
    CACHE
        .read()
        .map(|c| c.appearance.profile_style(options, p))
        .unwrap_or_else(|_| Appearance::default().profile_style(options, p))
}
