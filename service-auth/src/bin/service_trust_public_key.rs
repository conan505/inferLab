use std::{env, io};

use service_auth::ServiceTrustRootSigningIdentity;

fn main() -> io::Result<()> {
    let key_id = required_env("INFERLAB_SERVICE_TRUST_ROOT_KEY_ID")?;
    let seed = required_env("INFERLAB_SERVICE_TRUST_ROOT_PRIVATE_KEY_B64")?;
    let identity = ServiceTrustRootSigningIdentity::from_base64_seed(key_id, &seed)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    println!("{}", identity.public_key_base64());
    Ok(())
}

fn required_env(name: &str) -> io::Result<String> {
    env::var(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} is required")))
}
