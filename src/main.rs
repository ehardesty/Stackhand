use std::ffi::OsString;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};

fn main() -> Result<()> {
    match parse_mode(std::env::args_os().skip(1))? {
        Mode::Project { path, profiles } => match path {
            Some(path) => stackhand::run_project_with_profiles(&path, &profiles),
            None => stackhand::run_discovered_project_with_profiles(&profiles),
        },
        Mode::ConfigValidate { path, profiles } => {
            let sources =
                stackhand::validate_project_sources_with_profiles(path.as_deref(), &profiles)
                    .map_err(|error| anyhow!("configuration error: {error}"))?;
            println!("Project configuration is valid:");
            print_sources(&sources);
            Ok(())
        }
        Mode::ConfigShow { path, profiles } => {
            let view = stackhand::show_project_with_profiles(path.as_deref(), &profiles)
                .map_err(|error| anyhow!("configuration error: {error}"))?;
            println!("Project sources (precedence order):");
            print_sources(&view.sources);
            println!("Effective Project:");
            print!("{}", view.yaml);
            Ok(())
        }
        Mode::FixtureProject { path, profiles } => match path {
            Some(path) => stackhand::project_fixture::run_with_profiles(&path, &profiles),
            None => stackhand::project_fixture::run_discovered_with_profiles(&profiles),
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
        profiles: Vec<String>,
    },
    ConfigValidate {
        path: Option<PathBuf>,
        profiles: Vec<String>,
    },
    ConfigShow {
        path: Option<PathBuf>,
        profiles: Vec<String>,
    },
    FixtureProject {
        path: Option<PathBuf>,
        profiles: Vec<String>,
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
            profiles: Vec::new(),
        });
    };

    if first == "config" {
        let command = args.next().context("config requires a subcommand")?;
        if command == "validate" {
            let (path, profiles) = parse_path_and_profiles(args.collect(), "config validate")?;
            return Ok(Mode::ConfigValidate { path, profiles });
        }
        if command == "show" {
            let (path, profiles) = parse_path_and_profiles(args.collect(), "config show")?;
            return Ok(Mode::ConfigShow { path, profiles });
        }
        bail!("unknown config command: {}", command.to_string_lossy());
    }

    if first == "--fixture-project" {
        let (path, profiles) = parse_path_and_profiles(args.collect(), "--fixture-project")?;
        return Ok(Mode::FixtureProject { path, profiles });
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
    let (path, profiles) = parse_path_and_profiles(project_arguments, "Project")?;
    Ok(Mode::Project { path, profiles })
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

fn parse_path_and_profiles(
    arguments: Vec<OsString>,
    context: &str,
) -> Result<(Option<PathBuf>, Vec<String>)> {
    let mut path = None;
    let mut profiles = Vec::new();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        if argument == "--profile" {
            let value = arguments
                .next()
                .context("--profile requires a profile name")?;
            profiles.push(profile_name(value)?);
            continue;
        }
        if let Some(value) = argument
            .to_str()
            .and_then(|argument| argument.strip_prefix("--profile="))
        {
            profiles.push(profile_name(OsString::from(value))?);
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
    Ok((path, profiles))
}

fn print_sources(sources: &stackhand::ResolutionSources) {
    println!("  base: {}", sources.base.display());
    for profile in &sources.profiles {
        println!("  profile: {profile}");
    }
    if let Some(local) = &sources.local {
        println!("  local override: {}", local.display());
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
