use std::{env, fs, io, path::PathBuf};

use service_auth::{ServiceTrustPolicyPayload, ServiceTrustRootSigningIdentity};

fn main() -> io::Result<()> {
    let path = env::args().nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: sign_service_trust POLICY_JSON",
        )
    })?;
    if env::args().nth(2).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: sign_service_trust POLICY_JSON",
        ));
    }
    let key_id = required_env("INFERLAB_SERVICE_TRUST_ROOT_KEY_ID")?;
    let seed = required_env("INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64")?;
    let identity = ServiceTrustRootSigningIdentity::from_base64_seed(key_id, &seed)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let bytes = fs::read(PathBuf::from(path))?;
    let policy = serde_json::from_slice::<ServiceTrustPolicyPayload>(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let snapshot = identity
        .sign(&policy)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    serde_json::to_writer_pretty(io::stdout(), &snapshot).map_err(io::Error::other)?;
    println!();
    Ok(())
}

fn required_env(name: &str) -> io::Result<String> {
    env::var(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is required")))
}
