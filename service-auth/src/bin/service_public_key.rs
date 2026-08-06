use std::{env, io};

use service_auth::{LEGACY_CREDENTIAL_ID, ServiceSigningIdentity};

fn main() -> io::Result<()> {
    let service_id = required_env("INFERLAB_SERVICE_ID")?;
    let credential_id = env::var("INFERLAB_SERVICE_CREDENTIAL_ID")
        .unwrap_or_else(|_| LEGACY_CREDENTIAL_ID.to_owned());
    let seed = required_env("INFERLAB_SERVICE_PRIVATE_KEY_B64")?;
    let identity =
        ServiceSigningIdentity::from_base64_seed_with_credential(service_id, credential_id, &seed)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    println!("{}", identity.public_key_base64());
    Ok(())
}

fn required_env(name: &str) -> io::Result<String> {
    env::var(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is required")))
}
