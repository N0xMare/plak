use blake2::{Blake2s256, Digest};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, aead::AeadInPlace};
use snow::{
    params::{CipherChoice, DHChoice, HashChoice},
    resolvers::CryptoResolver,
    types::{Cipher, Dh, Hash, Random},
};
use zeroize::Zeroizing;

pub(crate) struct Resolver;

impl CryptoResolver for Resolver {
    fn resolve_rng(&self) -> Option<Box<dyn Random>> {
        Some(Box::new(Entropy))
    }
    fn resolve_dh(&self, choice: &DHChoice) -> Option<Box<dyn Dh>> {
        matches!(choice, DHChoice::Curve25519)
            .then(|| Box::new(DiffieHellman::default()) as Box<dyn Dh>)
    }
    fn resolve_hash(&self, choice: &HashChoice) -> Option<Box<dyn Hash>> {
        matches!(choice, HashChoice::Blake2s)
            .then(|| Box::new(BlakeHash(Blake2s256::new())) as Box<dyn Hash>)
    }
    fn resolve_cipher(&self, choice: &CipherChoice) -> Option<Box<dyn Cipher>> {
        matches!(choice, CipherChoice::ChaChaPoly)
            .then(|| Box::new(ChaCha::default()) as Box<dyn Cipher>)
    }
}

struct Entropy;
impl Random for Entropy {
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), snow::Error> {
        getrandom::fill(dest).map_err(|_| snow::Error::Rng)
    }
}

#[derive(Default)]
struct DiffieHellman {
    private: Zeroizing<[u8; 32]>,
    public: [u8; 32],
}
impl Dh for DiffieHellman {
    fn name(&self) -> &'static str {
        "25519"
    }
    fn pub_len(&self) -> usize {
        32
    }
    fn priv_len(&self) -> usize {
        32
    }
    fn set(&mut self, private: &[u8]) {
        self.private.copy_from_slice(private);
        self.public =
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(*self.private))
                .to_bytes();
    }
    fn generate(&mut self, rng: &mut dyn Random) -> Result<(), snow::Error> {
        let mut private = Zeroizing::new([0; 32]);
        rng.try_fill_bytes(&mut *private)?;
        self.set(&*private);
        Ok(())
    }
    fn pubkey(&self) -> &[u8] {
        &self.public
    }
    fn privkey(&self) -> &[u8] {
        &*self.private
    }
    fn dh(&self, public: &[u8], out: &mut [u8]) -> Result<(), snow::Error> {
        // snow supplies its MAXDHLEN backing array, not a pub_len-sized slice.
        let public: [u8; 32] = public
            .get(..32)
            .ok_or(snow::Error::Dh)?
            .try_into()
            .map_err(|_| snow::Error::Dh)?;
        let shared = x25519_dalek::StaticSecret::from(*self.private)
            .diffie_hellman(&x25519_dalek::PublicKey::from(public));
        if !shared.was_contributory() {
            return Err(snow::Error::Dh);
        }
        out[..32].copy_from_slice(shared.as_bytes());
        Ok(())
    }
}

#[derive(Default)]
struct ChaCha {
    key: Zeroizing<[u8; 32]>,
}
fn nonce(value: u64) -> [u8; 12] {
    let mut bytes = [0; 12];
    bytes[4..].copy_from_slice(&value.to_le_bytes());
    bytes
}
impl Cipher for ChaCha {
    fn name(&self) -> &'static str {
        "ChaChaPoly"
    }
    fn set(&mut self, key: &[u8; 32]) {
        self.key.copy_from_slice(key);
    }
    fn encrypt(&self, counter: u64, ad: &[u8], plaintext: &[u8], out: &mut [u8]) -> usize {
        out[..plaintext.len()].copy_from_slice(plaintext);
        let cipher = ChaCha20Poly1305::new((&*self.key).into());
        // Detached encryption cannot fail: the caller (snow) bounds the message
        // below ChaCha20's counter limit and provides the tag space.
        let tag = cipher
            .encrypt_in_place_detached((&nonce(counter)).into(), ad, &mut out[..plaintext.len()])
            .unwrap_or_else(|_| unreachable!("bounded Noise message"));
        out[plaintext.len()..plaintext.len() + 16].copy_from_slice(&tag);
        plaintext.len() + 16
    }
    fn decrypt(
        &self,
        counter: u64,
        ad: &[u8],
        ciphertext: &[u8],
        out: &mut [u8],
    ) -> Result<usize, snow::Error> {
        let len = ciphertext
            .len()
            .checked_sub(16)
            .ok_or(snow::Error::Decrypt)?;
        out[..len].copy_from_slice(&ciphertext[..len]);
        ChaCha20Poly1305::new((&*self.key).into())
            .decrypt_in_place_detached(
                (&nonce(counter)).into(),
                ad,
                &mut out[..len],
                ciphertext[len..].into(),
            )
            .map_err(|_| snow::Error::Decrypt)?;
        Ok(len)
    }
}

struct BlakeHash(Blake2s256);
impl Hash for BlakeHash {
    fn name(&self) -> &'static str {
        "BLAKE2s"
    }
    fn block_len(&self) -> usize {
        64
    }
    fn hash_len(&self) -> usize {
        32
    }
    fn reset(&mut self) {
        self.0 = Blake2s256::new();
    }
    fn input(&mut self, data: &[u8]) {
        Digest::update(&mut self.0, data);
    }
    fn result(&mut self, out: &mut [u8]) {
        out[..32].copy_from_slice(&self.0.clone().finalize());
    }
}
