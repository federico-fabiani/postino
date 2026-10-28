//! PGP/MIME structure, RFC 3156.
//!
//! This module is deliberately crypto-free. It finds the exact bytes a caller must hand to an
//! OpenPGP implementation and assembles the RFC 3156 containers around bytes it is given. Nothing
//! here parses, validates or produces a key, a signature or a ciphertext.
//!
//! The received side preserves the signed entity byte-for-byte, including line endings and header
//! folding: a verifier that re-serializes the part would silently break every signature. The
//! outgoing side only ever places already-serialized bytes inside a new multipart.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use sha2::{Digest, Sha256};

use crate::util::{hex_lower, normalize_crlf};
use crate::{Limits, MimeError};

/// Longest accepted multipart boundary, per RFC 2046.
pub const MAX_BOUNDARY_BYTES: usize = 70;
/// A PGP/MIME container has exactly two parts.
pub const PGP_MIME_PARTS: usize = 2;
/// Hard ceiling for an armored payload this module will copy into a message.
pub const MAX_PGP_ARMOR_BYTES: usize = 16 * 1024 * 1024;
/// Signature hash algorithm name written by [`serialize_signed`].
pub const MICALG_SHA256: &str = "pgp-sha256";

const SIGNATURE_PROTOCOL: &str = "application/pgp-signature";
const ENCRYPTED_PROTOCOL: &str = "application/pgp-encrypted";

/// The two RFC 3156 shapes this module recognizes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgpMimeEntity {
    /// A `multipart/signed` message: the exact signed entity and the decoded detached signature.
    Signed {
        /// The first part exactly as it appeared, headers included, without the CRLF that belongs
        /// to the following boundary delimiter. This is the byte string the signature covers.
        entity: Vec<u8>,
        /// The detached signature, transfer-decoded. It may be binary or ASCII armor.
        signature: Vec<u8>,
        /// The `protocol` parameter, lowercased.
        protocol: String,
        /// The `micalg` parameter, lowercased, when present.
        micalg: Option<String>,
    },
    /// A `multipart/encrypted` message: the decoded OpenPGP payload and its version part.
    Encrypted {
        /// The encrypted payload, transfer-decoded.
        payload: Vec<u8>,
        /// The `protocol` parameter, lowercased.
        protocol: String,
        /// The version identification part body, trimmed.
        version: String,
    },
}

/// The two pieces of an outgoing RFC 5322 message that PGP/MIME wraps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingParts {
    /// The outer RFC 5322 headers (`From`, `To`, `Subject`, `Date`, `Message-ID`, `MIME-Version`
    /// and any other non-content header), each line ending in CRLF.
    pub message_headers: Vec<u8>,
    /// The MIME entity: everything from the first `Content-*` header through the end of the body.
    pub entity: Vec<u8>,
}

/// Recognize a PGP/MIME message and extract the bytes crypto needs.
///
/// `Ok(None)` means the message is not PGP/MIME or declares another protocol (for example an
/// S/MIME `multipart/signed`); `Err` means it declares PGP/MIME but its structure cannot be read
/// safely. Limits are the same ones [`crate::parse`] applies.
pub fn detect(raw: &[u8], limits: &Limits) -> Result<Option<PgpMimeEntity>, MimeError> {
    let limits = limits.clamped();
    if raw.len() > limits.max_bytes {
        return Err(MimeError::TooLarge { size: raw.len(), limit: limits.max_bytes });
    }
    let Some(header_end) = header_block_end(raw) else {
        return Err(MimeError::Malformed { reason: "the input has no header block" });
    };
    if header_end > limits.max_header_bytes {
        return Err(MimeError::TooLarge { size: header_end, limit: limits.max_header_bytes });
    }
    let header_block = raw.get(..header_end).unwrap_or_default();
    let fields = header_fields(header_block);
    let Some(content_type) = field(&fields, "content-type") else { return Ok(None) };
    let Some((media_type, params)) = parse_content_type(content_type) else {
        return Err(MimeError::Malformed { reason: "the Content-Type field is not type/subtype" });
    };
    let protocol = param(&params, "protocol").map(str::to_ascii_lowercase);
    match media_type.as_str() {
        "multipart/signed" if protocol.as_deref() == Some(SIGNATURE_PROTOCOL) => {
            let entity = split_signed(raw, &fields, &limits)?;
            Ok(Some(entity))
        }
        "multipart/encrypted" if protocol.as_deref() == Some(ENCRYPTED_PROTOCOL) => {
            let entity = split_encrypted(raw, &fields, &limits)?;
            Ok(Some(entity))
        }
        _ => Ok(None),
    }
}

