use ckb_hash::new_blake2b;
use std::{fs, path::Path};
use tracing::warn;

// TODO: we need to securely erase the key.
// We wrap the key in a struct to obtain create a function to obtain secret entropy from this key.
// Unfortunately, SecioKeyPair does not allow us to obtain the secret key from the key pair.
pub struct KeyPair([u8; 32]);

use tentacle::secio::SecioKeyPair;

use rand::{thread_rng, Rng};
use std::io::{Error, ErrorKind, Read, Write};

impl KeyPair {
    pub fn generate_random_key() -> Self {
        loop {
            let mut key: [u8; 32] = [0; 32];
            thread_rng().fill(&mut key);
            if Self::try_from(key.as_slice()).is_ok() {
                return Self(key);
            }
        }
    }

    pub fn read_or_generate(path: &Path) -> Result<Self, Error> {
        match Self::read_from_file(path)? {
            Some(key) => Ok(key),
            None => {
                let key = Self::generate_random_key();
                key.write_to_file(path)?;
                Ok(Self::read_from_file(path)?.expect("key created from previous step"))
            }
        }
    }

    pub fn write_to_file(&self, path: &Path) -> Result<(), Error> {
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(path)
            .and_then(|mut file| {
                file.write_all(self.as_ref())?;
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

    pub fn read_from_file(path: &Path) -> Result<Option<Self>, Error> {
        read_secret_key(path)
    }
}

impl AsRef<[u8]> for KeyPair {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl From<KeyPair> for SecioKeyPair {
    fn from(val: KeyPair) -> Self {
        SecioKeyPair::secp256k1_raw_key(val.0).expect("key must have been validated")
    }
}

impl TryFrom<&[u8]> for KeyPair {
    type Error = Error;

    fn try_from(value: &[u8]) -> Result<Self, Self::Error> {
        if value.len() < 32 {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "invalid secret key length",
            ));
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(value);
        if SecioKeyPair::secp256k1_raw_key(key).is_err() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "invalid secret key data",
            ));
        }
        Ok(Self(key))
    }
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
        KeyPair::try_from(buf.as_slice())
            .map(Some)
            .map_err(|_| Error::new(ErrorKind::InvalidData, "invalid secret key data"))
    })
}

pub(crate) fn blake2b_hash_with_salt(data: &[u8], salt: &[u8]) -> [u8; 32] {
    let mut hasher = new_blake2b();
    hasher.update(salt);
    hasher.update(data);
    let mut result = [0u8; 32];
    hasher.finalize(&mut result);
    result
}
