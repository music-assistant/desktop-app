// ABOUTME: FLAC decoder conformance tests against the reference implementation
// ABOUTME: Tripwire for regressions in the pinned flac-codec dependency

//! Conformance tests for [`FlacDecoder`].
//!
//! Fixtures in `tests/data/` are produced by `gen_fixtures.sh`: audio is
//! encoded with the **reference** flac encoder and the expected PCM is the
//! **reference** flac decoder's output. Our decoder (built on the pinned
//! `flac-codec` crate) must agree bit-exactly. If bumping `flac-codec` ever
//! breaks these tests, that is the tripwire firing — investigate upstream
//! before shipping.

use sendspin::audio::decode::{Decoder, FlacDecoder};

/// Split a FLAC file into (codec header, frame section).
///
/// The codec header (fLaC marker + metadata blocks) is what Sendspin servers
/// send base64-encoded in `stream/start`; the frame section is what arrives
/// as binary audio chunks.
fn split_flac(bytes: &[u8]) -> (&[u8], &[u8]) {
    assert_eq!(&bytes[..4], b"fLaC", "fixture is not a FLAC file");

    // Walk the metadata block chain: each block starts with a 1-byte
    // last-block flag + type, then a 24-bit big-endian length. Frames begin
    // immediately after the block with the last-flag set.
    let mut pos = 4;
    loop {
        let last = bytes[pos] & 0x80 != 0;
        let len = u32::from_be_bytes([0, bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]) as usize;
        pos += 4 + len;
        if last {
            break;
        }
    }

    bytes.split_at(pos)
}

/// Parse little-endian raw PCM at the given bit depth, scaled to full i32
/// range exactly as the playback pipeline expects (16-bit << 16, 24-bit << 8).
fn parse_expected(raw: &[u8], bits: u32) -> Vec<i32> {
    match bits {
        16 => raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| (i16::from_le_bytes([c[0], c[1]]) as i32) << 16)
            .collect(),
        24 => raw
            .as_chunks::<3>()
            .0
            .iter()
            .map(|c| {
                // Assemble the 24-bit value in the top three bytes and use an
                // arithmetic shift back down to sign-extend it, then widen to
                // full scale (net effect: one shift up by 8).
                let unscaled =
                    ((c[0] as i32) | ((c[1] as i32) << 8) | ((c[2] as i32) << 16)) << 8 >> 8;
                unscaled << 8
            })
            .collect(),
        other => panic!("unsupported fixture bit depth {other}"),
    }
}

fn run_conformance(name: &str, bits: u32) {
    let flac = std::fs::read(format!("tests/data/{name}.flac")).unwrap();
    let expected_raw = std::fs::read(format!("tests/data/{name}.expected.raw")).unwrap();

    let (header, frames) = split_flac(&flac);
    let expected = parse_expected(&expected_raw, bits);

    // Decode the whole frame section as one chunk, with header validation on.
    let decoder = FlacDecoder::with_header(header).unwrap();
    let decoded = decoder.decode(frames).unwrap();

    assert_eq!(
        decoded.len(),
        expected.len(),
        "{name}: decoded sample count mismatch"
    );
    assert_eq!(
        decoded.as_ref(),
        expected.as_slice(),
        "{name}: decoded samples differ from reference flac decoder output"
    );
}

#[test]
fn conformance_48k_16bit_stereo() {
    run_conformance("48k_16bit_stereo", 16);
}

#[test]
fn conformance_48k_24bit_stereo() {
    run_conformance("48k_24bit_stereo", 24);
}

#[test]
fn conformance_44k_16bit_stereo_small_blocks() {
    run_conformance("44k_16bit_stereo_b576", 16);
}

#[test]
fn conformance_96k_24bit_stereo() {
    run_conformance("96k_24bit_stereo", 24);
}

/// Frames split at arbitrary (non-frame-boundary-aware) points must fail
/// loudly rather than produce silent corruption; frames split at boundaries
/// (as the protocol delivers them) are covered by unit tests.
#[test]
fn mid_frame_split_is_an_error_not_corruption() {
    let flac = std::fs::read("tests/data/48k_16bit_stereo.flac").unwrap();
    let (header, frames) = split_flac(&flac);

    let decoder = FlacDecoder::with_header(header).unwrap();
    // Cut inside the first frame (a 4096-sample stereo frame is far larger
    // than 100 bytes).
    assert!(decoder.decode(&frames[..100]).is_err());
}
