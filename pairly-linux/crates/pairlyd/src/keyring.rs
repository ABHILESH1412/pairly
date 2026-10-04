//! This PC's identity key, kept in the desktop keyring (the freedesktop Secret Service:
//! gnome-keyring, KWallet), which stores it encrypted with the login password. Everything else
//! secret (the registry's pairing secrets) is sealed with a key derived from it.
//!
//! - An identity in the old `identity.key` file moves into the keyring (checked by reading it
//!   back), and the file is deleted.
//! - Without a keyring, a new install keeps the identity in that `0600` file.
//! - If the keyring is unavailable but devices are already paired, we refuse to start rather
//!   than make a new identity, which would lose every pairing.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use pairly_crypto::{FileKeyStore, IdentityKeypair, KEY_LEN, KeyStore};
use tracing::{info, warn};
use zbus::Connection;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const SERVICE: &str = "org.freedesktop.secrets";
const PATH: &str = "/org/freedesktop/secrets";
const DEFAULT_COLLECTION: &str = "/org/freedesktop/secrets/aliases/default";
/// Ordinary calls; unlocking can wait for the user to type a password.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
/// At login we may start before the login keyring is unlocked (by PAM, moments later) and
/// before the desktop can show an unlock prompt: wait this long for it before prompting.
const UNLOCK_WAIT: Duration = Duration::from_secs(60);
const UNLOCK_POLL: Duration = Duration::from_secs(2);

/// `(session, parameters, value, content type)`, the Secret Service's secret struct.
type Secret = (OwnedObjectPath, Vec<u8>, Vec<u8>, String);

/// Load this PC's identity, moving it into the keyring if needed (see the module docs).
pub async fn identity(data_dir: &Path, paired_devices: usize) -> Result<IdentityKeypair> {
    let file = FileKeyStore::new(data_dir.join("identity.key"));
    let from_file = file.load().context("reading identity.key")?;
    let attributes = attributes(data_dir);
    let keyring = match Keyring::open().await {
        Ok(k) => k,
        Err(e) => {
            if let Some(kp) = from_file {
                warn!(error = %e, "no keyring: keeping the identity in identity.key");
                return Ok(kp);
            }
            if paired_devices > 0 {
                bail!(
                    "this PC's identity is kept in the keyring, which isn't available ({e:#}). \\
                     Unlock or start your keyring (gnome-keyring, KWallet) and restart pairlyd"
                );
            }
            warn!(error = %e, "no keyring: keeping a new identity in identity.key");
            let kp = IdentityKeypair::generate();
            file.store(&kp).context("writing identity.key")?;
            return Ok(kp);
        }
    };
    if let Some(kp) = keyring.load(&attributes).await? {
        if let Some(old) = from_file {
            if old.public() == kp.public() {
                remove(file.path());
            } else {
                // Shouldn't happen; keep the file aside rather than lose it.
                let aside = file.path().with_extension("key.old");
                warn!(file = %aside.display(), "identity.key differs from the keyring's; kept aside");
                let _ = std::fs::rename(file.path(), &aside);
            }
        }
        return Ok(kp);
    }
    let kp = from_file.unwrap_or_else(IdentityKeypair::generate);
    keyring.store(&attributes, &kp).await?;
    // Only delete the file once the keyring gives the same key back.
    match keyring.load(&attributes).await? {
        Some(back) if back.public() == kp.public() => {
            remove(file.path());
            info!("this PC's identity is now kept in the keyring");
            Ok(kp)
        }
        _ => {
            warn!("the keyring didn't keep the identity; using identity.key");
            file.store(&kp).context("writing identity.key")?;
            Ok(kp)
        }
    }
}

/// Identify our item; the data folder keeps separate instances (`--config`) apart. The
/// `application` tag predates the app ID and stays as it is: changing it would lose the stored
/// identity.
fn attributes(data_dir: &Path) -> HashMap<&'static str, String> {
    HashMap::from([
        ("application", "dev.pairly".to_owned()),
        ("kind", "identity".to_owned()),
        ("data-dir", data_dir.display().to_string()),
    ])
}

fn remove(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        warn!(file = %path.display(), error = %e, "couldn't delete the old identity file");
    }
}

struct Keyring {
    conn: Connection,
    session: OwnedObjectPath,
}

impl Keyring {
    async fn open() -> Result<Self> {
        let conn = Connection::session().await?;
        // "plain": the secret crosses the local session bus as-is, which only this user's
        // processes can reach (and those can ask the keyring directly anyway).
        let (_, session): (OwnedValue, OwnedObjectPath) = call(
            &conn,
            PATH,
            "org.freedesktop.Secret.Service",
            "OpenSession",
            &("plain", Value::from("")),
        )
        .await
        .context("no Secret Service on the session bus")?;
        Ok(Self { conn, session })
    }

