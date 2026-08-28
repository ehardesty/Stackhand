//! Deserialized YAML forms accepted by the configuration resolver.
//!
//! This module defines the single canonical version 1 YAML shape. The resolver
//! lowers it into the validated Project model.

use std::collections::{BTreeMap, HashSet};

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::readiness::ReadinessFile;

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConfigFile {
    pub(super) version: u64,
    #[serde(default)]
    pub(super) processes: ProcessCollection,
    // The typed parse keeps every profile subject to the top-level schema,
    // including profiles that were not selected.
    pub(super) profiles: Option<BTreeMap<String, ProfileFile>>,
    pub(super) settings: Option<SettingsFile>,
}

#[allow(dead_code)]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProfileFile {
    #[serde(default)]
    pub(super) enable: Vec<String>,
    #[serde(default)]
    pub(super) disable: Vec<String>,
    #[serde(default)]
    pub(super) overrides: serde_yaml::Value,
    pub(super) settings: Option<serde_yaml::Value>,
}

#[derive(Default)]
pub(super) struct ProcessCollection {
    pub(super) entries: Vec<ProcessEntry>,
}

pub(super) struct ProcessEntry {
    pub(super) key: String,
    pub(super) process: ProcessFile,
}

impl<'de> Deserialize<'de> for ProcessCollection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ProcessCollectionVisitor;

        impl<'de> Visitor<'de> for ProcessCollectionVisitor {
            type Value = ProcessCollection;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a name-keyed mapping of Processes")
            }

            fn visit_seq<A>(self, _sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                Err(de::Error::custom(
                    "processes must be a name-keyed mapping; use 'processes: {name: {...}}'",
                ))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::new();
                let mut names = HashSet::new();
                while let Some(name) = map.next_key::<String>()? {
                    if !names.insert(name.clone()) {
                        return Err(de::Error::custom(format!(
                            "duplicate Process name '{name}'"
                        )));
                    }
                    entries.push(ProcessEntry {
                        key: name,
                        process: map.next_value::<ProcessFile>()?,
                    });
                }
                Ok(ProcessCollection { entries })
            }
        }

        deserializer.deserialize_any(ProcessCollectionVisitor)
    }
}

pub(super) struct DependencyCollection {
    pub(super) entries: Vec<DependencyEntry>,
}

pub(super) struct DependencyEntry {
    pub(super) key: String,
    pub(super) value: Option<serde_yaml::Value>,
}

impl<'de> Deserialize<'de> for DependencyCollection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DependencyCollectionVisitor;

        impl<'de> Visitor<'de> for DependencyCollectionVisitor {
            type Value = DependencyCollection;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a name-keyed mapping of Dependencies")
            }

            fn visit_seq<A>(self, _sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                Err(de::Error::custom(
                    "depends_on must be a name-keyed mapping; use 'depends_on: {process-name: condition}'",
                ))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut entries = Vec::new();
                let mut names = HashSet::new();
                while let Some(name) = map.next_key::<String>()? {
                    if !names.insert(name.clone()) {
                        return Err(de::Error::custom(format!(
                            "duplicate Dependency name '{name}'"
                        )));
                    }
                    entries.push(DependencyEntry {
                        key: name,
                        value: map.next_value::<Option<serde_yaml::Value>>()?,
                    });
                }
                Ok(DependencyCollection { entries })
            }
        }

        deserializer.deserialize_any(DependencyCollectionVisitor)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SettingsFile {
    pub(super) shell: Option<ShellFile>,
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
    pub(super) kind: Option<String>,
    pub(super) enabled: Option<bool>,
    pub(super) autostart: Option<bool>,
    pub(super) cwd: Option<String>,
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