fn split_signed(raw: &[u8], fields: &[(String, String)], limits: &Limits) -> Result<PgpMimeEntity, MimeError> {
    let Some((_, params)) = field(fields, "content-type").and_then(parse_content_type) else {
        return Err(MimeError::Malformed { reason: "the signed message has no Content-Type" });
    };
    let Some(boundary) = param(&params, "boundary") else {
        return Err(MimeError::Malformed { reason: "multipart/signed has no boundary parameter" });
    };
    let header_end = header_block_end(raw).ok_or(MimeError::Malformed { reason: "the input has no header block" })?;
    let body = raw.get(header_end..).unwrap_or_default();
    let parts = split_multipart(body, boundary)?;
    if parts.len() != PGP_MIME_PARTS {
        return Err(MimeError::Malformed { reason: "multipart/signed must contain exactly two parts" });
    }
    let entity = parts.first().copied().unwrap_or_default().to_vec();
    if entity.len() > limits.max_part_bytes {
        return Err(MimeError::TooLarge { size: entity.len(), limit: limits.max_part_bytes });
    }
    let signature_part = parts.get(1).copied().unwrap_or_default();
    let signature = decode_part(signature_part, "application/pgp-signature", limits)?;
    Ok(PgpMimeEntity::Signed {
        entity,
        signature,
        protocol: SIGNATURE_PROTOCOL.to_owned(),
        micalg: param(&params, "micalg").map(str::to_ascii_lowercase),
    })
}

fn split_encrypted(raw: &[u8], fields: &[(String, String)], limits: &Limits) -> Result<PgpMimeEntity, MimeError> {
    let Some((_, params)) = field(fields, "content-type").and_then(parse_content_type) else {
        return Err(MimeError::Malformed { reason: "the encrypted message has no Content-Type" });
    };
    let Some(boundary) = param(&params, "boundary") else {
        return Err(MimeError::Malformed { reason: "multipart/encrypted has no boundary parameter" });
    };
    let header_end = header_block_end(raw).ok_or(MimeError::Malformed { reason: "the input has no header block" })?;
    let body = raw.get(header_end..).unwrap_or_default();
    let parts = split_multipart(body, boundary)?;
    if parts.len() != PGP_MIME_PARTS {
        return Err(MimeError::Malformed { reason: "multipart/encrypted must contain exactly two parts" });
    }
    let version_part = parts.first().copied().unwrap_or_default();
    let version = decode_part(version_part, "application/pgp-encrypted", limits)?;
    let version = String::from_utf8_lossy(&version).trim().to_owned();
    if version.is_empty() || version.len() > 64 {
        return Err(MimeError::Malformed { reason: "the PGP/MIME version part is empty or too long" });
    }
    let payload_part = parts.get(1).copied().unwrap_or_default();
    let payload = decode_part(payload_part, "application/octet-stream", limits)?;
    Ok(PgpMimeEntity::Encrypted { payload, protocol: ENCRYPTED_PROTOCOL.to_owned(), version })
}

