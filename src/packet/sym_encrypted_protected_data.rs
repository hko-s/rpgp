use std::io;

use byteorder::WriteBytesExt;
use log::debug;
use nom::bytes::streaming::take;
use nom::combinator::map_res;
use nom::number::streaming::be_u8;
use rand::{thread_rng, CryptoRng, Rng};
use sha2::Sha256;

use crate::crypto::aead::AeadAlgorithm;
use crate::crypto::sym::SymmetricKeyAlgorithm;
use crate::errors::{Error, IResult, Result};
use crate::packet::PacketTrait;
use crate::ser::Serialize;
use crate::types::{Tag, Version};

/// Symmetrically Encrypted Integrity Protected Data Packet
/// <https://tools.ietf.org/html/rfc4880.html#section-5.12>
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SymEncryptedProtectedData {
    packet_version: Version,
    data: Data,
}

#[derive(Clone, PartialEq, Eq, derive_more::Debug)]
pub enum Data {
    V1 {
        #[debug("{}", hex::encode(data))]
        data: Vec<u8>,
    },
    V2 {
        sym_alg: SymmetricKeyAlgorithm,
        aead: AeadAlgorithm,
        chunk_size: u8,
        #[debug("{}", hex::encode(salt))]
        salt: [u8; 32],
        #[debug("{}", hex::encode(data))]
        data: Vec<u8>,
    },
}

impl SymEncryptedProtectedData {
    /// Parses a `SymEncryptedData` packet from the given slice.
    pub fn from_slice(packet_version: Version, input: &[u8]) -> Result<Self> {
        ensure!(input.len() > 1, "invalid input length");
        let (_, data) = parse()(input)?;

        Ok(SymEncryptedProtectedData {
            data,
            packet_version,
        })
    }

    /// Encrypts the data using the given symmetric key.
    pub fn encrypt_with_rng<R: CryptoRng + Rng>(
        rng: R,
        alg: SymmetricKeyAlgorithm,
        key: &[u8],
        plaintext: &[u8],
    ) -> Result<Self> {
        let data = alg.encrypt_protected_with_rng(rng, key, plaintext)?;

        Ok(SymEncryptedProtectedData {
            packet_version: Default::default(),
            data: Data::V1 { data },
        })
    }

    /// Same as [`encrypt_with_rng`], but uses [`thread_rng`] for RNG.
    ///
    /// [`encrypt_with_rng`]: SymEncryptedProtectedData::encrypt_with_rng
    /// [`thread_rng`]: rand::thread_rng
    pub fn encrypt(alg: SymmetricKeyAlgorithm, key: &[u8], plaintext: &[u8]) -> Result<Self> {
        Self::encrypt_with_rng(&mut thread_rng(), alg, key, plaintext)
    }

    pub fn data(&self) -> &Data {
        &self.data
    }

    pub fn data_as_slice(&self) -> &[u8] {
        match &self.data {
            Data::V1 { data } => data,
            Data::V2 { data, .. } => data,
        }
    }

    pub fn version(&self) -> usize {
        match self.data {
            Data::V1 { .. } => 1,
            Data::V2 { .. } => 2,
        }
    }

