use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use libghostty_vt::style::RgbColor;

use super::SessionStop;
use crate::terminal::colors::{ColorSupport, HostColorProfile};

pub(crate) const QUERY_BUDGET: Duration = Duration::from_millis(750);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ColorReply {
    Foreground(RgbColor),
    Background(RgbColor),
    Indexed(u8, RgbColor),
}

// Ghostty's OSC parser classifies color operations but does not expose their data.
// Framing stays in the host reader; Ghostty parses each color specification.
pub(super) fn parse_reply(bytes: &[u8]) -> Vec<ColorReply> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Vec::new();
    };
    let mut fields = text.split(';');
    match fields.next() {
        Some("4") => {
            let mut replies = Vec::new();
            while let (Some(index), Some(value)) = (fields.next(), fields.next()) {
                if let (Ok(index), Ok(color)) = (index.parse::<u8>(), RgbColor::parse(value)) {
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
            RgbColor::parse(value)
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
}

enum QueryState {
    Collecting(Box<HostColorProfile>),
    Frozen(Arc<HostColorProfile>),
}

impl HostColorQuery {
    pub(crate) fn new(deadline: Instant) -> Self {
        Self(Arc::new(Shared {
            state: Mutex::new(QueryState::Collecting(Box::default())),
            ready: Condvar::new(),
            deadline,
        }))
    }

    pub(crate) fn deadline(&self) -> Instant {
        self.0.deadline
    }

    pub(crate) fn request() -> Vec<u8> {
        use std::io::Write;
        let mut bytes = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\".to_vec();
        for index in 0..256 {
            write!(bytes, "\x1b]4;{index};?\x1b\\").expect("Vec writes cannot fail");
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
            if profile.support() == ColorSupport::Supported {
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
        let query = HostColorQuery::new(now + Duration::from_secs(1));
        query.record(ColorReply::Background(RgbColor { r: 1, g: 2, b: 3 }), now);
        let stop = SessionStop::default();
        stop.stop();
        let profile = query.finish(&stop);
        assert_eq!(profile.support(), ColorSupport::Partial);
        assert_eq!(profile.background, Some(RgbColor { r: 1, g: 2, b: 3 }));
        assert_eq!(profile.foreground, None);
        query.record(ColorReply::Background(RgbColor { r: 9, g: 8, b: 7 }), now);
        assert_eq!(query.finish(&stop), profile);
        let fallback = HostColorQuery::new(now).finish(&SessionStop::default());
        assert_eq!(fallback.support(), ColorSupport::Fallback);
    }

    #[test]
    fn all_palette_answers_complete_one_profile_without_per_index_waits() {
        let now = Instant::now();
        let query = HostColorQuery::new(now + Duration::from_secs(10));
        query.record(ColorReply::Foreground(RgbColor { r: 1, g: 2, b: 3 }), now);
        query.record(ColorReply::Background(RgbColor { r: 4, g: 5, b: 6 }), now);
        for index in 0..=255 {
            query.record(
                ColorReply::Indexed(
                    index,
                    RgbColor {
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
            Some(RgbColor {
                r: 255,
                g: 17,
                b: 29
            })
        );
        query.record(ColorReply::Foreground(RgbColor::default()), now);
        assert_eq!(query.finish(&SessionStop::default()), profile);
    }
}
