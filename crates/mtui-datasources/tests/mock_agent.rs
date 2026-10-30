//! Shared mock ssh-agent for the integration suite.
//!
//! One copy for the whole `it` binary (see `log_capture`): holds decrypted
//! private keys and signs in-process.

use mtui_datasources::obs::{AgentKeys, ObsError};
use ssh_key::{HashAlg, PrivateKey, PublicKey, Signature};

/// A mock ssh-agent holding a set of decrypted private keys, signing in-process.
///
/// Reproduces a real agent's behaviour (raw signature bytes, RSA→rsa-sha2-512
/// via the `hash_alg` flag) without a socket, so every agent-selection branch
/// runs offline. Mishandled signature packing fails the
/// `_assert_authz_verifies` round-trip.
pub struct MockAgent {
    keys: Vec<PrivateKey>,
    fail: bool,
}

impl MockAgent {
    pub fn with(keys: Vec<PrivateKey>) -> Self {
        Self { keys, fail: false }
    }
    pub fn failing() -> Self {
        Self {
            keys: vec![],
            fail: true,
        }
    }
}

#[async_trait::async_trait]
impl AgentKeys for MockAgent {
    async fn identities(&mut self) -> Result<Vec<PublicKey>, ObsError> {
        if self.fail {
            return Err(ObsError::Config(
                "could not query the ssh-agent: boom".to_owned(),
            ));
        }
        Ok(self.keys.iter().map(|k| k.public_key().clone()).collect())
    }

    async fn sign(
        &mut self,
        public: &PublicKey,
        _hash_alg: Option<HashAlg>,
        data: &[u8],
    ) -> Result<Signature, ObsError> {
        let key = self
            .keys
            .iter()
            .find(|k| k.public_key().key_data() == public.key_data())
            .expect("agent asked to sign with a key it holds");
        Ok(signature::Signer::sign(key, data))
    }
}
