#!/usr/bin/env bash
# ABOUTME: Regenerates FLAC conformance fixtures for tests/flac_conformance.rs
# ABOUTME: Requires ffmpeg and the reference flac CLI (xiph.org); outputs are committed
#
# Each fixture is a pair:
#   <name>.flac         - encoded by the REFERENCE flac encoder
#   <name>.expected.raw - the same audio decoded back by the REFERENCE flac decoder
#
# The conformance test feeds <name>.flac's frames through sendspin's
# FlacDecoder (flac-codec crate) and requires bit-exact agreement with
# <name>.expected.raw. This is the tripwire that catches any decode
# regression in the pinned flac-codec dependency: correctness is anchored
# to the reference implementation, never to flac-codec itself.
#
# Signal content mixes tones with noise so frames exercise LPC, fixed
# predictors, and rice partitions rather than trivial constant subframes.

set -euo pipefail
cd "$(dirname "$0")"

command -v ffmpeg >/dev/null || { echo "ffmpeg not found" >&2; exit 1; }
command -v flac >/dev/null || { echo "flac (reference CLI) not found" >&2; exit 1; }

DUR=0.2

gen() {
    local name="$1" rate="$2" bits="$3" blocksize="$4" level="$5"
    local fmt="s${bits}le"

    # Deterministic source: two detuned sines per channel plus seeded noise.
    ffmpeg -y -v error \
        -f lavfi -i "sine=frequency=441:sample_rate=${rate}:duration=${DUR}" \
        -f lavfi -i "sine=frequency=1087:sample_rate=${rate}:duration=${DUR}" \
        -f lavfi -i "anoisesrc=colour=pink:seed=1234:sample_rate=${rate}:duration=${DUR}:amplitude=0.15" \
        -filter_complex "[0][1][2]amix=inputs=3:normalize=0,volume=0.5,aformat=channel_layouts=stereo" \
        -f "${fmt}" -acodec "pcm_${fmt}" "${name}.src.raw"

    flac -f -s "-${level}" -b "${blocksize}" \
        --force-raw-format --endian=little --sign=signed \
        --channels=2 --bps="${bits}" --sample-rate="${rate}" \
        -o "${name}.flac" "${name}.src.raw"

    # Reference decode: this is the ground truth the test compares against.
    flac -d -f -s \
        --force-raw-format --endian=little --sign=signed \
        -o "${name}.expected.raw" "${name}.flac"

    rm "${name}.src.raw"
    echo "generated ${name}.flac ($(wc -c < "${name}.flac") bytes)"
}

#    name                     rate  bits blocksize level
gen "48k_16bit_stereo"        48000 16   4096      8
gen "48k_24bit_stereo"        48000 24   4096      8
gen "44k_16bit_stereo_b576"   44100 16   576       5
gen "96k_24bit_stereo"        96000 24   4096      8
