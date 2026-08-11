use crate::model::AssetPreferences;

#[cfg(target_os = "linux")]
use std::ffi::{OsStr, OsString};

pub fn asset_preferences() -> AssetPreferences {
    #[cfg(target_os = "linux")]
    {
        detect_from(|name| std::env::var_os(name))
    }
    #[cfg(not(target_os = "linux"))]
    {
        AssetPreferences::default()
    }
}

#[cfg(target_os = "linux")]
fn detect_from(environment: impl Fn(&str) -> Option<OsString>) -> AssetPreferences {
    for variable in ["XDG_CURRENT_DESKTOP", "DESKTOP_SESSION"] {
        let Some(value) = environment(variable) else {
            continue;
        };
        match desktop_family(&value) {
            DesktopFamily::Gtk => return AssetPreferences(vec!["gtk".into()]),
            DesktopFamily::Qt => return AssetPreferences(vec!["qt".into()]),
            DesktopFamily::Conflicting => return AssetPreferences::default(),
            DesktopFamily::Unknown => {}
        }
    }
    AssetPreferences::default()
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DesktopFamily {
    Gtk,
    Qt,
    Conflicting,
    Unknown,
}

#[cfg(target_os = "linux")]
fn desktop_family(value: &OsStr) -> DesktopFamily {
    let value = value.to_string_lossy().to_ascii_lowercase();
    let mut gtk = false;
    let mut qt = false;
    for token in value
        .split(|character: char| {
            character == ':'
                || character == ';'
                || character == ','
                || character == '/'
                || character == '\\'
                || character.is_ascii_whitespace()
        })
        .map(|token| token.trim_end_matches(".desktop"))
        .filter(|token| !token.is_empty())
    {
        gtk |= matches!(
            token,
            "gtk"
                | "gnome"
                | "ubuntu"
                | "unity"
                | "cinnamon"
                | "x-cinnamon"
                | "mate"
                | "xfce"
                | "xfce4"
                | "lxde"
                | "budgie"
                | "pantheon"
        );
        qt |= matches!(
            token,
            "qt" | "kde" | "plasma" | "plasmawayland" | "plasmax11" | "lxqt"
        );
    }
    match (gtk, qt) {
        (true, false) => DesktopFamily::Gtk,
        (false, true) => DesktopFamily::Qt,
        (true, true) => DesktopFamily::Conflicting,
        (false, false) => DesktopFamily::Unknown,
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn detect(values: &[(&str, &str)]) -> AssetPreferences {
        let values = values
            .iter()
            .map(|(key, value)| ((*key).to_owned(), OsString::from(value)))
            .collect::<HashMap<_, _>>();
        detect_from(|key| values.get(key).cloned())
    }

    #[test]
    fn detects_common_gtk_desktops() {
        for desktop in [
            "GNOME",
            "ubuntu:GNOME",
            "Unity",
            "X-Cinnamon",
            "MATE",
            "XFCE",
            "LXDE",
            "Budgie:GNOME",
            "Pantheon",
        ] {
            assert_eq!(
                detect(&[("XDG_CURRENT_DESKTOP", desktop)]),
                AssetPreferences(vec!["gtk".into()]),
                "{desktop}"
            );
        }
    }

    #[test]
    fn detects_common_qt_desktops() {
        for desktop in ["KDE", "KDE:Plasma", "LXQt", "plasmawayland", "plasmax11"] {
            assert_eq!(
                detect(&[("XDG_CURRENT_DESKTOP", desktop)]),
                AssetPreferences(vec!["qt".into()]),
                "{desktop}"
            );
        }
    }

    #[test]
    fn falls_back_to_desktop_session_only_when_current_desktop_is_unknown() {
        assert_eq!(
            detect(&[
                ("XDG_CURRENT_DESKTOP", "mystery"),
                ("DESKTOP_SESSION", "/usr/share/xsessions/plasma.desktop"),
            ]),
            AssetPreferences(vec!["qt".into()])
        );
    }

    #[test]
    fn unknown_and_conflicting_desktops_have_no_preference() {
        assert_eq!(
            detect(&[("XDG_CURRENT_DESKTOP", "mystery")]),
            AssetPreferences::default()
        );
        assert_eq!(
            detect(&[
                ("XDG_CURRENT_DESKTOP", "GNOME:KDE"),
                ("DESKTOP_SESSION", "plasma"),
            ]),
            AssetPreferences::default()
        );
    }
}
