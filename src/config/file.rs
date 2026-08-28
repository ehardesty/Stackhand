//! Deserialized YAML forms accepted by the configuration resolver.
//!
//! This module keeps the temporary and canonical spellings at the file-format
//! seam. The resolver lowers both forms into the same validated Project model.

use std::collections::HashSet;

use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::readiness::ReadinessFile;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ConfigFile {
    pub(super) version: u64,
    #[serde(default)]
    pub(super) processes: ProcessCollection,
    pub(super) settings: Option<SettingsFile>,
}

#[derive(Default)]
pub(super) struct ProcessCollection {
    pub(super) entries: Vec<ProcessEntry>,
}

pub(super) struct ProcessEntry {
    pub(super) key: Option<String>,
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
                formatter.write_str("a sequence or name-keyed mapping of Processes")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut entries = Vec::new();
                while let Some(process) = sequence.next_element::<ProcessFile>()? {
                    entries.push(ProcessEntry { key: None, process });
                }
                Ok(ProcessCollection { entries })
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
                        key: Some(name),
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
    pub(super) key: Option<String>,
    pub(super) value: serde_yaml::Value,
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
                formatter.write_str("a sequence or name-keyed mapping of Dependencies")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut entries = Vec::new();
                while let Some(value) = sequence.next_element::<serde_yaml::Value>()? {
                    entries.push(DependencyEntry { key: None, value });
                }
                Ok(DependencyCollection { entries })
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
                        key: Some(name),
                        value: map.next_value::<serde_yaml::Value>()?,
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
    pub(super) program: String,
    pub(super) args: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessFile {
    pub(super) name: Option<String>,
    pub(super) kind: Option<String>,
    pub(super) enabled: Option<bool>,
    pub(super) autostart: Option<bool>,
    pub(super) working_dir: Option<String>,
    pub(super) cwd: Option<String>,
    pub(super) env: Option<std::collections::BTreeMap<String, String>>,
    pub(super) environment: Option<std::collections::BTreeMap<String, String>>,
    pub(super) terminal: Option<TerminalFile>,
    pub(super) input: Option<String>,
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
    Legacy(CommandObject),
    Direct(Vec<serde_yaml::Value>),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CommandObject {
    pub(super) program: Option<String>,
    pub(super) args: Option<Vec<serde_yaml::Value>>,
    pub(super) shell: Option<String>,
}

impl<'de> Deserialize<'de> for CommandFile {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_yaml::Value::deserialize(deserializer)?;
        match value {
            serde_yaml::Value::Sequence(values) => Ok(Self::Direct(values)),
            serde_yaml::Value::Mapping(map) => {
                let object = serde_yaml::from_value(serde_yaml::Value::Mapping(map))
                    .map_err(de::Error::custom)?;
                Ok(Self::Legacy(object))
            }
            other => Err(de::Error::custom(format!(
                "command must be a sequence or mapping, got {other:?}"
            ))),
        }
    }
}

pub(super) enum TerminalFile {
    Legacy(String),
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
            serde_yaml::Value::String(value) => Ok(Self::Legacy(value)),
            serde_yaml::Value::Mapping(map) => {
                let settings = serde_yaml::from_value(serde_yaml::Value::Mapping(map))
                    .map_err(de::Error::custom)?;
                Ok(Self::Settings(settings))
            }
            other => Err(de::Error::custom(format!(
                "terminal must be a string or mapping, got {other:?}"
            ))),
        }
    }
}
