use tempfile::NamedTempFile;

use crate::utils::encrypt_decrypt_file::decrypt_from_file;
use crate::utils::encrypt_decrypt_file::encrypt_to_file;

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
