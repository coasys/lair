//! Lair server configuration types. You only need this module if you are
//! configuring a standalone or in-process lair keystore server.

use crate::*;
use one_err::OneErr;
use std::future::Future;
use std::sync::{Arc, Mutex};

const PID_FILE_NAME: &str = "pid_file";
const STORE_FILE_NAME: &str = "store_file";

/// Enum for configuring signature fallback handling.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum LairServerSignatureFallback {
    /// No fallback handling. If a pub key does not exist
    /// in the lair store, a sign_by_pub_key request will error.
    None,

    /// Specify a command to execute on lair server start.
    /// This command will be fed framed json signature requests on stdin,
    /// and is expected to respond to those requests with framed
    /// json responses on stdout.
    #[serde(rename_all = "camelCase")]
    Command {
        /// The program command to execute.
        program: std::path::PathBuf,

        /// Optional arguments to be passed to command on execute.
        args: Option<Vec<String>>,
    },
}

/// Inner config type used by lair servers. This will be wrapped in an
/// `Arc` in the typedef [LairServerConfig].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct LairServerConfigInner {
    /// The connection url for communications between server / client.
    /// - `unix:///path/to/unix/socket?k=Yada`
    /// - `named_pipe:\\.\pipe\my_pipe_name?k=Yada`
    /// - `tcp://127.0.0.1:12345?k=Yada`
    pub connection_url: url::Url,

    /// The pid file for managing a running lair-keystore process
    pub pid_file: std::path::PathBuf,

    /// The sqlcipher store file for persisting secrets
    pub store_file: std::path::PathBuf,

    /// Configuration for managing sign_by_pub_key fallback
    /// in case the pub key does not exist in the lair store.
    pub signature_fallback: LairServerSignatureFallback,

    /// salt for sqlcipher connection
    pub database_salt: BinDataSized<16>,

    /// salt for decrypting runtime data
    pub runtime_secrets_salt: BinDataSized<16>,

    /// argon2id mem_limit for decrypting runtime data
    pub runtime_secrets_mem_limit: u32,

    /// argon2id ops_limit for decrypting runtime data
    pub runtime_secrets_ops_limit: u32,

    /// the runtime context key secret
    pub runtime_secrets_context_key: SecretDataSized<32, 49>,

    /// the server identity signature keypair seed
    pub runtime_secrets_id_seed: SecretDataSized<32, 49>,
}

impl std::fmt::Display for LairServerConfigInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_yaml::to_string(&self).map_err(|_| std::fmt::Error)?;

        // inject some helpful comments
        let mut lines = Vec::new();
        for (id, line) in s.split('\n').enumerate() {
            if id > 0 {
                if line.starts_with("connectionUrl:") {
                    lines.push("");
                    lines.push("# The connection url for communications between server / client.");
                    lines.push("# - `unix:///path/to/unix/socket?k=Yada`");
                    lines.push(
                        "# - `named_pipe:\\\\.\\pipe\\my_pipe_name?k=Yada`",
                    );
                    lines.push("# - (not yet supported) `tcp://127.0.0.1:12345?k=Yada`");
                } else if line.starts_with("pidFile:") {
                    lines.push("");
                    lines.push("# The pid file for managing a running lair-keystore process");
                } else if line.starts_with("storeFile:") {
                    lines.push("");
                    lines.push(
                        "# The sqlcipher store file for persisting secrets",
                    );
                } else if line.starts_with("signatureFallback:") {
                    lines.push("");
                    lines.push(
                        "# Configuration for managing sign_by_pub_key fallback",
                    );
                    lines.push("# in case the pub key does not exist in the lair store.");
                    lines.push("# - `signatureFallback: none`");
                    lines.push("# - ```");
                    lines.push("#   signatureFallback: !command");
                    lines.push("#     # 'program' will resolve to a path, specifying 'echo'");
                    lines.push("#     # will try to run './echo', probably not what you want.");
                    lines.push("#     program: \"./my-executable\"");
                    lines.push("#     # args are optional");
                    lines.push("#     args:");
                    lines.push("#       - test-arg1");
                    lines.push("#       - test-arg2");
                    lines.push("#   ```");
                } else if line.starts_with("databaseSalt:") {
                    lines.push("");
                    lines.push("# -- cryptographic secrets --");
                    lines.push("# If you modify the data below, you risk losing access to your keys.");
                }
            }
            lines.push(line);
        }
        f.write_str(&lines.join("\n"))
    }
}

