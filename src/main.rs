use std::ffi::OsString;

use anyhow::{Context, Result, bail};

fn main() -> Result<()> {
    match parse_mode(std::env::args_os().skip(1))? {
        Mode::Interactive => stackhand::app::run_interactive(),
        Mode::FixtureRoundTrip(text) => stackhand::app::run_fixture_round_trip(&text),
        Mode::FixtureInput => stackhand::app::run_fixture_input(),
        Mode::FixturePaste => stackhand::app::run_fixture_paste(),
        Mode::FixtureRendering => stackhand::app::run_fixture_rendering(),
        Mode::FixtureScrollback => stackhand::scrollback_fixture::run(),
        Mode::FixtureMouse => stackhand::mouse_fixture::run(),
    }
}

enum Mode {
    Interactive,
    FixtureRoundTrip(String),
    FixtureInput,
    FixturePaste,
    FixtureRendering,
    FixtureScrollback,
    FixtureMouse,
}

fn parse_mode(mut args: impl Iterator<Item = OsString>) -> Result<Mode> {
    let Some(first) = args.next() else {
        return Ok(Mode::Interactive);
    };

    if first == "--fixture-rendering" {
        if args.next().is_some() {
            bail!("--fixture-rendering does not accept arguments");
        }
        return Ok(Mode::FixtureRendering);
    }

    if first == "--fixture-input" {
        if args.next().is_some() {
            bail!("--fixture-input does not accept arguments");
        }
        return Ok(Mode::FixtureInput);
    }

    if first == "--fixture-paste" {
        if args.next().is_some() {
            bail!("--fixture-paste does not accept arguments");
        }
        return Ok(Mode::FixturePaste);
    }

    if first == "--fixture-scrollback" {
        if args.next().is_some() {
            bail!("--fixture-scrollback does not accept arguments");
        }
        return Ok(Mode::FixtureScrollback);
    }

    if first == "--fixture-mouse" {
        if args.next().is_some() {
            bail!("--fixture-mouse does not accept arguments");
        }
        return Ok(Mode::FixtureMouse);
    }

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
