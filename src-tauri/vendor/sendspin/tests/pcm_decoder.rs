use cpal::Sample;
use sendspin::audio::decode::{Decoder, PcmDecoder};

#[test]
fn test_decode_pcm_16bit() {
    let decoder = PcmDecoder::new(16);

    // 4 samples (8 bytes) of 16-bit PCM
    let data = vec![
        0x00, 0x04, // 1024 in little-endian
        0x00, 0x08, // 2048
        0xFF, 0xFF, // -1
        0x00, 0x00, // 0
    ];

    let samples = decoder.decode(&data).unwrap();

    assert_eq!(samples.len(), 4);
    assert_eq!(samples[0].to_sample::<i16>(), 1024);
    assert_eq!(samples[1].to_sample::<i16>(), 2048);
    assert_eq!(samples[2].to_sample::<i16>(), -1);
    assert_eq!(samples[3].to_sample::<i16>(), 0);
}

#[test]
fn test_decode_pcm_24bit() {
    let decoder = PcmDecoder::new(24);

    // 2 samples (6 bytes) of 24-bit PCM
    let data = vec![
        0x00, 0x10, 0x00, // 4096 in little-endian 24-bit
        0xFF, 0xFF, 0xFF, // -1 in 24-bit
    ];

    let samples = decoder.decode(&data).unwrap();

    assert_eq!(samples.len(), 2);
    // Values are scaled to full i32 range (left-shifted 8 bits from 24-bit).
    assert_eq!(samples[0], 4096 << 8);
    assert_eq!(samples[1], -1_i32 << 8);
}

#[test]
fn test_decode_pcm_16bit_empty_input() {
    let decoder = PcmDecoder::new(16);
    let result = decoder.decode(&[]);
    assert!(result.is_err(), "empty input should be rejected");
}

#[test]
fn test_decode_pcm_24bit_empty_input() {
    let decoder = PcmDecoder::new(24);
    let result = decoder.decode(&[]);
    assert!(result.is_err(), "empty input should be rejected");
}

#[test]
fn test_decode_pcm_16bit_misaligned_trailing_byte_rejected() {
    let decoder = PcmDecoder::new(16);
    // 3 bytes: not a multiple of 2
    let result = decoder.decode(&[0x00, 0x04, 0xFF]);
    assert!(result.is_err(), "truncated 16-bit PCM should be rejected");
}

#[test]
fn test_decode_pcm_24bit_misaligned_trailing_bytes_rejected() {
    let decoder = PcmDecoder::new(24);
    // 5 bytes: not a multiple of 3
    let result = decoder.decode(&[0x00, 0x10, 0x00, 0xAB, 0xCD]);
    assert!(result.is_err(), "truncated 24-bit PCM should be rejected");
}

#[test]
fn test_decode_pcm_16bit_single_sample_max() {
    let decoder = PcmDecoder::new(16);
    let data = vec![0xFF, 0x7F]; // i16::MAX = 32767
    let samples = decoder.decode(&data).unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].to_sample::<i16>(), i16::MAX);
}

#[test]
fn test_decode_pcm_16bit_single_sample_min() {
    let decoder = PcmDecoder::new(16);
    let data = vec![0x00, 0x80]; // i16::MIN = -32768 in LE
    let samples = decoder.decode(&data).unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].to_sample::<i16>(), i16::MIN);
}

#[test]
fn test_decode_pcm_24bit_single_sample_max() {
    let decoder = PcmDecoder::new(24);
    let data = vec![0xFF, 0xFF, 0x7F];
    let samples = decoder.decode(&data).unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0], 8_388_607_i32 << 8);
}

#[test]
fn test_decode_pcm_24bit_single_sample_min() {
    let decoder = PcmDecoder::new(24);
    let data = vec![0x00, 0x00, 0x80]; // -8388608 in 24-bit LE
    let samples = decoder.decode(&data).unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0], i32::MIN);
}

#[test]
fn test_decode_pcm_zero_bit_depth_unsupported() {
    let decoder = PcmDecoder::new(0);
    let result = decoder.decode(&[0x00]);
    assert!(result.is_err());
}

#[test]
fn test_decode_pcm_unsupported_bit_depth() {
    let decoder = PcmDecoder::new(8);
    let result = decoder.decode(&[0x00, 0x01]);
    assert!(result.is_err());
}

#[test]
fn test_decode_pcm_32bit_unsupported() {
    let decoder = PcmDecoder::new(32);
    let result = decoder.decode(&[0x00; 4]);
    assert!(result.is_err());
}

#[test]
fn test_decode_pcm_16bit_sub_sample_input() {
    let decoder = PcmDecoder::new(16);
    // Single byte — not enough for one 16-bit sample
    let result = decoder.decode(&[0xFF]);
    assert!(
        result.is_err(),
        "sub-sample 16-bit input should be rejected"
    );
}

#[test]
fn test_decode_pcm_24bit_sub_sample_input() {
    let decoder = PcmDecoder::new(24);
    // Two bytes — not enough for one 24-bit sample
    let result = decoder.decode(&[0xFF, 0xFF]);
    assert!(
        result.is_err(),
        "sub-sample 24-bit input should be rejected"
    );
}
