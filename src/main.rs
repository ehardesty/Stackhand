use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};

fn main() -> Result<()> {
    match parse_mode(std::env::args_os().skip(1))? {
        Mode::Project { path, profile } => match path {
            Some(path) => stackhand::run_project_with_profile(&path, profile.as_deref()),
            None => stackhand::run_discovered_project_with_profile(profile.as_deref()),
        },
        Mode::ConfigValidate { path, profile } => {
            let sources = stackhand::validate_project_sources(path.as_deref(), profile.as_deref())
                .map_err(|error| anyhow!("configuration error: {error}"))?;
            println!("Project configuration is valid:");
            print_sources(&sources);
            Ok(())
        }
        Mode::ConfigShow { path, profile } => {
            let view = stackhand::show_project(path.as_deref(), profile.as_deref())
                .map_err(|error| anyhow!("configuration error: {error}"))?;
            println!("Project sources (precedence order):");
            print_sources(&view.sources);
            println!("Effective Project:");
            print!("{}", view.yaml);
            Ok(())
        }
        Mode::FixtureProject { path, profile } => match path {
            Some(path) => stackhand::project_fixture::run_with_profile(&path, profile.as_deref()),
            None => stackhand::project_fixture::run_discovered_with_profile(profile.as_deref()),
        },
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
    Project {
        path: Option<PathBuf>,
        profile: Option<String>,
    },
    ConfigValidate {
        path: Option<PathBuf>,
        profile: Option<String>,
    },
    ConfigShow {
        path: Option<PathBuf>,
        profile: Option<String>,
    },
    FixtureProject {
        path: Option<PathBuf>,
        profile: Option<String>,
    },
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
        return Ok(Mode::Project {
            path: None,
            profile: None,
        });
    };

    if first == "config" {
        let command = args.next().context("config requires a subcommand")?;
        if command == "validate" {
            let (path, profile) = parse_path_and_profile(args.collect(), "config validate")?;
            return Ok(Mode::ConfigValidate { path, profile });
        }
        if command == "show" {
            let (path, profile) = parse_path_and_profile(args.collect(), "config show")?;
            return Ok(Mode::ConfigShow { path, profile });
        }
        bail!("unknown config command: {}", command.to_string_lossy());
    }

    if first == "--fixture-project" {
        let (path, profile) = parse_path_and_profile(args.collect(), "--fixture-project")?;
        return Ok(Mode::FixtureProject { path, profile });
    }

    if first == "--fixture-rendering" {
        return zero_argument_mode(&mut args, "--fixture-rendering", Mode::FixtureRendering);
    }

    if first == "--fixture-input" {
        return zero_argument_mode(&mut args, "--fixture-input", Mode::FixtureInput);
    }

    if first == "--fixture-paste" {
        return zero_argument_mode(&mut args, "--fixture-paste", Mode::FixturePaste);
    }

    if first == "--fixture-scrollback" {
        return zero_argument_mode(&mut args, "--fixture-scrollback", Mode::FixtureScrollback);
    }

    if first == "--fixture-mouse" {
        return zero_argument_mode(&mut args, "--fixture-mouse", Mode::FixtureMouse);
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

    let mut project_arguments = vec![first];
    project_arguments.extend(args);
    let (path, profile) = parse_path_and_profile(project_arguments, "Project")?;
    Ok(Mode::Project { path, profile })
}

fn zero_argument_mode(
    args: &mut impl Iterator<Item = OsString>,
    flag: &str,
    mode: Mode,
) -> Result<Mode> {
    if args.next().is_some() {
        bail!("{flag} does not accept arguments");
    }
    Ok(mode)
}

fn parse_path_and_profile(
    arguments: Vec<OsString>,
    context: &str,
) -> Result<(Option<PathBuf>, Option<String>)> {
    let mut path = None;
    let mut profile = None;
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        if argument == "--profile" {
            let value = arguments
                .next()
                .context("--profile requires a profile name")?;
            set_profile(&mut profile, profile_name(value)?)?;
            continue;
        }
        if let Some(value) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix("--profile="))
        {
            set_profile(&mut profile, profile_name(OsString::from(value))?)?;
            continue;
        }
        if argument.to_string_lossy().starts_with('-') {
            bail!("unknown argument: {}", argument.to_string_lossy());
        }
        if path.is_some() {
            bail!("{context} accepts at most one Project path");
        }
        path = Some(PathBuf::from(argument));
    }
    Ok((path, profile))
}

fn set_profile(selected: &mut Option<String>, profile: String) -> Result<()> {
    if selected.replace(profile).is_some() {
        bail!("--profile can be specified only once");
    }
    Ok(())
}

fn print_sources(sources: &stackhand::ResolutionSources) {
    println!("  base: {}", sources.base.display());
    if let Some(local) = &sources.local {
        println!("  local override: {}", local.display());
    }
    if let Some(profile) = &sources.profile {
        println!("  profile: {profile}");
    }
}

fn profile_name(value: OsString) -> Result<String> {
    let value = value
        .into_string()
        .map_err(|_| anyhow!("profile name must be valid UTF-8"))?;
    if value.is_empty() {
        bail!("--profile requires a profile name");
    }
    Ok(value)
}
