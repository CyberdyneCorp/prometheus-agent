//! Minisign keys and detached signatures for release files.

use std::path::Path;

use minisign::{KeyPair, PublicKey, SecretKeyBox};
use pmt_update::TrustRoot;

/// Writes `<out>/<name>.key` (unencrypted secret key, 0600) and
/// `<out>/<name>.pub`.
pub(super) fn keygen(out: &Path, name: &str) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let KeyPair { pk, sk } =
        KeyPair::generate_unencrypted_keypair().map_err(|e| format!("keygen: {e}"))?;
    let comment = format!("prometheus release key {name}");
    let pub_text = pk
        .to_box()
        .map_err(|e| e.to_string())?
        .into_string()
        .replace("minisign public key", &comment);
    let sk_text = sk.to_box(None).map_err(|e| e.to_string())?.into_string();
    let key_path = out.join(format!("{name}.key"));
    pmt_core::private_fs::write_atomic(&key_path, sk_text.as_bytes())
        .map_err(|e| format!("{}: {e}", key_path.display()))?;
    let pub_path = out.join(format!("{name}.pub"));
    std::fs::write(&pub_path, pub_text).map_err(|e| format!("{}: {e}", pub_path.display()))?;
    println!("wrote {} and {}", key_path.display(), pub_path.display());
    Ok(())
}

/// Signs each file with `key_text` (an unencrypted minisign secret key
/// file's text; the workflow passes it through an environment variable so
/// CI secrets never touch the disk).
pub(super) fn sign_files(
    key_text: &str,
    require_embedded: bool,
    public_key_out: Option<&Path>,
    files: &[impl AsRef<Path>],
) -> Result<(), String> {
    let sk = SecretKeyBox::from_string(key_text)
        .and_then(SecretKeyBox::into_unencrypted_secret_key)
        .map_err(|e| format!("not an unencrypted minisign secret key: {e}"))?;
    let pk = PublicKey::from_secret_key(&sk).map_err(|e| e.to_string())?;
    if require_embedded {
        require_embedded_key(&pk)?;
    }
    if let Some(out) = public_key_out {
        let text = pk.to_box().map_err(|e| e.to_string())?.into_string();
        std::fs::write(out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    for file in files {
        let file = file.as_ref();
        let data = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let signature = sign(&pk, &sk, &data, file)?;
        let mut out = file.as_os_str().to_os_string();
        out.push(".minisig");
        std::fs::write(&out, signature).map_err(|e| format!("{}: {e}", file.display()))?;
        println!("signed {}", file.display());
    }
    Ok(())
}

/// The `.minisig` text of `data`; the trusted comment names the file.
fn sign(
    pk: &PublicKey,
    sk: &minisign::SecretKey,
    data: &[u8],
    file: &Path,
) -> Result<String, String> {
    let name = file.file_name().unwrap_or_default().to_string_lossy();
    let trusted = format!("file:{name}");
    minisign::sign(Some(pk), sk, data, Some(&trusted), None)
        .map(minisign::SignatureBox::into_string)
        .map_err(|e| format!("{}: {e}", file.display()))
}

/// A real release must be signed by a key the binary trusts.
fn require_embedded_key(pk: &PublicKey) -> Result<(), String> {
    let text = pk.to_box().map_err(|e| e.to_string())?.into_string();
    let candidate = TrustRoot::from_keys(&[("signing", &text)]).map_err(|e| e.to_string())?;
    let id = candidate.keys()[0].id().to_owned();
    let embedded = TrustRoot::embedded();
    let trusted = embedded
        .keys()
        .iter()
        .any(|k| k.id() == id && matches!(k.role(), "primary" | "standby"));
    if trusted {
        Ok(())
    } else {
        Err(format!(
            "signing key {id} is neither the primary nor the standby key embedded in crates/pmt-update/keys/"
        ))
    }
}