/// Split an outgoing RFC 5322 message into outer headers and MIME entity.
///
/// The first header whose name starts with `content-` begins the entity. The serializer writes
/// `MIME-Version` before it, so it stays with the outer message.
pub fn split_outgoing(raw: &[u8]) -> Result<OutgoingParts, MimeError> {
    let Some(header_end) = header_block_end(raw) else {
        return Err(MimeError::Malformed { reason: "the outgoing message has no header block" });
    };
    let mut position = 0usize;
    while position < header_end {
        let Some(rest) = raw.get(position..header_end) else { break };
        let Some(offset) = rest.iter().position(|byte| *byte == b'\n') else { break };
        let line_end = position.saturating_add(offset);
        let line = raw.get(position..line_end).unwrap_or_default();
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            break;
        }
        let is_continuation = line.first().is_some_and(|byte| *byte == b' ' || *byte == b'\t');
        if !is_continuation {
            let name_end = line.iter().position(|byte| *byte == b':').unwrap_or(line.len());
            let name = line.get(..name_end).unwrap_or_default();
            if name.len() >= 8 && name.get(..8).is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"content-")) {
                return Ok(OutgoingParts {
                    message_headers: raw.get(..position).unwrap_or_default().to_vec(),
                    entity: raw.get(position..).unwrap_or_default().to_vec(),
                });
            }
        }
        position = line_end.saturating_add(1);
    }
    Err(MimeError::Malformed { reason: "the outgoing message has no MIME entity headers" })
}

/// Wrap an exact MIME entity and an armored detached signature in `multipart/signed`.
///
/// The signature must cover `entity` exactly as passed. The serializer writes one CRLF before the
/// first boundary; extraction strips that one separator, so an entity that itself ends with CRLF
/// still round-trips byte for byte.
pub fn serialize_signed(entity: &[u8], signature_armor: &str, micalg: &str) -> Result<Vec<u8>, MimeError> {
    if entity.is_empty() {
        return Err(MimeError::Encode { reason: "the signed entity is empty" });
    }
    if !is_token(micalg) || micalg.len() > 32 {
        return Err(MimeError::Encode { reason: "micalg must be a short token such as pgp-sha256" });
    }
    let armor = checked_armor(signature_armor, "-----BEGIN PGP SIGNATURE-----", "-----END PGP SIGNATURE-----")?;
    let boundary = boundary_for("signed", &[entity, armor.as_bytes()]);
    let mut out = Vec::with_capacity(entity.len().saturating_add(armor.len()).saturating_add(512));
    out.extend_from_slice(
        format!(
            "Content-Type: multipart/signed; micalg={micalg}; protocol=\"application/pgp-signature\"; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(entity);
    out.extend_from_slice(
        format!(
            "\r\n--{boundary}\r\nContent-Type: application/pgp-signature; name=\"signature.asc\"\r\nContent-Description: OpenPGP digital signature\r\nContent-Disposition: attachment; filename=\"signature.asc\"\r\nContent-Transfer-Encoding: 7bit\r\n\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(armor.as_bytes());
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok(out)
}

/// Wrap an armored OpenPGP message in `multipart/encrypted` with the required version part.
pub fn serialize_encrypted(payload_armor: &str) -> Result<Vec<u8>, MimeError> {
    let armor = checked_armor(payload_armor, "-----BEGIN PGP MESSAGE-----", "-----END PGP MESSAGE-----")?;
    let boundary = boundary_for("encrypted", &[armor.as_bytes()]);
    let mut out = Vec::with_capacity(armor.len().saturating_add(640));
    out.extend_from_slice(
        format!("Content-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\n")
            .as_bytes(),
    );
    out.extend_from_slice(b"Content-Type: application/pgp-encrypted\r\nContent-Description: PGP/MIME version identification\r\n\r\nVersion: 1\r\n");
    out.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Type: application/octet-stream; name=\"encrypted.asc\"\r\nContent-Description: OpenPGP encrypted message\r\nContent-Disposition: inline; filename=\"encrypted.asc\"\r\nContent-Transfer-Encoding: 7bit\r\n\r\n"
        )
        .as_bytes(),
    );
    out.extend_from_slice(armor.as_bytes());
    out.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok(out)
}

fn checked_armor(armor: &str, begin: &str, end: &str) -> Result<String, MimeError> {
    if armor.len() > MAX_PGP_ARMOR_BYTES {
        return Err(MimeError::TooLarge { size: armor.len(), limit: MAX_PGP_ARMOR_BYTES });
    }
    let normalized = normalize_crlf(armor);
    let trimmed = normalized.trim_end_matches(['\r', '\n']);
    if !trimmed.starts_with(begin) || !trimmed.ends_with(end) || !trimmed.is_ascii() || trimmed.contains('\0') {
        return Err(MimeError::Encode { reason: "the payload is not one ASCII-armored OpenPGP block" });
    }
    if trimmed.lines().any(|line| line.len() > 998) {
        return Err(MimeError::Encode { reason: "an armored line is longer than 998 bytes" });
    }
    Ok(trimmed.trim_end_matches(['\r', '\n']).to_owned())
}

fn boundary_for(label: &str, parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(label.as_bytes());
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    format!("=_postino_pgp_{}", hex_lower(digest.get(..9).unwrap_or_default()))
}

fn header_block_end(raw: &[u8]) -> Option<usize> {
    let mut position = 0usize;
    while position < raw.len() {
        let rest = raw.get(position..)?;
        let offset = rest.iter().position(|byte| *byte == b'\n')?;
        let line_end = position.saturating_add(offset);
        let line = raw.get(position..line_end)?;
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            return Some(line_end.saturating_add(1).min(raw.len()));
        }
        position = line_end.saturating_add(1);
    }
    None
}

fn header_fields(block: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(block);
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = fields.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            fields.push((name.trim().to_owned(), value.trim().to_owned()));
        }
    }
    fields
}

