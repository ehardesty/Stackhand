use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use libghostty_vt::color::Rgb;

use super::SessionStop;
use crate::terminal::colors::{ColorSupport, HostColorProfile};

pub(crate) const QUERY_BUDGET: Duration = Duration::from_millis(750);

#[derive(Clone, Copy)]
pub(crate) enum ColorQueries {
    DefaultsOnly,
    FullPalette,
}

impl ColorQueries {
    pub(crate) fn for_environment() -> Self {
        Self::for_host(
            std::env::var_os("TMUX").is_some(),
            std::env::var("TERM_PROGRAM").ok().as_deref(),
            std::env::var("TERM_PROGRAM_VERSION").ok().as_deref(),
        )
    }

    fn for_host(in_tmux: bool, program: Option<&str>, version: Option<&str>) -> Self {
        // tmux before 3.7 consumes incomplete palette replies (tmux/tmux#4749).
        if !in_tmux || program == Some("tmux") && version.is_some_and(safe_tmux_version) {
            Self::FullPalette
        } else {
            Self::DefaultsOnly
        }
    }

    fn complete(self, profile: &HostColorProfile) -> bool {
        match self {
            Self::DefaultsOnly => profile.foreground.is_some() && profile.background.is_some(),
            Self::FullPalette => profile.support() == ColorSupport::Supported,
        }
    }
}

fn safe_tmux_version(version: &str) -> bool {
    let Some((major, rest)) = version.split_once('.') else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (minor, suffix) = rest.split_at(digits);
    if !suffix.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return false;
    }
    match (major.parse::<u16>(), minor.parse::<u16>()) {
        (Ok(major), Ok(minor)) => (major, minor) >= (3, 7),
        _ => false,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ColorReply {
    Foreground(Rgb),
    Background(Rgb),
    Indexed(u8, Rgb),
}

// Ghostty's OSC parser classifies color operations but does not expose their data.
pub(super) fn parse_reply(bytes: &[u8]) -> Vec<ColorReply> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let mut fields = text.split(';');
    match fields.next() {
        Some("4") => {
            let mut replies = Vec::new();
            while let (Some(index), Some(value)) = (fields.next(), fields.next()) {
                if let (Ok(index), Ok(color)) = (index.parse::<u8>(), Rgb::parse(value)) {
                    replies.push(ColorReply::Indexed(index, color));
                }
            }
            replies
        }
        Some(command @ ("10" | "11")) => {
            let Some(value) = fields.next() else {
                return Vec::new();
            };
            if fields.next().is_some() {
                return Vec::new();
            }
            Rgb::parse(value)
                .ok()
                .map(|color| match command {
                    "10" => ColorReply::Foreground(color),
                    _ => ColorReply::Background(color),
                })
                .into_iter()
                .collect()
        }
        _ => Vec::new(),
    }
}

#[derive(Clone)]
pub(crate) struct HostColorQuery(Arc<Shared>);

struct Shared {
    state: Mutex<QueryState>,
    ready: Condvar,
    deadline: Instant,
    queries: ColorQueries,
}

enum QueryState {
    Collecting(Box<HostColorProfile>),
    Frozen(Arc<HostColorProfile>),
}

impl HostColorQuery {
    pub(crate) fn new(deadline: Instant, queries: ColorQueries) -> Self {
        Self(Arc::new(Shared {
            state: Mutex::new(QueryState::Collecting(Box::default())),
            ready: Condvar::new(),
            deadline,
            queries,
        }))
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.0.deadline
    }

    pub(crate) fn request(&self) -> Vec<u8> {
        use std::io::Write;
        let mut bytes = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\".to_vec();
        if matches!(self.0.queries, ColorQueries::FullPalette) {
            for index in 0..256 {
                write!(bytes, "\x1b]4;{index};?\x1b\\").expect("Vec writes cannot fail");
            }
        }
        bytes
    }

    pub(super) fn record(&self, reply: ColorReply, now: Instant) {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        if now >= self.0.deadline {
            return;
        }
        if let QueryState::Collecting(profile) = &mut *state {
            match reply {
                ColorReply::Foreground(color) => profile.foreground = Some(color),
                ColorReply::Background(color) => profile.background = Some(color),
                ColorReply::Indexed(index, color) => {
                    profile.indexed[usize::from(index)] = Some(color)
                }
            }
            if self.0.queries.complete(profile) {
                *state = QueryState::Frozen(Arc::new((**profile).clone()));
                self.0.ready.notify_all();
            }
        }
    }

