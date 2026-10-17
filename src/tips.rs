//! Short tips about gestures the browser doesn't show, such as holding an
//! entry to edit it. A tip shows on its screen until the user does what it
//! teaches or dismisses it; which are done is saved in `settings.json`
//! (see [`crate::config::tips_done`]) and Settings can turn tips off.
//!
//! To add a tip: a [`Tip`] variant, and its id, screen and text below. The
//! browser decides when a screen has something to teach (see
//! `Navigator::tip_screen`), then marks the tip done when it is learned.

use std::collections::BTreeSet;

/// Where a tip is shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// The server list, with a saved server on it.
    Servers,
    /// A folder on a server where files can be changed.
    Folder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tip {
    HoldServer,
    HoldEntry,
}

impl Tip {
    /// In the order they show, per screen.
    pub const ALL: [Tip; 2] = [Tip::HoldServer, Tip::HoldEntry];

    /// What is saved once the tip is done: never change an id.
    pub fn id(self) -> &'static str {
        match self {
            Tip::HoldServer => "hold_server",
            Tip::HoldEntry => "hold_entry",
        }
    }

    pub fn screen(self) -> Screen {
        match self {
            Tip::HoldServer => Screen::Servers,
            Tip::HoldEntry => Screen::Folder,
        }
    }

    pub fn text(self) -> &'static str {
        match self {
            Tip::HoldServer => "Hold a server to edit or delete it",
            Tip::HoldEntry => "Hold a file or folder to rename, move or delete it",
        }
    }
}

/// The tip to show on `screen`: the first not yet done, none when tips are off.
pub fn pick(screen: Screen, enabled: bool, done: &BTreeSet<String>) -> Option<Tip> {
    enabled
        .then(|| {
            Tip::ALL
                .into_iter()
                .find(|t| t.screen() == screen && !done.contains(t.id()))
        })
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn each_screen_gets_its_own_tip() {
        let none = done(&[]);
        assert_eq!(pick(Screen::Servers, true, &none), Some(Tip::HoldServer));
        assert_eq!(pick(Screen::Folder, true, &none), Some(Tip::HoldEntry));
    }

    #[test]
    fn learned_or_dismissed_tips_stay_away() {
        let servers = done(&["hold_server"]);
        assert_eq!(pick(Screen::Servers, true, &servers), None);
        assert_eq!(pick(Screen::Folder, true, &servers), Some(Tip::HoldEntry));
        assert_eq!(
            pick(Screen::Folder, true, &done(&["hold_entry", "x"])),
            None
        );
    }

    #[test]
    fn tips_off_shows_nothing() {
        assert_eq!(pick(Screen::Servers, false, &done(&[])), None);
        assert_eq!(pick(Screen::Folder, false, &done(&[])), None);
    }

    #[test]
    fn ids_are_unique_and_texts_plain() {
        let ids: BTreeSet<_> = Tip::ALL.iter().map(|t| t.id()).collect();
        assert_eq!(ids.len(), Tip::ALL.len());
        for t in Tip::ALL {
            assert!(!t.text().contains('\u{2014}') && !t.text().ends_with('.'));
        }
    }
}
