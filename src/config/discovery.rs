use std::{
    env,
    path::{Path, PathBuf},
};

use config as cfg;
use serde::Deserialize;
use url::Url;

use crate::error::{Error, Result};

use super::{ApiKey, Credentials};

const ENV_URL: &str = "ECMWF_DATASTORES_URL";
const ENV_KEY: &str = "ECMWF_DATASTORES_KEY";
const ECMWF_RC_FILE: &str = ".ecmwfdatastoresrc";
const CDS_RC_FILE: &str = ".cdsapirc";

#[derive(Deserialize)]
struct RawCredentials {
    url: String,

    key: Option<String>,
}

impl Credentials {
    /// Discovers one complete credential source without combining sources.
    ///
    /// The order is environment, `~/.ecmwfdatastoresrc`, then `~/.cdsapirc`.
    /// If either supported environment variable is set, the environment is
    /// treated as the selected source and must contain a URL.
    pub fn discover() -> Result<Self> {
        if env::var_os(ENV_URL).is_some() || env::var_os(ENV_KEY).is_some() {
            return Self::from_env();
        }

        let ecmwf = home_config_path(ECMWF_RC_FILE)?;
        if ecmwf.is_file() {
            return Self::from_file(ecmwf);
        }

        Self::from_cdsapirc()
    }

    /// Loads credentials from `ECMWF_DATASTORES_URL` and `ECMWF_DATASTORES_KEY`.
    pub fn from_env() -> Result<Self> {
        let url = env::var(ENV_URL)
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::MissingEnvironmentVariable(ENV_URL.into()))?;
        let api_key = env::var(ENV_KEY)
            .ok()
            .filter(|value| !value.is_empty())
            .map(ApiKey::parse)
            .transpose()?;
        Ok(Self::new(Url::parse(&url)?, api_key))
    }
    /// Loads credentials from a YAML file containing `url` and optional `key` fields.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = expand_path(path.as_ref());
        let raw = cfg::Config::builder()
            .add_source(cfg::File::from(path.clone()).format(cfg::FileFormat::Yaml))
            .build()
            .map_err(|err| map_config_error(err, path.clone()))?
            .try_deserialize::<RawCredentials>()
            .map_err(|err| Error::ConfigParse {
                path,
                source: std::io::Error::new(std::io::ErrorKind::InvalidData, err),
            })?;
        raw.try_into()
    }
    /// Loads credentials from `~/.cdsapirc`.
    pub fn from_cdsapirc() -> Result<Self> {
        Self::from_file(home_config_path(CDS_RC_FILE)?)
    }
    /// Loads credentials from `~/.ecmwfdatastoresrc`.
    pub fn from_ecmwf_datastores_rc() -> Result<Self> {
        Self::from_file(home_config_path(ECMWF_RC_FILE)?)
    }
}

impl TryFrom<RawCredentials> for Credentials {
    type Error = Error;

    fn try_from(raw: RawCredentials) -> Result<Self> {
        let endpoint = Url::parse(&raw.url)?;
        let api_key = raw.key.map(ApiKey::try_from).transpose()?;
        Ok(Self::new(endpoint, api_key))
    }
}

fn expand_path(raw: &Path) -> PathBuf {
    raw.to_str().map_or_else(
        || raw.to_path_buf(),
        |path| PathBuf::from(shellexpand::tilde(path).into_owned()),
    )
}

fn home_config_path(file_name: &str) -> Result<PathBuf> {
    dirs::home_dir()
        .map(|path| path.join(file_name))
        .ok_or(Error::HomeDirectoryNotFound)
}

fn map_config_error(err: cfg::ConfigError, path: PathBuf) -> Error {
    match err {
        cfg::ConfigError::NotFound(_) => Error::ConfigNotFound { path },
        other => Error::ConfigParse {
            path,
            source: std::io::Error::new(std::io::ErrorKind::InvalidData, other),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::NamedTempFile;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_config_paths() {
        use std::os::unix::ffi::OsStrExt as _;

        let path = Path::new(std::ffi::OsStr::from_bytes(b"config-\xff.yaml"));
        assert_eq!(expand_path(path), path);
    }

    #[test]
    fn parses_config_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(&mut file, "url: https://example.test/api").unwrap();
        writeln!(&mut file, "key: SECRET").unwrap();

        let credentials = Credentials::from_file(file.path()).unwrap();

        assert_eq!(credentials.endpoint().as_str(), "https://example.test/api");
        assert!(credentials.api_key().is_some());
    }

    #[test]
    fn reports_invalid_file() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(&mut file, "invalid line").unwrap();
        let err = Credentials::from_file(file.path()).unwrap_err();
        assert!(matches!(err, Error::ConfigParse { .. }));
    }
}
