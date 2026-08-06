use std::{env, fs, io, path::PathBuf, time::SystemTime};

use control_auth::{ControlWritePayload, RoutingWorker, WriterSigningIdentity};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
struct Worker {
    id: String,
    base_url: String,
    #[serde(default = "default_weight")]
    weight: u32,
}

#[derive(Deserialize, Serialize)]
struct Configuration {
    routing_policy: String,
    workers: Vec<Worker>,
}

#[derive(Serialize)]
struct AuthorizedWrite {
    expected_revision: u64,
    configuration: Configuration,
    authorization: control_auth::ControlWriteAuthorization,
}

struct Arguments {
    cluster_id: String,
    expected_revision: u64,
    issued_at_ms: u64,
    nonce: String,
    configuration: PathBuf,
}

fn main() -> io::Result<()> {
    let arguments = parse_arguments()?;
    let writer_id = required_env("INFERLAB_CONTROL_WRITER_ID")?;
    let private_seed = required_env("INFERLAB_CONTROL_WRITER_PRIVATE_KEY_B64")?;
    let signer = WriterSigningIdentity::from_base64_seed(writer_id, &private_seed)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let configuration: Configuration = serde_json::from_slice(&fs::read(&arguments.configuration)?)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let payload = ControlWritePayload {
        cluster_id: &arguments.cluster_id,
        expected_revision: arguments.expected_revision,
        issued_at_ms: arguments.issued_at_ms,
        nonce: &arguments.nonce,
        routing_policy: &configuration.routing_policy,
        workers: configuration
            .workers
            .iter()
            .map(|worker| RoutingWorker {
                id: &worker.id,
                base_url: &worker.base_url,
                weight: worker.weight,
            })
            .collect(),
    };
    let authorization = signer
        .sign(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    serde_json::to_writer_pretty(
        io::stdout(),
        &AuthorizedWrite {
            expected_revision: arguments.expected_revision,
            configuration,
            authorization,
        },
    )
    .map_err(io::Error::other)?;
    println!();
    Ok(())
}

fn parse_arguments() -> io::Result<Arguments> {
    let mut values = env::args().skip(1);
    let cluster_id = values.next().ok_or_else(usage)?;
    let expected_revision = values
        .next()
        .ok_or_else(usage)?
        .parse::<u64>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let issued_at = values.next().ok_or_else(usage)?;
    let issued_at_ms = if issued_at == "now" {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    } else {
        issued_at
            .parse::<u64>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?
    };
    let nonce = values.next().ok_or_else(usage)?;
    let configuration = values.next().map(PathBuf::from).ok_or_else(usage)?;
    if values.next().is_some() {
        return Err(usage());
    }
    Ok(Arguments {
        cluster_id,
        expected_revision,
        issued_at_ms,
        nonce,
        configuration,
    })
}

fn usage() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "usage: sign_control_write CLUSTER_ID EXPECTED_REVISION ISSUED_AT_MS|now NONCE CONFIGURATION_JSON",
    )
}

fn required_env(name: &str) -> io::Result<String> {
    env::var(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is required")))
}

fn default_weight() -> u32 {
    1
}
