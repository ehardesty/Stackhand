//! Deserialized YAML forms accepted by the configuration resolver.
//!
//! This module defines the single canonical version 1 YAML shape. The resolver
//! lowers it into the validated Project model.

use std::collections::{BTreeMap, HashSet};

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_yaml::Value;

use super::readiness::ReadinessFile;

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConfigFile {
    pub(super) version: u64,
    pub(super) base_profile_name: Option<String>,
    pub(super) env_files: Option<Vec<String>>,
    pub(super) profiles: Option<BTreeMap<String, ProjectProfileFile>>,
    #[serde(default)]
    pub(super) groups: GroupCollection,
    #[serde(default)]
    pub(super) processes: ProcessCollection,
    pub(super) settings: Option<SettingsFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProjectProfileFile {
    pub(super) env_files: Option<Vec<String>>,
}

/// Deserialize one name-keyed YAML mapping into `(name, value)` entries in
/// YAML order, rejecting duplicate names. This is one schema-level concept:
/// a mapping whose YAML order and duplicate-name rejection matter. Each
/// collection owns its own error wording.
fn ordered_mapping<'de, D, T>(
    deserializer: D,
    expecting: &'static str,
    sequence_hint: &'static str,
    duplicate_name: impl Fn(&str) -> String,
) -> Result<Vec<(String, T)>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct OrderedMappingVisitor<'a, T, F> {
        expecting: &'static str,
        sequence_hint: &'static str,
        duplicate_name: &'a F,
        marker: std::marker::PhantomData<T>,
    }

    impl<'de, T, F> Visitor<'de> for OrderedMappingVisitor<'_, T, F>
    where
        T: Deserialize<'de>,
        F: Fn(&str) -> String,
    {
        type Value = Vec<(String, T)>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str(self.expecting)
        }

        fn visit_seq<A>(self, _sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            Err(de::Error::custom(self.sequence_hint))
        }

        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut entries = Vec::new();
            let mut names = HashSet::new();
            while let Some(name) = map.next_key::<String>()? {
                if !names.insert(name.clone()) {
                    return Err(de::Error::custom((self.duplicate_name)(&name)));
                }
                let value = map.next_value::<T>()?;
                entries.push((name, value));
            }
            Ok(entries)
        }
    }

    deserializer.deserialize_any(OrderedMappingVisitor {
        expecting,
        sequence_hint,
        duplicate_name: &duplicate_name,
        marker: std::marker::PhantomData,
    })
}

#[derive(Default)]
pub(super) struct GroupCollection {
    pub(super) entries: Vec<(String, Vec<String>)>,
}

impl<'de> Deserialize<'de> for GroupCollection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ordered_mapping(
            deserializer,
            "a name-keyed mapping of Process Groups",
            "groups must be a name-keyed mapping; use 'groups: {Group name: [process-name]}'",
            |name| format!("duplicate Process Group name '{name}'"),
        )
        .map(|entries| Self { entries })
    }
}

#[derive(Default)]
pub(super) struct ProcessCollection {
    pub(super) entries: Vec<(String, ProcessFile)>,
}

impl<'de> Deserialize<'de> for ProcessCollection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ordered_mapping(
            deserializer,
            "a name-keyed mapping of Processes",
            "processes must be a name-keyed mapping; use 'processes: {name: {...}}'",
            |name| format!("duplicate Process name '{name}'"),
        )
        .map(|entries| Self { entries })
    }
}

pub(super) struct DependencyCollection {
    pub(super) entries: Vec<(String, Option<serde_yaml::Value>)>,
}

impl<'de> Deserialize<'de> for DependencyCollection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        ordered_mapping(
            deserializer,
            "a name-keyed mapping of Dependencies",
            "depends_on must be a name-keyed mapping; use 'depends_on: {process-name: condition}'",
            |name| format!("duplicate Dependency name '{name}'"),
        )
        .map(|entries| Self { entries })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettingsFile {
    pub(super) shell: Option<ShellFile>,
    pub(super) port_discovery: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ShellFile {
    pub(super) program: Option<String>,
    pub(super) args: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessFile {
    /// A fixed Process Profile name, or the reserved `base` label.
    pub(super) profile: Option<String>,
    /// Named partial Process patches. Their field allow-list is validated
    /// before the patches are merged and lowered.
    pub(super) profiles: Option<BTreeMap<String, Value>>,
    pub(super) kind: Option<String>,
    pub(super) enabled: Option<bool>,
    pub(super) autostart: Option<bool>,
    pub(super) cwd: Option<String>,
    pub(super) env_files: Option<Vec<String>>,
    pub(super) environment: Option<std::collections::BTreeMap<String, Option<String>>>,
    pub(super) terminal: Option<TerminalFile>,
    pub(super) success_exit_codes: Option<Vec<i32>>,
    pub(super) depends_on: Option<DependencyCollection>,
    pub(super) ready: Option<ReadinessFile>,
    pub(super) liveness: Option<ReadinessFile>,
    pub(super) restart: Option<RestartFile>,
    pub(super) command: Option<CommandFile>,
    pub(super) shell: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RestartFile {
    pub(super) policy: Option<String>,
    pub(super) backoff: Option<String>,
    pub(super) max_restarts: Option<u32>,
    pub(super) on_unhealthy: Option<bool>,
}

pub(super) enum CommandFile {
    Direct(Vec<serde_yaml::Value>),
}

impl<'de> Deserialize<'de> for CommandFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        match value {
            serde_yaml::Value::Sequence(values) => Ok(Self::Direct(values)),
            _ => Err(de::Error::custom(
                "command must be a sequence of the program and arguments; use 'command: [program, arg1, ...]' (use a sibling 'shell:' field for shell expressions)",
            )),
        }
    }
}

pub(super) enum TerminalFile {
    Settings(TerminalSettings),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct TerminalSettings {
    pub(super) mode: Option<String>,
    pub(super) input: Option<String>,
}

impl<'de> Deserialize<'de> for TerminalFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        match value {
            serde_yaml::Value::Mapping(map) => {
                let settings = serde_yaml::from_value(serde_yaml::Value::Mapping(map))
                    .map_err(de::Error::custom)?;
                Ok(Self::Settings(settings))
            }
            _ => Err(de::Error::custom(
                "terminal must be a mapping with 'mode' and optional 'input'; use 'terminal: {mode: pipe|pty, input: disabled|focused}'",
            )),
        }
    }
}
