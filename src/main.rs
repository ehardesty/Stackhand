use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};

fn main() -> Result<()> {
    match parse_mode(std::env::args_os().skip(1))? {
        Mode::Project(Some(path)) => stackhand::run_project(&path),
        Mode::Project(None) => stackhand::run_discovered_project(),
        Mode::ConfigValidate(path) => {
            let base = stackhand::validate_project(path.as_deref())
                .map_err(|error| anyhow!("configuration error: {error}"))?;
            println!("Project configuration is valid: {}", base.display());
            Ok(())
        }
        Mode::FixtureProject(path) => stackhand::project_fixture::run(&path),
        Mode::FixtureRoundTrip(text) => stackhand::prototype::run_fixture_round_trip(&text),
        Mode::FixtureInput => stackhand::prototype::run_fixture_input(),
        Mode::FixturePaste => stackhand::prototype::run_fixture_paste(),
        Mode::FixtureRendering => stackhand::prototype::run_fixture_rendering(),
        Mode::FixtureScrollback => stackhand::prototype::run_fixture_scrollback(),
        Mode::FixtureMouse => stackhand::prototype::run_fixture_mouse(),
        Mode::FixtureInteraction(path) => stackhand::interaction_fixture::run(&path),
        Mode::FixtureSmoke(path) => stackhand::smoke_fixture::run(&path),
    }
}

enum Mode {
    Project(Option<PathBuf>),
    ConfigValidate(Option<PathBuf>),
    FixtureProject(PathBuf),
    FixtureRoundTrip(String),
    FixtureInput,
    FixturePaste,
    FixtureRendering,
    FixtureScrollback,
    FixtureMouse,
    FixtureInteraction(PathBuf),
    FixtureSmoke(PathBuf),
}

fn parse_mode(mut args: impl Iterator<Item = OsString>) -> Result<Mode> {
    let Some(first) = args.next() else {
        return Ok(Mode::Project(None));
    };

    if first == "config" {
        let command = args.next().context("config requires a subcommand")?;
        if command != "validate" {
            bail!("unknown config command: {}", command.to_string_lossy());
        }
        let path = args.next().map(PathBuf::from);
        if args.next().is_some() {
            bail!("config validate accepts at most one Project path");
        }
        return Ok(Mode::ConfigValidate(path));
    }

    if first == "--fixture-project" {
        let path = args
            .next()
            .context("--fixture-project requires a YAML path")?;
        return Ok(Mode::FixtureProject(PathBuf::from(path)));
    }

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

    if first == "--fixture-smoke" {
        let path = args
            .next()
            .context("--fixture-smoke requires a YAML path")?;
        return Ok(Mode::FixtureSmoke(PathBuf::from(path)));
    }

    if first == "--fixture-interaction" {
        let path = args
            .next()
            .context("--fixture-interaction requires a YAML path")?;
        return Ok(Mode::FixtureInteraction(PathBuf::from(path)));
    }

    if first == "--fixture-round-trip" {
        let text = args
            .next()
            .context("--fixture-round-trip requires one text argument")?
            .into_string()
            .map_err(|_| anyhow::anyhow!("fixture text must be valid UTF-8"))?;
        if args.next().is_some() {
            bail!("--fixture-round-trip accepts only one text argument");
        }
        return Ok(Mode::FixtureRoundTrip(text));
    }

    if first.to_string_lossy().starts_with('-') {
        bail!("unknown argument: {}", first.to_string_lossy());
    }
    if args.next().is_some() {
        bail!("only one Project file is accepted");
    }

    Ok(Mode::Project(Some(PathBuf::from(first))))
}
