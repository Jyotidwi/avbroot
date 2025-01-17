/*
 * SPDX-FileCopyrightText: 2022-2023 Andrew Gunnerson
 * SPDX-License-Identifier: GPL-3.0-only
 */

use std::{
    collections::BTreeMap,
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    iter,
    sync::atomic::AtomicBool,
};

use cms::signed_data::SignedData;
use const_oid::{db::rfc5912, ObjectIdentifier};
use memchr::memmem;
use prost::Message;
use ring::digest::Context;
use rsa::{Pkcs1v15Sign, RsaPrivateKey};
use sha1::Sha1;
use sha2::Sha256;
use thiserror::Error;
use x509_cert::{der::Encode, Certificate};
use zip::{result::ZipError, write::FileOptions, CompressionMethod, ZipArchive, ZipWriter};

use crate::{
    crypto,
    format::payload::{self, PayloadHeader},
    protobuf::build::tools::releasetools::{ota_metadata::OtaType, OtaMetadata},
    stream::{self, FromReader, HashingReader, HashingWriter},
};

pub const PATH_METADATA: &str = "META-INF/com/android/metadata";
pub const PATH_METADATA_PB: &str = "META-INF/com/android/metadata.pb";
pub const PATH_OTACERT: &str = "META-INF/com/android/otacert";
pub const PATH_PAYLOAD: &str = "payload.bin";
pub const PATH_PROPERTIES: &str = "payload_properties.txt";

const NAME_PAYLOAD_METADATA: &str = "payload_metadata.bin";

pub const PF_NAME: &str = "ota-property-files";
pub const PF_STREAMING_NAME: &str = "ota-streaming-property-files";

const ZIP_EOCD_MAGIC: &[u8; 4] = b"PK\x05\x06";

const COMMENT_MESSAGE: &[u8] = b"signed by avbroot\0";

const LEGACY_SEP: &str = "|";

