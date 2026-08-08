use std::{env, error::Error, fmt};

use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::Service;

const LOG_FORMAT_ENV: &str = "INFERLAB_LOG_FORMAT";

/// Supported process-wide log encodings.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LogFormat {
    #[default]
    Compact,
    Json,
}

impl LogFormat {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Json => "json",
        }
    }

    /// Parse the exact configured value; unset defaults to compact.
    pub fn from_value(value: Option<&str>) -> Result<Self, LogFormatError> {
        match value {
            None | Some("compact") => Ok(Self::Compact),
            Some("json") => Ok(Self::Json),
            Some(value) => Err(LogFormatError::Unsupported(value.to_owned())),
        }
    }

    pub fn from_env() -> Result<Self, LogFormatError> {
        match env::var(LOG_FORMAT_ENV) {
            Ok(value) => Self::from_value(Some(&value)),
            Err(env::VarError::NotPresent) => Ok(Self::Compact),
            Err(env::VarError::NotUnicode(_)) => Err(LogFormatError::NonUnicode),
        }
    }
}

/// Initialize the global tracing subscriber from `RUST_LOG` and the strict
/// `INFERLAB_LOG_FORMAT=compact|json` contract.
pub fn init_tracing(service: Service) -> Result<(), TracingInitError> {
    let format = LogFormat::from_env().map_err(TracingInitError::LogFormat)?;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let result = match format {
        LogFormat::Compact => tracing_subscriber::registry()
            .with(filter)
            .with(tracing_subscriber::fmt::layer().compact())
            .try_init(),
        LogFormat::Json => tracing_subscriber::registry()
            .with(filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .flatten_event(true)
                    .with_current_span(false)
                    .with_span_list(false),
            )
            .try_init(),
    };
    result.map_err(|error| TracingInitError::SetGlobal(error.to_string()))?;

    tracing::info!(
        service = service.as_str(),
        event = "tracing_initialized",
        log_format = format.as_str(),
    );
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LogFormatError {
    NonUnicode,
    Unsupported(String),
}

impl fmt::Display for LogFormatError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonUnicode => write!(formatter, "{LOG_FORMAT_ENV} is not valid Unicode"),
            Self::Unsupported(value) => write!(
                formatter,
                "unsupported {LOG_FORMAT_ENV} value `{value}`; expected `compact` or `json`"
            ),
        }
    }
}

impl Error for LogFormatError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TracingInitError {
    LogFormat(LogFormatError),
    SetGlobal(String),
}

impl fmt::Display for TracingInitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LogFormat(error) => error.fmt(formatter),
            Self::SetGlobal(error) => write!(formatter, "failed to initialize tracing: {error}"),
        }
    }
}

impl Error for TracingInitError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LogFormat(error) => Some(error),
            Self::SetGlobal(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_format_is_strict_and_defaults_to_compact() {
        assert_eq!(LogFormat::from_value(None), Ok(LogFormat::Compact));
        assert_eq!(
            LogFormat::from_value(Some("compact")),
            Ok(LogFormat::Compact)
        );
        assert_eq!(LogFormat::from_value(Some("json")), Ok(LogFormat::Json));
        for invalid in ["", "JSON", "pretty", " json"] {
            assert_eq!(
                LogFormat::from_value(Some(invalid)),
                Err(LogFormatError::Unsupported(invalid.to_owned()))
            );
        }
    }
}
