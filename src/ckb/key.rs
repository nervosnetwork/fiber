use log::{debug, warn};
use std::{fs, path::Path};
pub use tentacle::secio::SecioKeyPair as KeyPair;

use rand::{thread_rng, Rng};
use std::io::{Error, ErrorKind, Read, Write};

// TODO: we need to securely erase the key.
pub(crate) fn generate_random_key() -> [u8; 32] {
    loop {
        let mut key: [u8; 32] = [0; 32];
        thread_rng().fill(&mut key);
        if KeyPair::secp256k1_raw_key(key).is_ok() {
            return key;
        }
    }
}

pub(crate) fn write_secret_to_file(secret: &[u8], path: &Path) -> Result<(), Error> {
    fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(path)
        .and_then(|mut file| {
            file.write_all(secret)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o400))
            }
            #[cfg(not(unix))]
            {
                let mut permissions = file.metadata()?.permissions();
                permissions.set_readonly(true);
                file.set_permissions(permissions)
            }
        })
}

pub(crate) fn read_secret_key(path: &Path) -> Result<Option<KeyPair>, Error> {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let warn = |m: bool, d: &str| {
        if m {
            warn!(
                "Your network secret file's permission is not {}, path: {:?}. \
                Please fix it as soon as possible",
                d, path
            )
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        warn(
            file.metadata()?.permissions().mode() & 0o177 != 0,
            "less than 0o600",
        );
    }
    #[cfg(not(unix))]
    {
        warn(!file.metadata()?.permissions().readonly(), "readonly");
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).and_then(|_read_size| {
        KeyPair::secp256k1_raw_key(&buf)
            .map(Some)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "invalid secret key data"))
    })
}

pub fn read_or_generate_private_key(path: &Path) -> Result<KeyPair, Error> {
    match read_secret_key(path)? {
        Some(key) => Ok(key),
        None => {
            debug!("Generating random secret key");
            let key = generate_random_key();
            write_secret_to_file(&key, path)?;
            Ok(read_secret_key(path)?.expect("key created from previous step"))
        }
    }
}