impl LairServerConfigInner {
    /// decode yaml bytes into a config struct
    pub fn from_bytes(bytes: &[u8]) -> LairResult<Self> {
        serde_yaml::from_slice(bytes).map_err(one_err::OneErr::new)
    }

    /// Construct a new default lair server config instance.
    /// Respects hc_seed_bundle::PwHashLimits.
    pub fn new<P>(
        root_path: P,
        passphrase: SharedLockedArray,
    ) -> impl Future<Output = LairResult<Self>> + 'static + Send
    where
        P: AsRef<std::path::Path>,
    {
        let root_path = root_path.as_ref().to_owned();
        let limits = hc_seed_bundle::PwHashLimits::current();
        async move {
            // default pid_file name is '[root_path]/pid_file'
            let mut pid_file = root_path.clone();
            pid_file.push(PID_FILE_NAME);

            // default store_file name is '[root_path]/store_file'
            let mut store_file = root_path.clone();
            store_file.push(STORE_FILE_NAME);

            // pre-hash the passphrase
            let mut pw_hash = sodoken::SizedLockedArray::<64>::new()?;
            sodoken::blake2b::blake2b_hash(
                &mut *pw_hash.lock(),
                &passphrase.lock().unwrap().lock(),
                None,
            )?;

            // pull the captured argon2id limits
            let ops_limit = limits.as_ops_limit();
            let mem_limit = limits.as_mem_limit();

            // generate an argon2id pre_secret from the passphrase
            let (salt, pre_secret) =
                tokio::task::spawn_blocking(move || -> LairResult<_> {
                    // generate a random salt for the pwhash
                    let mut salt = [0; sodoken::argon2::ARGON2_ID_SALTBYTES];
                    sodoken::random::randombytes_buf(&mut salt)?;

                    let mut pre_secret =
                        sodoken::SizedLockedArray::<32>::new()?;

                    sodoken::argon2::blocking_argon2id(
                        &mut *pre_secret.lock(),
                        &*pw_hash.lock(),
                        &salt,
                        ops_limit,
                        mem_limit,
                    )?;

                    Ok((salt, pre_secret))
                })
                .await
                .map_err(OneErr::new)??;
            let pre_secret = Arc::new(Mutex::new(pre_secret));

            // derive our context secret
            // this will be used to encrypt the context_key
            let mut ctx_secret = sodoken::SizedLockedArray::<32>::new()?;
            sodoken::kdf::derive_from_key(
                &mut *ctx_secret.lock(),
                42,
                b"CtxSecKy",
                &pre_secret.lock().unwrap().lock(),
            )?;
            let ctx_secret = Arc::new(Mutex::new(ctx_secret));

            // derive our signature secret
            // this will be used to encrypt the signature seed
            let mut id_secret = sodoken::SizedLockedArray::<32>::new()?;
            sodoken::kdf::derive_from_key(
                &mut *id_secret.lock(),
                142,
                b"IdnSecKy",
                &pre_secret.lock().unwrap().lock(),
            )?;
            let id_secret = Arc::new(Mutex::new(id_secret));

            // the context key is used to encrypt our store_file
            let mut context_key = sodoken::SizedLockedArray::<32>::new()?;
            sodoken::random::randombytes_buf(&mut *context_key.lock())?;

            // the sign seed derives our signature keypair
            // which allows us to authenticate server identity
            let mut id_seed = sodoken::SizedLockedArray::<32>::new()?;
            sodoken::random::randombytes_buf(&mut *id_seed.lock())?;

            // server identity encryption keypair
            let mut id_pk = [0; sodoken::crypto_box::XSALSA_PUBLICKEYBYTES];
            let mut id_sk = sodoken::SizedLockedArray::<32>::new()?;
            sodoken::crypto_box::xsalsa_seed_keypair(
                &mut id_pk,
                &mut id_sk.lock(),
                &id_seed.lock(),
            )?;

            // lock the context key
            let context_key = SecretDataSized::encrypt(
                ctx_secret,
                Arc::new(Mutex::new(context_key)),
            )
            .await?;

            // lock the signature seed
            let id_seed = SecretDataSized::encrypt(
                id_secret,
                Arc::new(Mutex::new(id_seed)),
            )
            .await?;

            // get the signature public key bytes for encoding in the url
            let id_pk: BinDataSized<32> = id_pk.into();

            // on windows, we default to using "named pipes"
            #[cfg(windows)]
            let connection_url = {
                let id = nanoid::nanoid!();
                url::Url::parse(&format!(
                    "named-pipe:\\\\.\\pipe\\{}?k={}",
                    id, id_pk
                ))
                .unwrap()
            };

            // on not-windows, we default to using unix domain sockets
            #[cfg(not(windows))]
            let connection_url = {
                let mut con_path = dunce::canonicalize(root_path)?;
                con_path.push("socket");
                url::Url::parse(&format!(
                    "unix://{}?k={}",
                    con_path.to_str().unwrap(),
                    id_pk
                ))
                .unwrap()
            };

            // generate a random salt for the sqlcipher database
            let mut db_salt = [0; 16];
            sodoken::random::randombytes_buf(&mut db_salt)?;

            // put together the full server config struct
            let config = LairServerConfigInner {
                connection_url,
                pid_file,
                store_file,
                signature_fallback: LairServerSignatureFallback::None,
                database_salt: db_salt.into(),
                runtime_secrets_salt: salt.into(),
                runtime_secrets_mem_limit: mem_limit,
                runtime_secrets_ops_limit: ops_limit,
                runtime_secrets_context_key: context_key,
                runtime_secrets_id_seed: id_seed,
            };

            Ok(config)
        }
    }

