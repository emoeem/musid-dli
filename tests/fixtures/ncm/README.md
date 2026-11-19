# NCM container fixtures

Synthetic vectors built by the independent audit builder (pure Python +
`openssl enc -aes-128-ecb` for AES, zero code from the audited crate), spec
source: ncm2mp3-core 0.3.2 `docs/ARCHITECTURE.md` §2/§3. Audit date
2026-11-18. Copied verbatim from `/tmp/audit-ncm2mp3/vectors/`.

## `golden.ncm`: full-container positive vector

3319 bytes, SHA-256
`427e45d52ae7d649e8b15b5a334373e9789962fe725e25d2c9ac9679373b62cb`
(pre-3.x layout: unpadded cover frame).

Build parameters:

* RC4 key material: ASCII `audit-golden-key-01`
* audio plaintext: `ID3\x04\x00\x00\x00\x00\x00\x0A` + 3000 bytes of
  `(i * 11 + 3) & 0xFF`, 3010 bytes total
* metadata JSON: `{musicName:"Audit Golden Vector", artist:[["Auditor",7]],
  album:"ncm-handler audit", format:"mp3", bitrate:128000, duration:6000}`
* cover: 16 bytes `FF D8 FF E0 00 10` + ASCII `JFIF-audit` (JPEG head)

Expected handler output (cross-checked against the published
`ncm2mp3-core 0.3.2` decoder; audio SHA-256
`6c2cd8d7cea64435930fd9d5f518b7a1b55a2a0dbd671ce1c5dbf5a3e7e25557`):

* title `Audit Golden Vector`, artists `["Auditor"]`,
  album `ncm-handler audit`
* duration 6000 ms, audio format sniffed `Mp3` (ID3 head)
* artwork = the 16 cover bytes above, MIME `image/jpeg`
* extracted audio = the 3010 plaintext bytes above, byte-exact

## `panic_key.ncm`: adversarial empty-key negative vector

127 bytes, SHA-256
`14bb2a01179487042bfb9b3bc5679d009d94d42a783f6fddb79aa9c5fff6e9f7`.

The RC4 key blob decrypts to exactly `neteasecloudmusic`, i.e. zero key
material. `ncm2mp3-core 0.3.2` panics on it (`crypto/rc4.rs:26` "NCM stream
key must be non-empty", reproduced locally via the published crate). The
handler must reject this as an error (`Error::InvalidInput`) before
constructing the crate decoder: a panic on hostile/corrupt input is a bug,
not an outcome.

## Regeneration

`/tmp/audit-ncm2mp3/vectors/build_golden.py` (audit session scratch space)
rebuilds `golden.ncm`; its independent decode self-check re-derives every
value above without touching the audited crate.