fn field<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields.iter().find(|(field, _)| field.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

fn parse_content_type(value: &str) -> Option<(String, Vec<(String, String)>)> {
    let mut pieces: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in value.char_indices() {
        match character {
            '\\' if quoted && !escaped => escaped = true,
            '"' if !escaped => quoted = !quoted,
            ';' if !quoted => {
                pieces.push(value.get(start..index)?);
                start = index.saturating_add(1);
            }
            _ => escaped = false,
        }
        if character != '\\' {
            escaped = false;
        }
    }
    pieces.push(value.get(start..)?);
    let media_type = pieces.first()?.trim().to_ascii_lowercase();
    if media_type.is_empty() || !media_type.contains('/') || media_type.split('/').count() != 2 {
        return None;
    }
    let mut params = Vec::new();
    for piece in pieces.iter().skip(1).take(16) {
        let Some((name, raw)) = piece.split_once('=') else { continue };
        let name = name.trim().to_ascii_lowercase();
        if name.is_empty() {
            continue;
        }
        let raw = raw.trim();
        let value = if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
            let inner = raw.get(1..raw.len().saturating_sub(1)).unwrap_or_default();
            let mut unescaped = String::with_capacity(inner.len());
            let mut escape = false;
            for character in inner.chars() {
                if escape {
                    unescaped.push(character);
                    escape = false;
                } else if character == '\\' {
                    escape = true;
                } else {
                    unescaped.push(character);
                }
            }
            unescaped
        } else {
            raw.to_owned()
        };
        params.push((name, value));
    }
    Some((media_type, params))
}

fn param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

fn split_multipart<'a>(body: &'a [u8], boundary: &str) -> Result<Vec<&'a [u8]>, MimeError> {
    if boundary.is_empty() || boundary.len() > MAX_BOUNDARY_BYTES || !boundary.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        return Err(MimeError::Malformed { reason: "the multipart boundary is empty, too long or not printable ASCII" });
    }
    let delimiter = format!("--{boundary}");
    let close = format!("--{boundary}--");
    let mut parts: Vec<&[u8]> = Vec::new();
    let mut start: Option<usize> = None;
    let mut position = 0usize;
    while position < body.len() {
        let line_end = body
            .get(position..)
            .and_then(|rest| rest.iter().position(|byte| *byte == b'\n'))
            .map_or(body.len(), |offset| position.saturating_add(offset));
        let line = body.get(position..line_end).unwrap_or_default();
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = line.strip_suffix(b" ").unwrap_or(line);
        let line = line.strip_suffix(b"\t").unwrap_or(line);
        let is_delimiter = line == delimiter.as_bytes();
        let is_close = line == close.as_bytes();
        if is_delimiter || is_close {
            if let Some(part_start) = start.take() {
                parts.push(trim_boundary_crlf(body.get(part_start..position).unwrap_or_default()));
            }
            if parts.len() > 8 {
                return Err(MimeError::Malformed { reason: "too many multipart sections" });
            }
            if is_close {
                return Ok(parts);
            }
            start = Some(line_end.saturating_add(1).min(body.len()));
        }
        position = line_end.saturating_add(1);
    }
    if let Some(part_start) = start.take() {
        parts.push(trim_boundary_crlf(body.get(part_start..).unwrap_or_default()));
    }
    Ok(parts)
}

