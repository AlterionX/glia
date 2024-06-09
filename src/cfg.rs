use config::{Config, ConfigError};
use serde::{Deserialize, Serialize};

use crate::ReportableError;

fn err_mapper(e: ConfigError) -> ReportableError {
    match e {
        config::ConfigError::Frozen => {
            ReportableError { message: "frozen config".to_owned() }
        },
        config::ConfigError::Type { origin: _, unexpected: _, expected: _, key: _ } => {
            ReportableError { message: "config bad".to_owned() }
        },
        config::ConfigError::Message(m) => {
            ReportableError { message: format!("message {m:?}") }
        },
        config::ConfigError::Foreign(_f) => {
            ReportableError { message: "foreign".to_owned() }
        },
        config::ConfigError::NotFound(_n) => {
            ReportableError { message: "not found".to_owned() }
        },
        config::ConfigError::PathParse(p) => {
            ReportableError { message: format!("bad path {p:?}") }
        },
        config::ConfigError::FileParse { uri, cause } => {
            ReportableError { message: format!("file parsing {uri:?}, {cause:?}") }
        },
    }
}

#[derive(Serialize, Deserialize)]
pub struct LoggingConfiguration {
    pub path: String,
}

#[derive(Serialize, Deserialize)]
pub struct Configuration {
    pub log: LoggingConfiguration,
}

const DEFAULT_FILE: &str = "config/cfg.toml";
const ENV_PREFIX: &str = "GLIA";

pub fn read(arg_file: &str) -> Result<Box<Configuration>, ReportableError> {
    let cfg = Config::builder()
        .add_source(config::File::new(arg_file, config::FileFormat::Toml))
        .add_source(config::Environment::with_prefix(ENV_PREFIX))
        .build()
        .map_err(err_mapper)?;
    Ok(Box::new(cfg.try_deserialize().map_err(err_mapper)?))
}