    /// Decrypts the inner data, returning the result.
    pub fn decrypt(
        &self,
        session_key: &[u8],
        sym_alg: Option<SymmetricKeyAlgorithm>,
    ) -> Result<Vec<u8>> {
        match &self.data {
            Data::V1 { data } => {
                let mut data = data.clone();
                let res = sym_alg
                    .expect("v1")
                    .decrypt_protected(session_key, &mut data)?;
                Ok(res.to_vec())
            }
            Data::V2 {
                sym_alg,
                aead,
                chunk_size,
                salt,
                data,
            } => {
                // Initial key material is the session key.
                let ikm = session_key;

                // Salt is used.
                let salt = Some(&salt[..]);

                let info = [
                    Tag::SymEncryptedProtectedData.encode(), // packet type
                    0x02,                                    // version
                    (*sym_alg).into(),
                    (*aead).into(),
                    *chunk_size,
                ];

                let chunk_size = expand_chunk_size(*chunk_size);
                let hk = hkdf::Hkdf::<Sha256>::new(salt, ikm);
                let mut okm = [0u8; 42];
                hk.expand(&info, &mut okm).expect("42");
                debug!("info: {} - hkdf: {}", hex::encode(info), hex::encode(okm));
                let message_key = &okm[..sym_alg.key_size()];
                let raw_iv_len = aead.nonce_size() - 8;
                let iv = &okm[sym_alg.key_size()..sym_alg.key_size() + raw_iv_len];
                let mut nonce = vec![0u8; aead.nonce_size()];
                nonce[..raw_iv_len].copy_from_slice(iv);

                debug!("message_key: {}", hex::encode(message_key));
                debug!("iv: {}", hex::encode(iv));
                debug!("nonce: {}", hex::encode(&nonce));

                let mut data = data.clone();

                debug!(
                    "data {}, chunk_size {} - {}",
                    hex::encode(&data),
                    chunk_size,
                    data.len()
                );
                let mut out = Vec::new();
                let chunk_size = usize::try_from(chunk_size)?;

                // There are n chunks, n auth tags + 1 final auth tag
                let offset = data.len() - aead.tag_size();
                let (main_chunks, final_auth_tag) = data.split_at_mut(offset);

                let mut chunk_index: u64 = 0;
                for chunk in main_chunks.chunks_mut(chunk_size + aead.tag_size()) {
                    let offset = chunk.len() - aead.tag_size();
                    let (chunk, auth_tag) = chunk.split_at_mut(offset);

                    debug!(
                        "chunk {} - tag {}",
                        hex::encode(&chunk),
                        hex::encode(&auth_tag)
                    );

                    aead.decrypt_in_place(sym_alg, message_key, &nonce, &info, auth_tag, chunk)?;
                    debug!("decrypted {}", hex::encode(&chunk));
                    out.extend_from_slice(chunk);

                    // Update nonce to include the next chunk index
                    chunk_index += 1;
                    let l = nonce.len() - 8;
                    nonce[l..].copy_from_slice(&chunk_index.to_be_bytes());
                }

                // verify final auth tag
                debug!("final auth tag: {}", hex::encode(&final_auth_tag));

                // Associated data is extended with number of plaintext octets.
                let size = out.len() as u64;
                let mut final_info = info.to_vec();
                final_info.extend_from_slice(&size.to_be_bytes());

                // Update final nonce
                debug!("final nonce {}", hex::encode(&nonce));
                debug!("final auth {}", hex::encode(&final_info));

                aead.decrypt_in_place(
                    sym_alg,
                    message_key,
                    &nonce,
                    &final_info,
                    final_auth_tag,
                    &mut [][..], // encrypts empty string
                )?;

                Ok(out)
            }
        }
    }
}

impl Serialize for SymEncryptedProtectedData {
    fn to_writer<W: io::Write>(&self, writer: &mut W) -> Result<()> {
        match &self.data {
            Data::V1 { data } => {
                writer.write_u8(0x01)?;
                writer.write_all(data)?;
            }
            Data::V2 {
                sym_alg,
                aead,
                chunk_size,
                salt,
                data,
            } => {
                writer.write_u8(0x02)?;
                writer.write_u8((*sym_alg).into())?;
                writer.write_u8((*aead).into())?;
                writer.write_u8(*chunk_size)?;
                writer.write_all(salt)?;
                writer.write_all(data)?;
            }
        }
        Ok(())
    }
}

impl PacketTrait for SymEncryptedProtectedData {
    fn packet_version(&self) -> Version {
        self.packet_version
    }

    fn tag(&self) -> Tag {
        Tag::SymEncryptedProtectedData
    }
}

fn expand_chunk_size(s: u8) -> u32 {
    1u32 << (s as u32 + 6)
}

fn parse() -> impl Fn(&[u8]) -> IResult<&[u8], Data> {
    move |i: &[u8]| {
        let (i, version) = be_u8(i)?;
        match version {
            0x01 => Ok((&[][..], Data::V1 { data: i.to_vec() })),
            0x02 => {
                let (i, sym_alg) = map_res(be_u8, SymmetricKeyAlgorithm::try_from)(i)?;
                let (i, aead) = map_res(be_u8, AeadAlgorithm::try_from)(i)?;
                let (i, chunk_size) = be_u8(i)?;
                let (i, salt) = take(32usize)(i)?;

                Ok((
                    &[][..],
                    Data::V2 {
                        sym_alg,
                        aead,
                        chunk_size,
                        salt: salt.try_into().expect("size checked"),
                        data: i.to_vec(),
                    },
                ))
            }
            _ => Err(nom::Err::Error(Error::Unsupported(format!(
                "unknown SymEncryptedProtecedData version {}",
                version
            )))),
        }
    }
}