fn trim_boundary_crlf(part: &[u8]) -> &[u8] {
    if let Some(stripped) = part.strip_suffix(b"\r\n") {
        return stripped;
    }
    part.strip_suffix(b"\n").unwrap_or(part)
}

fn decode_part(part: &[u8], expected_type: &str, limits: &Limits) -> Result<Vec<u8>, MimeError> {
    let Some(header_end) = header_block_end(part) else {
        return Err(MimeError::Malformed { reason: "a PGP/MIME part has no header block" });
    };
    if header_end > limits.max_header_bytes {
        return Err(MimeError::TooLarge { size: header_end, limit: limits.max_header_bytes });
    }
    let fields = header_fields(part.get(..header_end).unwrap_or_default());
    let Some(content_type) = field(&fields, "content-type") else {
        return Err(MimeError::Malformed { reason: "a PGP/MIME part has no Content-Type" });
    };
    let Some((media_type, _)) = parse_content_type(content_type) else {
        return Err(MimeError::Malformed { reason: "a PGP/MIME part has an invalid Content-Type" });
    };
    if media_type != expected_type {
        return Err(MimeError::Malformed { reason: "a PGP/MIME part has an unexpected Content-Type" });
    }
    let body = part.get(header_end..).unwrap_or_default();
    let encoding = field(&fields, "content-transfer-encoding").unwrap_or("7bit").trim().to_ascii_lowercase();
    let decoded = match encoding.as_str() {
        "" | "7bit" | "8bit" | "binary" => body.to_vec(),
        "base64" => {
            let max_encoded = limits.max_part_bytes.saturating_div(3).saturating_add(1).saturating_mul(4).saturating_add(8);
            if body.len() > max_encoded {
                return Err(MimeError::TooLarge { size: body.len(), limit: max_encoded });
            }
            let compact = body.iter().copied().filter(|byte| !byte.is_ascii_whitespace()).collect::<Vec<u8>>();
            BASE64.decode(&compact).map_err(|_| MimeError::Malformed { reason: "the part is not valid base64" })?
        }
        "quoted-printable" => decode_quoted_printable(body),
        _ => return Err(MimeError::Malformed { reason: "the part uses an unsupported transfer encoding" }),
    };
    if decoded.len() > limits.max_part_bytes {
        return Err(MimeError::TooLarge { size: decoded.len(), limit: limits.max_part_bytes });
    }
    Ok(decoded)
}

fn decode_quoted_printable(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len());
    let mut index = 0usize;
    while index < body.len() {
        let Some(&byte) = body.get(index) else { break };
        if byte != b'=' {
            out.push(byte);
            index = index.saturating_add(1);
            continue;
        }
        if body.get(index + 1) == Some(&b'\n') {
            index = index.saturating_add(2);
            continue;
        }
        if body.get(index + 1) == Some(&b'\r') && body.get(index + 2) == Some(&b'\n') {
            index = index.saturating_add(3);
            continue;
        }
        let high = body.get(index + 1).and_then(|byte| hex_value(*byte));
        let low = body.get(index + 2).and_then(|byte| hex_value(*byte));
        match (high, low) {
            (Some(high), Some(low)) => {
                out.push(high.saturating_mul(16).saturating_add(low));
                index = index.saturating_add(3);
            }
            _ => {
                out.push(byte);
                index = index.saturating_add(1);
            }
        }
    }
    out
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte.saturating_sub(b'0')),
        b'a'..=b'f' => Some(byte.saturating_sub(b'a').saturating_add(10)),
        b'A'..=b'F' => Some(byte.saturating_sub(b'A').saturating_add(10)),
        _ => None,
    }
}