    async fn search(
        &self,
        attributes: &HashMap<&str, String>,
    ) -> Result<(Option<OwnedObjectPath>, Option<OwnedObjectPath>)> {
        let (unlocked, locked): (Vec<OwnedObjectPath>, Vec<OwnedObjectPath>) = call(
            &self.conn,
            PATH,
            "org.freedesktop.Secret.Service",
            "SearchItems",
            &(attributes,),
        )
        .await?;
        Ok((unlocked.into_iter().next(), locked.into_iter().next()))
    }

    async fn load(&self, attributes: &HashMap<&str, String>) -> Result<Option<IdentityKeypair>> {
        let item = match self.search(attributes).await? {
            (Some(item), _) => item,
            (None, Some(item)) => {
                // Give the login a moment to unlock it before asking the user.
                info!("the keyring is locked; waiting for it to be unlocked");
                let waited = tokio::time::Instant::now();
                loop {
                    if waited.elapsed() >= UNLOCK_WAIT {
                        self.unlock(&item).await?;
                        break item;
                    }
                    tokio::time::sleep(UNLOCK_POLL).await;
                    if let (Some(item), _) = self.search(attributes).await? {
                        break item;
                    }
                }
            }
            (None, None) => return Ok(None),
        };
        let secrets: HashMap<OwnedObjectPath, Secret> = call(
            &self.conn,
            PATH,
            "org.freedesktop.Secret.Service",
            "GetSecrets",
            &(vec![item.clone()], &self.session),
        )
        .await?;
        let (_, _, value, _) = secrets
            .get(&item)
            .ok_or_else(|| anyhow!("the keyring returned no secret"))?;
        let bytes: [u8; KEY_LEN] = value
            .as_slice()
            .try_into()
            .map_err(|_| anyhow!("the keyring's identity has the wrong length"))?;
        Ok(Some(IdentityKeypair::from_secret(bytes)))
    }

    async fn store(&self, attributes: &HashMap<&str, String>, kp: &IdentityKeypair) -> Result<()> {
        let properties: HashMap<&str, Value<'_>> = HashMap::from([
            (
                "org.freedesktop.Secret.Item.Label",
                Value::from("Pairly device identity"),
            ),
            (
                "org.freedesktop.Secret.Item.Attributes",
                Value::from(attributes.clone()),
            ),
        ]);
        let secret = (
            &self.session,
            Vec::<u8>::new(),
            kp.secret_bytes().to_vec(),
            "application/octet-stream",
        );
        let (_, prompt): (OwnedObjectPath, OwnedObjectPath) = call(
            &self.conn,
            DEFAULT_COLLECTION,
            "org.freedesktop.Secret.Collection",
            "CreateItem",
            &(properties, secret, true),
        )
        .await
        .context("storing the identity in the keyring")?;
        self.prompt(&prompt).await
    }

    async fn unlock(&self, item: &OwnedObjectPath) -> Result<()> {
        let (_, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) = call(
            &self.conn,
            PATH,
            "org.freedesktop.Secret.Service",
            "Unlock",
            &(vec![item.clone()],),
        )
        .await?;
        self.prompt(&prompt).await
    }

    /// Show the keyring's prompt (e.g. for the password) if it needs one, and wait for it.
    async fn prompt(&self, prompt: &ObjectPath<'_>) -> Result<()> {
        if prompt.as_str() == "/" {
            return Ok(());
        }
        let proxy = zbus::Proxy::new(
            &self.conn,
            SERVICE,
            prompt.to_owned(),
            "org.freedesktop.Secret.Prompt",
        )
        .await?;
        let mut completed = proxy.receive_signal("Completed").await?;
        proxy.call_method("Prompt", &("",)).await?;
        let signal = tokio::time::timeout(PROMPT_TIMEOUT, completed.next())
            .await
            .context("the keyring prompt wasn't answered")?
            .ok_or_else(|| anyhow!("the keyring went away"))?;
        let (dismissed, _): (bool, OwnedValue) = signal.body().deserialize()?;
        if dismissed {
            bail!("the keyring prompt was dismissed");
        }
        Ok(())
    }
}

async fn call<R>(
    conn: &Connection,
    path: &str,
    interface: &str,
    method: &str,
    body: &(impl serde::Serialize + zbus::zvariant::DynamicType),
) -> Result<R>
where
    R: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
{
    let reply = tokio::time::timeout(
        CALL_TIMEOUT,
        conn.call_method(Some(SERVICE), path, Some(interface), method, body),
    )
    .await
    .with_context(|| format!("the keyring didn't answer {method}"))??;
    Ok(reply.body().deserialize()?)
}