    pub(crate) fn finish(&self, stop: &SessionStop) -> Arc<HostColorProfile> {
        let mut state = self.0.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            match &*state {
                QueryState::Frozen(profile) => return Arc::clone(profile),
                QueryState::Collecting(profile)
                    if Instant::now() >= self.0.deadline || stop.requested() =>
                {
                    let profile = Arc::new((**profile).clone());
                    *state = QueryState::Frozen(Arc::clone(&profile));
                    return profile;
                }
                _ => {
                    state = self
                        .0
                        .ready
                        .wait_timeout(state, Duration::from_millis(10))
                        .unwrap_or_else(|p| p.into_inner())
                        .0;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_and_absent_profiles_freeze_without_accepting_late_colors() {
        let now = Instant::now();
        let query = HostColorQuery::new(now + Duration::from_secs(1), ColorQueries::FullPalette);
        query.record(ColorReply::Background(Rgb { r: 1, g: 2, b: 3 }), now);
        let stop = SessionStop::default();
        stop.stop();
        let profile = query.finish(&stop);
        assert_eq!(profile.support(), ColorSupport::Partial);
        assert_eq!(profile.background, Some(Rgb { r: 1, g: 2, b: 3 }));
        assert_eq!(profile.foreground, None);
        query.record(ColorReply::Background(Rgb { r: 9, g: 8, b: 7 }), now);
        assert_eq!(query.finish(&stop), profile);
        let fallback =
            HostColorQuery::new(now, ColorQueries::FullPalette).finish(&SessionStop::default());
        assert_eq!(fallback.support(), ColorSupport::Fallback);
    }

    #[test]
    fn all_palette_answers_complete_one_profile_without_per_index_waits() {
        let now = Instant::now();
        let query = HostColorQuery::new(now + Duration::from_secs(10), ColorQueries::FullPalette);
        query.record(ColorReply::Foreground(Rgb { r: 1, g: 2, b: 3 }), now);
        query.record(ColorReply::Background(Rgb { r: 4, g: 5, b: 6 }), now);
        for index in 0..=255 {
            query.record(
                ColorReply::Indexed(
                    index,
                    Rgb {
                        r: index,
                        g: 17,
                        b: 29,
                    },
                ),
                now,
            );
        }
        let profile = query.finish(&SessionStop::default());
        assert_eq!(profile.support(), ColorSupport::Supported);
        assert_eq!(
            profile.indexed[255],
            Some(Rgb {
                r: 255,
                g: 17,
                b: 29
            })
        );
        query.record(ColorReply::Foreground(Rgb::default()), now);
        assert_eq!(query.finish(&SessionStop::default()), profile);
    }

    #[test]
    fn affected_or_unknown_tmux_never_requests_palette_and_finishes_on_default_replies() {
        let now = Instant::now();
        for version in [
            Some("3.6"),
            Some("3.6a"),
            Some("3.6b"),
            Some("next-3.7"),
            Some("openbsd-7.7"),
            Some("garbage"),
            None,
        ] {
            let query = HostColorQuery::new(
                now + Duration::from_secs(60),
                ColorQueries::for_host(true, Some("tmux"), version),
            );
            assert_eq!(query.request(), b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
            query.record(ColorReply::Foreground(Rgb { r: 1, g: 2, b: 3 }), now);
            query.record(ColorReply::Background(Rgb { r: 4, g: 5, b: 6 }), now);
            assert!(matches!(
                *query.0.state.lock().unwrap(),
                QueryState::Frozen(_)
            ));
            let profile = query.finish(&SessionStop::default());
            assert_eq!(profile.support(), ColorSupport::Partial);
            assert!(profile.indexed.iter().all(Option::is_none));
        }
        for program in [None, Some("ghostty")] {
            let query =
                HostColorQuery::new(now, ColorQueries::for_host(true, program, Some("3.8")));
            assert!(!query.request().windows(4).any(|bytes| bytes == b"\x1b]4;"));
        }
        for (in_tmux, version) in [
            (true, "3.7"),
            (true, "3.7c"),
            (true, "3.8"),
            (false, "garbage"),
        ] {
            let query = HostColorQuery::new(
                now,
                ColorQueries::for_host(in_tmux, Some("tmux"), Some(version)),
            );
            assert!(
                query
                    .request()
                    .windows(10)
                    .any(|bytes| bytes == b"\x1b]4;255;?\x1b")
            );
        }
    }
}
