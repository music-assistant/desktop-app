use cpal::{Sample, SampleFormat, SizedSample, U24};
use sendspin::audio::decode::{Decoder, PcmDecoder};

#[test]
fn u24_output_uses_unsigned_midpoint_for_silence() {
    assert_eq!(U24::FORMAT, SampleFormat::U24);
    assert_eq!(U24::from_sample(0.0_f32), U24::EQUILIBRIUM);
    assert_eq!(U24::from_sample(0.0_f32).inner(), 1 << 23);
}

#[test]
fn u24_output_preserves_decoded_24bit_samples() {
    let values: [i32; 13] = [
        -8_388_608, -8_388_607, -4_194_304, -65_537, -256, -1, 0, 1, 255, 65_537, 4_194_304,
        8_388_606, 8_388_607,
    ];
    let data: Vec<u8> = values
        .iter()
        .flat_map(|value| {
            let bytes = value.to_le_bytes();
            [bytes[0], bytes[1], bytes[2]]
        })
        .collect();
    let decoded = PcmDecoder::new(24).decode(&data).unwrap();
    assert_eq!(decoded.len(), values.len());

    for (&value, &sample) in values.iter().zip(decoded.iter()) {
        // Match the player's i32 -> f32 -> device sample conversion.
        let normalized = f32::from_sample(sample);
        let output = U24::from_sample(normalized);
        assert_eq!(
            output.inner(),
            value + (1 << 23),
            "incorrect unsigned output for signed 24-bit sample {value}"
        );
        assert_eq!(
            i32::from_sample(output),
            sample,
            "24-bit precision lost for signed sample {value}"
        );
    }
}
