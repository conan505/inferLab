use std::{env, fs, io, path::PathBuf};

use service_auth::{ServiceRequestPayload, ServiceSigningIdentity, canonical_json_body};

fn main() -> io::Result<()> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if arguments.len() != 7 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: sign_service_request METHOD PATH CLUSTER_ID AUDIENCE_ID ISSUED_AT_MS NONCE JSON_BODY|-",
        ));
    }
    let service_id = required_env("INFERLAB_SERVICE_ID")?;
    let seed = required_env("INFERLAB_SERVICE_PRIVATE_KEY_B64")?;
    let identity = ServiceSigningIdentity::from_base64_seed(service_id, &seed)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let issued_at_ms = arguments[4]
        .parse::<u64>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let body = if arguments[6] == "-" {
        Vec::new()
    } else {
        let raw = fs::read(PathBuf::from(&arguments[6]))?;
        let value: serde_json::Value = serde_json::from_slice(&raw)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        canonical_json_body(&value)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
    };
    let authentication = identity
        .authenticate(&ServiceRequestPayload {
            method: &arguments[0],
            path: &arguments[1],
            cluster_id: &arguments[2],
            audience_id: &arguments[3],
            issued_at_ms,
            nonce: &arguments[5],
            body: &body,
        })
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    serde_json::to_writer_pretty(io::stdout(), &authentication).map_err(io::Error::other)?;
    println!();
    Ok(())
}

fn required_env(name: &str) -> io::Result<String> {
    env::var(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is required")))
}
