use std::ffi::OsString;

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    match parse_mode(std::env::args_os().skip(1))? {
        Mode::Interactive => stackhand::app::run_interactive(),
        Mode::FixtureRoundTrip(text) => stackhand::app::run_fixture_round_trip(&text),
    }
}

enum Mode {
    Interactive,
    FixtureRoundTrip(String),
}

fn parse_mode(mut args: impl Iterator<Item = OsString>) -> Result<Mode> {
    let Some(first) = args.next() else {
        return Ok(Mode::Interactive);
    };

    if first != "--fixture-round-trip" {
        bail!("unknown argument: {}", first.to_string_lossy());
    }

    let text = args
        .next()
        .context("--fixture-round-trip requires one text argument")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("fixture text must be valid UTF-8"))?;

    if args.next().is_some() {
        bail!("--fixture-round-trip accepts only one text argument");
    }

    Ok(Mode::FixtureRoundTrip(text))
}
