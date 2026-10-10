use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::go_os::path_error;
use crate::go_path;
use crate::go_strconv::quote;

const VARIABLE: &str = "PERSISTENT_VOLUME_MOUNTS";
const FIELD: &str = "PersistentVolumeMounts";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VolumeMounts {
    roots: HashMap<String, PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownVolumeType {
    pub volume_type: String,
}

impl fmt::Display for UnknownVolumeType {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "volume type not found: {}", quote(&self.volume_type))
    }
}

impl std::error::Error for UnknownVolumeType {}

impl VolumeMounts {
    pub fn from_vars(vars: &HashMap<String, String>) -> Result<Self, String> {
        vars.get(VARIABLE)
            .filter(|value| !value.is_empty())
            .map(|value| parse_pairs(value))
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .map(|(name, path)| mount_root(&name, &path).map(|root| (name, root)))
            .collect::<Result<HashMap<_, _>, _>>()
            .map(|roots| Self { roots })
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    pub fn volume_path(&self, volume_type: &str, team_id: Uuid, volume_id: Uuid) -> Result<PathBuf, UnknownVolumeType> {
        self.roots
            .get(volume_type)
            .map(|root| {
                go_path::join(&[
                    &root.to_string_lossy(),
                    &format!("team-{team_id}"),
                    &format!("vol-{volume_id}"),
                ])
                .into()
            })
            .ok_or_else(|| UnknownVolumeType {
                volume_type: volume_type.to_owned(),
            })
    }
}

fn parse_pairs(value: &str) -> Result<HashMap<String, String>, String> {
    value
        .split(',')
        .map(|pair| {
            pair.split_once(':')
                .map(|(name, path)| (name.to_owned(), path.to_owned()))
                .ok_or_else(|| {
                    format!(
                        "env: parse error on field {} of type \"map[string]string\": {} should be in key:value format",
                        quote(FIELD),
                        quote(pair)
                    )
                })
        })
        .collect()
}

fn mount_root(name: &str, path: &str) -> Result<PathBuf, String> {
    let cleaned = go_path::clean(path);
    let absolute = match cleaned.starts_with('/') {
        true => PathBuf::from(cleaned),
        false => std::env::current_dir()
            .map(|cwd| PathBuf::from(go_path::join(&[&cwd.to_string_lossy(), &cleaned])))
            .map_err(|e| {
                format!(
                    "failed to make persistent volume mount {} an absolute path: {e}",
                    quote(name)
                )
            })?,
    };
    std::fs::metadata(&absolute).map(|_| absolute.clone()).map_err(|e| {
        format!(
            "failed to access persistent volume mount {} ({}): {}",
            quote(name),
            quote(&absolute.to_string_lossy()),
            path_error("stat", Path::new(&absolute), &e)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(value: &str) -> HashMap<String, String> {
        HashMap::from([(VARIABLE.to_owned(), value.to_owned())])
    }

    #[test]
    fn mounts_default_to_none() {
        assert!(VolumeMounts::from_vars(&HashMap::new()).unwrap().is_empty());
        assert!(VolumeMounts::from_vars(&vars("")).unwrap().is_empty());
    }

    #[test]
    fn mounts_are_cleaned_and_must_exist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().display().to_string();

        let mounts = VolumeMounts::from_vars(&vars(&format!("fast:{path}//,slow:{path}/."))).unwrap();

        let volume = Uuid::new_v4();
        let team = Uuid::new_v4();
        assert_eq!(
            mounts.volume_path("fast", team, volume),
            Ok(PathBuf::from(format!("{path}/team-{team}/vol-{volume}")))
        );
        assert!(mounts.volume_path("slow", team, volume).is_ok());
    }

    #[test]
    fn a_missing_mount_names_the_mount_and_the_stat_error() {
        let error = VolumeMounts::from_vars(&vars("fast:/definitely/not/here")).unwrap_err();

        assert_eq!(
            error,
            "failed to access persistent volume mount \"fast\" (\"/definitely/not/here\"): stat /definitely/not/here: no such file or directory"
        );
    }

    #[test]
    fn a_pair_without_a_colon_is_a_parse_error() {
        let error = VolumeMounts::from_vars(&vars("fast")).unwrap_err();

        assert_eq!(
            error,
            "env: parse error on field \"PersistentVolumeMounts\" of type \"map[string]string\": \"fast\" should be in key:value format"
        );
    }

    #[test]
    fn an_unknown_type_reads_like_the_go_error() {
        let error = VolumeMounts::default()
            .volume_path("nope", Uuid::nil(), Uuid::nil())
            .unwrap_err();

        assert_eq!(error.to_string(), "volume type not found: \"nope\"");
    }
}