#[derive(Debug, Error)]
pub enum Error {
    #[error("Cannot find OTA signature footer magic")]
    OtaMagicNotFound,
    #[error("Cannot find EOCD magic")]
    EocdMagicNotFound,
    #[error("EOCD magic found in archive comment")]
    EocdMagicInComment,
    #[error("Zip is too small to contain EOCD")]
    ZipTooSmall,
    #[error("Signature offset exceeds archive comment size")]
    SignatureOffsetTooLarge,
    #[error("Expected exactly one CMS embedded certificate, but found {0}")]
    NotOneCmsCertificate(usize),
    #[error("Expected exactly one CMS SignerInfo, but found {0}")]
    NotOneCmsSignerInfo(usize),
    #[error("Unsupported digest algorithm: {0}")]
    UnsupportedDigestAlgorithm(ObjectIdentifier),
    #[error("Unsupported signature algorithm: {0}")]
    UnsupportedSignatureAlgorithm(ObjectIdentifier),
    #[error("Invalid legacy metadata line: {0:?}")]
    InvalidLegacyMetadataLine(String),
    #[error("Unsupported legacy metadata field: {key:?} = {value:?}")]
    UnsupportedLegacyMetadataField { key: String, value: String },
    #[error("Expected entry offsets {expected:?}, but have {actual:?}")]
    MismatchedPropertyFiles { expected: String, actual: String },
    #[error("Property files {0:?} exceed {1} byte reserved space")]
    InsufficientReservedSpace(String, usize),
    #[error("Invalid property file entry: {0:?}")]
    InvalidPropertyFileEntry(String),
    #[error("Missing entry in OTA zip: {0}")]
    MissingZipEntry(&'static str),
    #[error("CMS signing error")]
    CmsSign(#[from] crypto::Error),
    #[error("Payload error")]
    Payload(#[from] payload::Error),
    #[error("Failed to decode protobuf message")]
    ProtobufDecode(#[from] prost::DecodeError),
    #[error("SPKI error")]
    Spki(#[from] pkcs8::spki::Error),
    #[error("x509 DER error")]
    Der(#[from] x509_cert::der::Error),
    #[error("RSA error")]
    Rsa(#[from] rsa::Error),
    #[error("Zip error")]
    Zip(#[from] ZipError),
    #[error("I/O error")]
    Io(#[from] io::Error),
}

type Result<T> = std::result::Result<T, Error>;

pub fn parse_protobuf_metadata(data: &[u8]) -> Result<OtaMetadata> {
    Ok(OtaMetadata::decode(data)?)
}

/// Synthesize protobuf structure from legacy plain-text metadata.
pub fn parse_legacy_metadata(data: &str) -> Result<OtaMetadata> {
    let mut metadata = OtaMetadata::default();

    for line in data.split('\n') {
        if line.is_empty() {
            continue;
        }

        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| Error::InvalidLegacyMetadataLine(line.to_owned()))?;
        let unsupported = || Error::UnsupportedLegacyMetadataField {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        // Booleans are represented by the presence or absence of `<key>=yes`.
        let parse_yes = || match value {
            "yes" => Ok(true),
            _ => Err(unsupported()),
        };
        let parse_list = || {
            value
                .split(LEGACY_SEP)
                .map(|s| s.to_owned())
                .collect::<Vec<_>>()
        };

        match key {
            "ota-type" => {
                match OtaType::from_str_name(value).ok_or_else(unsupported)? {
                    t @ (OtaType::Ab | OtaType::Block) => metadata.set_type(t),
                    // Not allowed by AOSP in the legacy format.
                    _ => return Err(unsupported()),
                }
            }
            "ota-wipe" => metadata.wipe = parse_yes()?,
            "ota-retrofit-dynamic-partitions" => {
                metadata.retrofit_dynamic_partitions = parse_yes()?
            }
            "ota-downgrade" => metadata.downgrade = parse_yes()?,
            "ota-required-cache" => {
                metadata.required_cache = value.parse().map_err(|_| unsupported())?;
            }
            "post-build" => {
                let p = metadata.postcondition.get_or_insert_with(Default::default);
                p.build = parse_list();
            }
            "post-build-incremental" => {
                let p = metadata.postcondition.get_or_insert_with(Default::default);
                p.build_incremental = value.to_owned();
            }
            "post-sdk-level" => {
                let p = metadata.postcondition.get_or_insert_with(Default::default);
                p.sdk_level = value.to_owned();
            }
            "post-security-patch-level" => {
                let p = metadata.postcondition.get_or_insert_with(Default::default);
                p.security_patch_level = value.to_owned();
            }
            "post-timestamp" => {
                let p = metadata.postcondition.get_or_insert_with(Default::default);
                p.timestamp = value.parse().map_err(|_| unsupported())?;
            }
            "pre-device" => {
                let p = metadata.precondition.get_or_insert_with(Default::default);
                p.device = parse_list();
            }
            "pre-build" => {
                let p = metadata.precondition.get_or_insert_with(Default::default);
                p.build = parse_list();
            }
            "pre-build-incremental" => {
                let p = metadata.precondition.get_or_insert_with(Default::default);
                p.build_incremental = value.to_owned();
            }
            "spl-downgrade" => metadata.spl_downgrade = parse_yes()?,
            k if k.ends_with("-property-files") => {
                metadata
                    .property_files
                    .insert(key.to_owned(), value.to_owned());
            }
            _ => {
                // Ignore. Some OEMs insert values that aren't defined in AOSP.
            }
        }
    }

    Ok(metadata)
}

/// Generate the legacy plain-text and modern protobuf serializations of the
/// given metadata instance.
fn serialize_metadata(metadata: &OtaMetadata) -> Result<(String, Vec<u8>)> {
    use std::fmt::Write;

    let mut pairs = BTreeMap::<String, String>::new();

    // Other types are not allowed by AOSP in the legacy format.
    if let t @ (OtaType::Ab | OtaType::Block) = metadata.r#type() {
        pairs.insert("ota-type".to_owned(), t.as_str_name().to_owned());
    }
    if metadata.wipe {
        pairs.insert("ota-wipe".to_owned(), "yes".to_owned());
    }
    if metadata.retrofit_dynamic_partitions {
        pairs.insert(
            "ota-retrofit-dynamic-partitions".to_owned(),
            "yes".to_owned(),
        );
    }
    if metadata.downgrade {
        pairs.insert("ota-downgrade".to_owned(), "yes".to_owned());
    }

    pairs.insert(
        "ota-required-cache".to_owned(),
        metadata.required_cache.to_string(),
    );

    if let Some(p) = &metadata.postcondition {
        pairs.insert("post-build".to_owned(), p.build.join(LEGACY_SEP));
        pairs.insert(
            "post-build-incremental".to_owned(),
            p.build_incremental.clone(),
        );
        pairs.insert("post-sdk-level".to_owned(), p.sdk_level.clone());
        pairs.insert(
            "post-security-patch-level".to_owned(),
            p.security_patch_level.clone(),
        );
        pairs.insert("post-timestamp".to_owned(), p.timestamp.to_string());
    }

    if let Some(p) = &metadata.precondition {
        pairs.insert("pre-device".to_owned(), p.device.join(LEGACY_SEP));
        if !p.build.is_empty() {
            pairs.insert("pre-build".to_owned(), p.build.join(LEGACY_SEP));
            pairs.insert(
                "pre-build-incremental".to_owned(),
                p.build_incremental.clone(),
            );
        }
    }

    if metadata.spl_downgrade {
        pairs.insert("spl-downgrade".to_owned(), "yes".to_owned());
    }

    pairs.extend(metadata.property_files.clone());

    let legacy_metadata = pairs.into_iter().fold(String::new(), |mut output, (k, v)| {
        let _ = writeln!(output, "{k}={v}");
        output
    });
    let modern_metadata = metadata.encode_to_vec();

    Ok((legacy_metadata, modern_metadata))
}

#[derive(Clone, Debug)]
pub struct ZipEntry {
    pub name: String,
    pub offset: u64,
    pub size: u64,
}

/// Parse OTA property files string.
pub fn parse_property_files(data: &str) -> Result<Vec<ZipEntry>> {
    let mut result = vec![];

    for entry in data.trim_end().split(',') {
        let mut pieces = entry.split(':');

        let name = pieces
            .next()
            .map(|p| p.to_owned())
            .ok_or_else(|| Error::InvalidPropertyFileEntry(entry.to_owned()))?;
        let offset = pieces
            .next()
            .and_then(|p| p.parse::<u64>().ok())
            .ok_or_else(|| Error::InvalidPropertyFileEntry(entry.to_owned()))?;
        let size = pieces
            .next()
            .and_then(|p| p.parse::<u64>().ok())
            .ok_or_else(|| Error::InvalidPropertyFileEntry(entry.to_owned()))?;

        if pieces.next().is_some() {
            return Err(Error::InvalidPropertyFileEntry(entry.to_owned()));
        }

        result.push(ZipEntry { name, offset, size });
    }

    Ok(result)
}

/// Compute the property files entries listing the offsets and sizes to every
/// zip entry.
fn compute_property_files(
    pf_name: &str,
    entries: &[ZipEntry],
    max_length: Option<usize>,
) -> Result<String> {
    let compute = |path: &'static str| -> Result<String> {
        let entry = entries
            .iter()
            .find(|e| e.name == path)
            .ok_or_else(|| Error::MissingZipEntry(path))?;
        let name = path.rsplit_once('/').map_or(path, |p| p.1);

        Ok(format!("{name}:{}:{}", entry.offset, entry.size))
    };

    let mut tokens = vec![];

    if pf_name == PF_NAME {
        tokens.push(compute(NAME_PAYLOAD_METADATA)?);
    }

    for path in [PATH_PAYLOAD, PATH_PROPERTIES] {
        tokens.push(compute(path)?);
    }

    for path in [
        "apex_info.pb",
        "care_map.pb",
        "care_map.txt",
        "compatibility.zip",
    ] {
        if let Ok(token) = compute(path) {
            tokens.push(token);
        }
    }

    if max_length.is_none() {
        tokens.push(format!("metadata:{}", " ".repeat(15)));
        tokens.push(format!("metadata.pb:{}", " ".repeat(15)));
    } else {
        tokens.push(compute(PATH_METADATA)?);
        tokens.push(compute(PATH_METADATA_PB)?);
    }

    let mut joined = tokens.join(",");

    if let Some(l) = max_length {
        if joined.len() > l {
            return Err(Error::InsufficientReservedSpace(joined, l));
        }

        let remain = l - joined.len();
        joined.extend(iter::repeat(' ').take(remain));
    }

    Ok(joined)
}

// Add fake payload_metadata.bin entry, covering the header + header signature
// regions of the payload.
fn add_payload_metadata_entry(
    entries: &mut Vec<ZipEntry>,
    payload_metadata_size: u64,
) -> Result<()> {
    let payload_offset = entries
        .iter()
        .find(|e| e.name == PATH_PAYLOAD)
        .ok_or_else(|| Error::MissingZipEntry(PATH_PAYLOAD))?
        .offset;
    entries.push(ZipEntry {
        name: NAME_PAYLOAD_METADATA.to_owned(),
        offset: payload_offset,
        size: payload_metadata_size,
    });

    Ok(())
}

/// Add metadata files to the output OTA zip. `zip_entries` is the list of
/// [`ZipEntry`] already written to `zip_writer`. `next_offset` is the current
/// file offset (where the next zip entry's local header begins).
/// `metadata` is the OTA metadata protobuf message from the original OTA.
/// `payload_metadata_size` is the size of the new payload's metadata and
/// metadata signature regions.
///
/// The zip file's backing file position MUST BE set to where the central
/// directory would start.
pub fn add_metadata(
    zip_entries: &[ZipEntry],
    zip_writer: &mut ZipWriter<impl Write>,
    next_offset: u64,
    metadata: &OtaMetadata,
    payload_metadata_size: u64,
) -> Result<OtaMetadata> {
    let mut metadata = metadata.clone();
    let options = FileOptions::default().compression_method(CompressionMethod::Stored);

    let mut zip_entries = zip_entries.to_owned();
    add_payload_metadata_entry(&mut zip_entries, payload_metadata_size)?;

    // Compute initial property files with reserved space as placeholders to
    // store the self-referential metadata entries later.
    metadata.property_files.clear();
    for pf in [PF_NAME, PF_STREAMING_NAME] {
        metadata.property_files.insert(
            pf.to_owned(),
            compute_property_files(pf, &zip_entries, None)?,
        );
    }

    // Add the placeholders to a temporary zip to compute final property files.
    let (temp_legacy_offset, temp_modern_offset) = {
        let (legacy_raw, modern_raw) = serialize_metadata(&metadata)?;
        let mut writer = ZipWriter::new_streaming(Cursor::new(Vec::new()));

        writer.start_file_with_extra_data(PATH_METADATA, options)?;
        let legacy_offset = writer.end_extra_data()?;
        writer.write_all(legacy_raw.as_bytes())?;

        writer.start_file_with_extra_data(PATH_METADATA_PB, options)?;
        let modern_offset = writer.end_extra_data()?;
        writer.write_all(&modern_raw)?;

        zip_entries.push(ZipEntry {
            name: PATH_METADATA.to_owned(),
            offset: next_offset + legacy_offset,
            size: legacy_raw.len() as u64,
        });
        zip_entries.push(ZipEntry {
            name: PATH_METADATA_PB.to_owned(),
            offset: next_offset + modern_offset,
            size: modern_raw.len() as u64,
        });

        (next_offset + legacy_offset, next_offset + modern_offset)
    };

    // Compute the final property files using the offsets of the fake entries.
    for (key, value) in &mut metadata.property_files {
        *value = compute_property_files(key, &zip_entries, Some(value.len()))?;
    }

    // Add the final metadata files to the real zip.
    {
        let (legacy_raw, modern_raw) = serialize_metadata(&metadata)?;

        zip_writer.start_file_with_extra_data(PATH_METADATA, options)?;
        let legacy_offset = zip_writer.end_extra_data()?;
        zip_writer.write_all(legacy_raw.as_bytes())?;

        zip_writer.start_file_with_extra_data(PATH_METADATA_PB, options)?;
        let modern_offset = zip_writer.end_extra_data()?;
        zip_writer.write_all(&modern_raw)?;

        assert_eq!(legacy_offset, temp_legacy_offset);
        assert_eq!(modern_offset, temp_modern_offset);
    }

    Ok(metadata)
}

/// Verify that the zip entry offsets and sizes match the OTA metadata.
pub fn verify_metadata(
    reader: impl Read + Seek,
    metadata: &OtaMetadata,
    payload_metadata_size: u64,
) -> Result<()> {
    let mut zip_reader = ZipArchive::new(reader)?;
    let mut zip_entries = vec![];

    for i in 0..zip_reader.len() {
        let entry = zip_reader.by_index(i)?;
        zip_entries.push(ZipEntry {
            name: entry.name().to_owned(),
            offset: entry.data_start(),
            size: entry.size(),
        });
    }

    add_payload_metadata_entry(&mut zip_entries, payload_metadata_size)?;

    for (key, value) in &metadata.property_files {
        let new_value = compute_property_files(key, &zip_entries, Some(value.len()))?;
        if *value != new_value {
            return Err(Error::MismatchedPropertyFiles {
                expected: value.clone(),
                actual: new_value,
            });
        }
    }

    Ok(())
}

/// Parse the CMS signature from the OTA zip comment. Returns the decoded CMS
/// [`SignedData`] structure and the length of the file (from the beginning)
/// that's covered by the signature. This does not perform any parsing of zip
/// data structures.
fn parse_ota_sig(mut reader: impl Read + Seek) -> Result<(SignedData, u64)> {
    let file_size = reader.seek(SeekFrom::End(0))?;

    reader.seek(SeekFrom::Current(-6))?;
    let mut footer = [0u8; 6];
    reader.read_exact(&mut footer)?;

    let abs_eoc_offset = u16::from_le_bytes(footer[0..2].try_into().unwrap());
    let sig_magic = u16::from_le_bytes(footer[2..4].try_into().unwrap());
    let comment_size = u16::from_le_bytes(footer[4..6].try_into().unwrap());

    if sig_magic != 0xffff {
        return Err(Error::OtaMagicNotFound);
    }

    // RecoverySystem.verifyPackage() always assumes a non-zip64 EOCD, so we'll
    // do the same.
    let eocd_size = u64::from(22 + comment_size);
    if file_size < eocd_size {
        return Err(Error::ZipTooSmall);
    } else if u64::from(abs_eoc_offset) > eocd_size {
        return Err(Error::SignatureOffsetTooLarge);
    }

    reader.seek(SeekFrom::Start(file_size - eocd_size))?;
    let mut eocd = vec![0u8; eocd_size as usize];
    reader.read_exact(&mut eocd)?;

    let mut eocd_magic_iter = memmem::find_iter(&eocd, ZIP_EOCD_MAGIC);
    if eocd_magic_iter.next() != Some(0) {
        return Err(Error::EocdMagicNotFound);
    }
    if eocd_magic_iter.next().is_some() {
        return Err(Error::EocdMagicInComment);
    }

    let sig_offset = eocd_size as usize - usize::from(abs_eoc_offset);
    let sd = crypto::parse_cms(&eocd[sig_offset..eocd_size as usize - 6])?;
    // The signature covers everything aside from the archive comment and its
    // length field.
    let hashed_size = file_size - 2 - u64::from(comment_size);

    Ok((sd, hashed_size))
}

/// Verify an OTA zip against its embedded certificates. This function makes no
/// assertion about whether the certificate is actually trusted. Returns the
/// embedded certificate.
///
/// CMS signed attributes are intentionally not supported because AOSP recovery
/// does not support them either. It expects the CMS [`SignedData`] structure to
/// be used for nothing more than a raw signature transport mechanism.
pub fn verify_ota(mut reader: impl Read + Seek, cancel_signal: &AtomicBool) -> Result<Certificate> {
    let (sd, hashed_size) = parse_ota_sig(&mut reader)?;

    // Make sure the certificate in the CMS structure matches the otacert zip
    // entry.
    let certs = crypto::get_cms_certs(&sd);
    if certs.len() != 1 {
        return Err(Error::NotOneCmsCertificate(certs.len()));
    }

    let cert = &certs[0];
    let public_key = crypto::get_public_key(cert)?;

    // Make sure this is a signature scheme we can handle. There's currently no
    // Rust library to verify arbitrary CMS signatures for large files without
    // fully reading them into memory.
    if sd.signer_infos.0.len() != 1 {
        return Err(Error::NotOneCmsSignerInfo(sd.signer_infos.0.len()));
    }

    let signer = sd.signer_infos.0.get(0).unwrap();
    if signer.digest_alg.oid != rfc5912::ID_SHA_256 && signer.digest_alg.oid != rfc5912::ID_SHA_1 {
        return Err(Error::UnsupportedDigestAlgorithm(signer.digest_alg.oid));
    } else if signer.signature_algorithm.oid != rfc5912::RSA_ENCRYPTION
        && signer.signature_algorithm.oid != rfc5912::SHA_256_WITH_RSA_ENCRYPTION
    {
        return Err(Error::UnsupportedSignatureAlgorithm(
            signer.signature_algorithm.oid,
        ));
    }

    // Manually hash the parts of the file covered by the signature.
    reader.seek(SeekFrom::Start(0))?;

    // We support SHA1 for verification only.
    let (algorithm, scheme) = if signer.digest_alg.oid == rfc5912::ID_SHA_256 {
        (&ring::digest::SHA256, Pkcs1v15Sign::new::<Sha256>())
    } else {
        (
            &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
            Pkcs1v15Sign::new::<Sha1>(),
        )
    };

    let mut hashing_reader = HashingReader::new(reader, Context::new(algorithm));

    stream::copy_n(&mut hashing_reader, io::sink(), hashed_size, cancel_signal)?;

    let (_, context) = hashing_reader.finish();
    let digest = context.finish();

    // Verify the signature against the public key.
    public_key.verify(scheme, digest.as_ref(), signer.signature.as_bytes())?;

    Ok(cert.clone())
}

/// Get and parse the protobuf-encoded OTA metadata, the PEM-encoded otacert,
/// the payload header, and the payload properties from an OTA zip.
pub fn parse_zip_ota_info(
    reader: impl Read + Seek,
) -> Result<(OtaMetadata, Certificate, PayloadHeader, String)> {
    let mut zip = ZipArchive::new(reader)?;

    let metadata = {
        let mut entry = zip.by_name(PATH_METADATA_PB)?;
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        OtaMetadata::decode(buf.as_slice())?
    };

    let certificate = {
        let entry = zip.by_name(PATH_OTACERT)?;
        crypto::read_pem_cert(entry)?
    };

    let header = {
        let entry = zip.by_name(PATH_PAYLOAD)?;
        PayloadHeader::from_reader(entry)?
    };

    let properties = {
        let mut entry = zip.by_name(PATH_PROPERTIES)?;
        let mut buf = String::new();
        entry.read_to_string(&mut buf)?;
        buf
    };

    Ok((metadata, certificate, header, properties))
}

/// A writer that produces a signapk-style signed zip file with a whole-file
/// signature stored in the zip archive comment. The data will be left in an
/// unusable state if [`Self::finish()`] is not called.
pub struct SigningWriter<W: Write> {
    inner: HashingWriter<W>,
    // Android only supports non-zip64 EOCD.
    queue: [u8; 22],
    used: usize,
}

impl<W: Write> SigningWriter<W> {
    pub fn new(inner: W) -> Self {
        Self {
            inner: HashingWriter::new(inner, Context::new(&ring::digest::SHA256)),
            queue: Default::default(),
            used: 0,
        }
    }

    pub fn finish(mut self, key: &RsaPrivateKey, cert: &Certificate) -> Result<W> {
        if self.used < self.queue.len() {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "Too small to contain EOCD").into(),
            );
        } else if &self.queue[..4] != b"PK\x05\x06" {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "EOCD magic not found").into());
        } else if &self.queue[20..22] != b"\0\0" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Archive comment is not 0 bytes",
            )
            .into());
        }

        // Chop off the archive comment size field and write the remaining data.
        self.inner.write_all(&self.queue[..20])?;

        let (mut raw_writer, context) = self.inner.finish();
        let digest = context.finish();

        let cms_signature = crypto::cms_sign_external(key, cert, digest.as_ref())?;
        let cms_signature_der = cms_signature.to_der()?;

        let mut comment = COMMENT_MESSAGE.to_vec();
        comment.extend(&cms_signature_der);

        let comment_size = comment.len() + 6;

        // Absolute value of the offset of the signature from the end of the
        // archive comment.
        comment.extend((cms_signature_der.len() as u16 + 6).to_le_bytes());

        // Magic value.
        comment.extend(b"\xff\xff");

        // EOCD archive comment size.
        comment.extend(((comment_size) as u16).to_le_bytes());

        if let Some(o) = memmem::find(&comment, ZIP_EOCD_MAGIC) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Archive comment contains EOCD magic at offset {o}"),
            )
            .into());
        }

        // Write EOCD comment size field, which was removed before.
        raw_writer.write_all(&((comment_size) as u16).to_le_bytes())?;

        // Finally, write the comment.
        raw_writer.write_all(&comment)?;

        Ok(raw_writer)
    }
}

impl<W: Write> Write for SigningWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let (front, back) = buf.split_at(buf.len().saturating_sub(self.queue.len()));
        if front.is_empty() {
            // Write data from the front of the queue while keeping it as full
            // as possible.
            let n_from_queue = (self.used + back.len()).saturating_sub(self.queue.len());
            self.inner.write_all(&self.queue[..n_from_queue])?;

            // Move unused queued bytes to the front.
            self.queue.rotate_left(n_from_queue);
            self.used -= n_from_queue;

            // Add the remaining data to the queue.
            self.queue[self.used..self.used + back.len()].copy_from_slice(back);
            self.used += back.len();
        } else {
            // We have enough data in the back to fill the entire queue.
            self.inner.write_all(&self.queue[..self.used])?;
            self.inner.write_all(front)?;

            self.queue.copy_from_slice(back);
            self.used = self.queue.len();
        }

        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
