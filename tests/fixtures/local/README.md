# Local scan audio-spec fixtures

Offline fixture batch for the local scan producer of the measured audio specs
(`src/local.rs`, `LocalMetadata.audio` → `AudioSpec`, MODEL_ALIGNMENT_DESIGN.md
§3.3). Audio fixtures are real encoded files; each one pins a branch of the
producer (lossy bitrate, lossless depth/rate, hi-res depth/rate, the MP4
lossless codec heuristic).

## Build provenance

Generated 2026-11-19 with ffmpeg 9.0.2 (`/usr/bin/ffmpeg`, pure sine source,
no network, no external assets). Exact commands, run in an empty scratch
directory:

```
ffmpeg -f lavfi -i "sine=frequency=1000:duration=1:sample_rate=44100" \
  -ac 1 -ar 44100 -b:a 128k -write_xing 0 spec_mp3_cbr128.mp3
ffmpeg -f lavfi -i "sine=frequency=1000:duration=1:sample_rate=44100" \
  -ac 1 -ar 44100 spec_flac_16_441.flac
ffmpeg -f lavfi -i "sine=frequency=1000:duration=1:sample_rate=96000" \
  -ac 1 -ar 96000 -sample_fmt s32 -bits_per_raw_sample 24 spec_flac_24_96.flac
ffmpeg -f lavfi -i "sine=frequency=1000:duration=1:sample_rate=44100" \
  -ac 1 -ar 44100 -c:a aac -b:a 128k spec_m4a_aac.m4a
```

`-sample_fmt s32 -bits_per_raw_sample 24` is required for the hi-res FLAC:
ffmpeg's default FLAC sample format is s16, and the file would then report
`bit_depth 16`.

## SHA-256

| Fixture | Bytes | SHA-256 |
|---|---|---|
| `spec_mp3_cbr128.mp3` | 16762 | `0d0edd60c8126aeac624f5f3600a259e3659d1a0e59c0f1b680467f8d6fc194a` |
| `spec_flac_16_441.flac` | 24579 | `076dcdd038f92fb7ecbd5bbbf5e0405b9d443426ba54bc4ba26983d8bd58609b` |
| `spec_flac_24_96.flac` | 42032 | `a4bc1d6322a68400c59e4972a14fb7f42e51fe1d186cad13e6a152b51d8e2e38` |
| `spec_m4a_aac.m4a` | 16222 | `48b3b637c896eded8717440732c04034688ac9dfb08385e8f570d6b62fd899d1` |

## Ground truth (lofty 0.22.4, default probe options, probed this session)

`AudioSpec.lossless` is the `src/local.rs` container rule applied to the
lofty `FileType`; the FLAC/AAC bitrates are computed by lofty from the file
itself (FLAC: stream length × 8 / duration; MP4: mdat size × timescale
ratio), so they are stable exactly because the files are committed.

| Fixture | lofty FileType | bitrate | sample_rate | bit_depth | AudioSpec | label |
|---|---|---|---|---|---|---|
| `spec_mp3_cbr128.mp3` | `Mpeg` | Some(128) | Some(44100) | None | `{128, 44100, None, false}` | `128K` |
| `spec_flac_16_441.flac` | `Flac` | Some(130) | Some(44100) | Some(16) | `{130, 44100, Some(16), true}` | `16/44.1` |
| `spec_flac_24_96.flac` | `Flac` | Some(269) | Some(96000) | Some(24) | `{269, 96000, Some(24), true}` | `24/96` |
| `spec_m4a_aac.m4a` | `Mp4` | Some(119) | Some(44100) | None | `{119, 44100, None, false}` | `119K` |

Cross-checked with ffprobe 9.0.2 (`format_name`/`bit_rate`, per-stream
`sample_rate`/`bits_per_raw_sample`); the MP4 AAC file carries no parsed bit
depth, which is the negative branch of the MP4 lossless heuristic.

## Exercise

| Fixture | Branch of the producer |
|---|---|
| `spec_mp3_cbr128.mp3` | lossy container (`Mpeg`): bitrate from the first frame header (CBR, `-write_xing 0` so no Xing/LAME size overrides it), `lossless false`, no bit depth |
| `spec_flac_16_441.flac` | lossless container: depth/rate from STREAMINFO, label `16/44.1`, lossless bitrate (130) kept in the slot but unused by the label |
| `spec_flac_24_96.flac` | hi-res branch (depth > 16): `24/96` |
| `spec_m4a_aac.m4a` | MP4 container with a lossy codec: no parsed depth, so `lossless false` (the heuristic's negative branch) |

The MP4 heuristic's positive branch (ALAC/FLAC in MP4 with a parsed depth)
and the zero-field filter are pinned as pure unit tests over
`audio_spec_from_parts`/`is_lossless` in `src/local.rs`: synthesizing such
containers needs a real ALAC mux, and the upstream audit pins the same split
(unit tests over `is_lossless` in voicefox `metadata.rs:255-268`, file-based
coverage elsewhere). The scan-level integration test reuses
`spec_mp3_cbr128.mp3` plus a runtime-synthesized PCM WAV stub (44100 Hz mono
16-bit, 1 s → `{706, 44100, Some(16), true}`; the same synthesis pattern the
CUE fixtures use) so `LocalLibraryProvider::scan` is proven to carry the spec
per file without committing an 88 KB WAV.