    /// Get the connection "scheme". i.e. "unix", "named-pipe", or "tcp".
    pub fn get_connection_scheme(&self) -> &str {
        self.connection_url.scheme()
    }

    /// Get the connection "path". This could have different meanings
    /// depending on if we are a unix domain socket or named pipe, etc.
    pub fn get_connection_path(&self) -> std::path::PathBuf {
        get_connection_path(&self.connection_url)
    }

    /// Get the server pub key BinDataSized<32> bytes from the connectionUrl
    pub fn get_server_pub_key(&self) -> LairResult<BinDataSized<32>> {
        get_server_pub_key_from_connection_url(&self.connection_url)
    }
}

/// Get the connection "path". This could have different meanings
/// depending on if we are a unix domain socket or named pipe, etc.
pub fn get_connection_path(url: &url::Url) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        std::path::PathBuf::from(url.path())
    }

    #[cfg(not(windows))]
    {
        url.to_file_path().expect("The connection url is invalid, as it does not decode to
an absolute file path. The likely cause is that a relative path was used instead of an absolute one.
If that's the case, try using an absolute one instead.")
    }
}

/// Helper utility for extracting a server_pub_key from a connection_url.
pub fn get_server_pub_key_from_connection_url(
    url: &url::Url,
) -> LairResult<BinDataSized<32>> {
    for (k, v) in url.query_pairs() {
        if k == "k" {
            return v.parse();
        }
    }
    Err("no server_pub_key on connection_url".into())
}

/// Configuration for running a lair-keystore server instance.
pub type LairServerConfig = Arc<LairServerConfigInner>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_config_yaml() {
        let tempdir = tempdir::TempDir::new("example").unwrap();
        let passphrase = Arc::new(Mutex::new(sodoken::LockedArray::from(
            b"passphrase".to_vec(),
        )));
        let mut srv = hc_seed_bundle::PwHashLimits::Minimum
            .with_exec(|| {
                LairServerConfigInner::new(tempdir.path(), passphrase)
            })
            .await
            .unwrap();

        println!("-- server config start --");
        println!("{}", &srv);
        println!("-- server config end --");
        assert_eq!(tempdir.path(), srv.pid_file.parent().unwrap(),);

        srv.signature_fallback = LairServerSignatureFallback::Command {
            program: std::path::Path::new("./my-executable").into(),
            args: Some(vec!["test-arg1".into(), "test-arg2".into()]),
        };

        println!("-- server config start --");
        println!("{}", &srv);
        println!("-- server config end --");
    }
}