fn is_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNED: &[u8] = b"Content-Type: multipart/signed; micalg=pgp-sha256; protocol=\"application/pgp-signature\"; boundary=\"b1\"\r\n\r\npreamble\r\n--b1\r\nContent-Type: text/plain\r\n\r\nCiao\r\nda qui\r\n--b1\r\nContent-Type: application/pgp-signature\r\n\r\n-----BEGIN PGP SIGNATURE-----\r\nabc\r\n-----END PGP SIGNATURE-----\r\n--b1--\r\n";

    #[test]
    fn extracts_the_exact_signed_entity() {
        let entity = detect(SIGNED, &Limits::default()).expect("detect").expect("pgp");
        let PgpMimeEntity::Signed { entity: signed, signature, micalg, .. } = entity else { panic!("signed") };
        assert_eq!(signed, b"Content-Type: text/plain\r\n\r\nCiao\r\nda qui");
        assert_eq!(signature, b"-----BEGIN PGP SIGNATURE-----\r\nabc\r\n-----END PGP SIGNATURE-----");
        assert_eq!(micalg.as_deref(), Some("pgp-sha256"));
    }

    #[test]
    fn serialization_round_trips_the_entity_byte_for_byte() {
        let armor = "-----BEGIN PGP SIGNATURE-----\r\nYWJj\r\n-----END PGP SIGNATURE-----";
        for entity in [
            b"Content-Type: text/plain\r\n\r\nCiao".as_slice(),
            b"Content-Type: text/plain\r\n\r\nCiao\r\n".as_slice(),
            b"Content-Type: text/plain\r\n\r\nCiao\r\n\r\n".as_slice(),
        ] {
            let raw = serialize_signed(entity, armor, MICALG_SHA256).expect("serialize");
            let PgpMimeEntity::Signed { entity: signed, signature, .. } = detect(&raw, &Limits::default()).expect("detect").expect("pgp") else {
                panic!("signed")
            };
            assert_eq!(signed, entity, "the signed entity changed across serialization");
            assert_eq!(signature, armor.as_bytes());
        }
    }

    #[test]
    fn encrypted_round_trip_keeps_the_payload() {
        let armor = "-----BEGIN PGP MESSAGE-----\r\nYWJj\r\n-----END PGP MESSAGE-----";
        let raw = serialize_encrypted(armor).expect("serialize");
        let PgpMimeEntity::Encrypted { payload, version, .. } = detect(&raw, &Limits::default()).expect("detect").expect("pgp") else {
            panic!("encrypted")
        };
        assert_eq!(payload, armor.as_bytes());
        assert_eq!(version, "Version: 1");
    }

    #[test]
    fn a_non_pgp_multipart_is_not_detected() {
        let raw = b"Content-Type: multipart/signed; protocol=\"application/pkcs7-signature\"; boundary=\"b\"\r\n\r\n--b\r\n\r\nx\r\n--b--\r\n";
        assert!(detect(raw, &Limits::default()).expect("detect").is_none());
    }

    #[test]
    fn split_outgoing_keeps_message_headers_apart() {
        let raw = b"From: a@example.com\r\nSubject: x\r\nMIME-Version: 1.0\r\nContent-Type: text/plain\r\n\r\nbody\r\n";
        let parts = split_outgoing(raw).expect("split");
        assert_eq!(parts.message_headers, b"From: a@example.com\r\nSubject: x\r\nMIME-Version: 1.0\r\n");
        assert_eq!(parts.entity, b"Content-Type: text/plain\r\n\r\nbody\r\n");
    }

    #[test]
    fn the_limits_apply_before_parsing() {
        let limits = Limits { max_bytes: 8, ..Limits::default() };
        assert!(matches!(detect(SIGNED, &limits), Err(MimeError::TooLarge { .. })));
    }
}
