use aes_gcm::aead::{generic_array::GenericArray, Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand::RngCore;
use scrypt::{scrypt, Params};
use std::fmt::Debug;
use std::fs;
use std::path::Path;
use tracing::debug;

const VERSION: u8 = 0;
const NONCE_LEN: usize = 12;
const SALT_LEN: usize = 16;

fn derive_key_from_password(password: &[u8], salt: &[u8]) -> Key<Aes256Gcm> {
    let mut key = [0u8; 32];
    let params = Params::recommended();
    scrypt(password, salt, &params, &mut key).expect("checked output key length");
    *Key::<Aes256Gcm>::from_slice(&key)
}

pub fn encrypt_to_file<P: AsRef<Path>>(
    file: P,
    plain_text: &[u8],
    password: &[u8],
) -> Result<(), String> {
    let mut salt = [0u8; SALT_LEN];
    let mut nonce = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut salt);
    rand::thread_rng().fill_bytes(&mut nonce);

    let key = derive_key_from_password(password, &salt);
    let cipher = Aes256Gcm::new(&key);
    let ciphertext = cipher
        .encrypt(GenericArray::from_slice(&nonce), plain_text)
        .map_err(|err| format!("encryption failed: {}", err))?;

    let mut file_bytes = vec![VERSION];
    file_bytes.extend_from_slice(&salt);
    file_bytes.extend_from_slice(&nonce);
    file_bytes.extend_from_slice(&ciphertext);

    fs::write(file, file_bytes).map_err(|err| format!("failed to write to file: {}", err))
}

pub fn decrypt_from_file<P: AsRef<Path> + Debug>(
    file: P,
    password: &[u8],
) -> Result<Vec<u8>, String> {
    debug!("Decrypting key from file {:?}", file);
    let file_bytes = fs::read(file).unwrap();
    let salt = &file_bytes[1..SALT_LEN + 1];
    let nonce = &file_bytes[SALT_LEN + 1..SALT_LEN + NONCE_LEN + 1];
    let ciphertext = &file_bytes[SALT_LEN + NONCE_LEN + 1..];

    let key = derive_key_from_password(password, salt);
    let cipher = Aes256Gcm::new(&key);
    cipher
        .decrypt(Nonce::from_slice(nonce), ciphertext)
        .map_err(|err| format!("decryption failed: {}", err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_encrypt_and_decrypt_success() {
        let password = b"not my password";
        let plain_text = b"my super secret private key data";

        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        encrypt_to_file(path, plain_text, password).unwrap();

        let decrypted = decrypt_from_file(path, password).unwrap();
        assert_eq!(plain_text.to_vec(), decrypted);
    }

    #[test]
    fn test_decrypt_with_wrong_password_should_fail() {
        let password = b"correct_password";
        let wrong_password = b"wrong_password";
        let plain_text = b"private key data";

        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path();

        encrypt_to_file(path, plain_text, password).unwrap();

        let result = decrypt_from_file(path, wrong_password);
        assert!(
            result.is_err(),
            "Decryption should fail with wrong password"
        );
    }
}
