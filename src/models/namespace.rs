//! Source namespace mapping (MODEL_ALIGNMENT_DESIGN.md §3.2): the single
//! pinned table between voicefox `SourceId` short names and musid-dli
//! provider id strings. The names differ (`wy` vs `netease`, `tx` vs
//! `qqmusic`, `kg` vs `kugou`, `apple` vs `applemusic`), and every
//! host-side `SongInfo`/`Track` adapter must go through here instead of
//! inventing its own translation.

/// voicefox `SourceId::as_str()` name to musid-dli provider id.
/// Anchors: `voicefox core/src/model/source.rs:46-62` and the `const ID`
/// values of each musid-dli source module (`source_netease.rs:28`,
/// `source_qqmusic.rs:35`, `source_kugou.rs:33`, `source_kuwo.rs:43`,
/// `source_migu.rs:42`, `source_bilibili.rs:51`, `source_jamendo.rs:24`,
/// `source_apple.rs:31`, `source_fivesing.rs:26`, `source_qianqian.rs:56`,
/// `source_joox.rs`).
///
/// The voicefox-only source `soda` has no musid-dli provider and `local`
/// follows LOCAL_LIBRARY_PARITY.md, so they are absent: the lookup returns
/// `None` for them. musid-dli-only providers (`youtube-music`, `lx-js`) have
/// no voicefox source and are absent from the forward direction.
const VOICEFOX_TO_PROVIDER: &[(&str, &str)] = &[
    ("wy", "netease"),
    ("tx", "qqmusic"),
    ("kg", "kugou"),
    ("kw", "kuwo"),
    ("mg", "migu"),
    ("bili", "bilibili"),
    ("jamendo", "jamendo"),
    ("apple", "applemusic"),
    ("fivesing", "fivesing"),
    ("qianqian", "qianqian"),
    // joox migrated (2026-11-19): identity name on both sides
    // (voicefox core/src/model/source.rs, `SourceId::Joox` = "joox").
    ("joox", "joox"),
];

/// voicefox `SourceId` short name to musid-dli provider id. Unknown strings
/// (including already-musid-dli names like `"netease"`, and voicefox sources
/// without a musid-dli provider) return `None`.
pub fn voicefox_source_to_provider(source: &str) -> Option<&'static str> {
    VOICEFOX_TO_PROVIDER
        .iter()
        .find(|(voicefox, _)| *voicefox == source)
        .map(|(_, provider)| *provider)
}

/// musid-dli provider id to the voicefox `SourceId` short name. Unknown
/// strings (including voicefox-only names like `"wy"`, and musid-dli-only
/// providers `youtube-music`/`lx-js`) return `None`.
pub fn provider_to_voicefox_source(provider: &str) -> Option<&'static str> {
    VOICEFOX_TO_PROVIDER
        .iter()
        .find(|(_, musid)| *musid == provider)
        .map(|(voicefox, _)| *voicefox)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T6 (§7): the whole table pinned in both directions; the identity
    /// pairs and the renames are both asserted explicitly so a typo cannot
    /// survive.
    #[test]
    fn namespace_table_is_pinned_both_ways() {
        let expected: &[(&str, &str)] = &[
            ("wy", "netease"),
            ("tx", "qqmusic"),
            ("kg", "kugou"),
            ("kw", "kuwo"),
            ("mg", "migu"),
            ("bili", "bilibili"),
            ("jamendo", "jamendo"),
            ("apple", "applemusic"),
            ("fivesing", "fivesing"),
            // qianqian migrated (2026-11-19): identity name on both sides
            // (voicefox core/src/model/source.rs:55).
            ("qianqian", "qianqian"),
            // joox migrated (2026-11-19, slice source-joox): identity name on
            // both sides (voicefox core/src/model/source.rs:57).
            ("joox", "joox"),
        ];
        assert_eq!(VOICEFOX_TO_PROVIDER, expected);

        for (voicefox, provider) in expected {
            assert_eq!(voicefox_source_to_provider(voicefox), Some(*provider));
            assert_eq!(provider_to_voicefox_source(provider), Some(*voicefox));
        }
    }

    /// T6 (§7): unknown strings are rejected on both sides, including
    /// same-language cross lookups (a musid-dli name is not a voicefox
    /// source, a voicefox name is not a musid-dli provider), the
    /// voicefox-only sources, and the musid-dli-only providers.
    #[test]
    fn unknown_strings_and_gap_sources_are_rejected() {
        // Cross-language lookups must not silently succeed.
        assert_eq!(voicefox_source_to_provider("netease"), None);
        assert_eq!(provider_to_voicefox_source("wy"), None);
        // voicefox-only sources (coverage gap; local goes through
        // LOCAL_LIBRARY_PARITY.md). qianqian left this list when the
        // provider migrated (2026-11-19), joox when source-joox landed
        // (2026-11-19); soda stays voicefox-only.
        assert_eq!(voicefox_source_to_provider("soda"), None);
        assert_eq!(voicefox_source_to_provider("local"), None);
        // musid-dli-only providers (reverse gap).
        assert_eq!(provider_to_voicefox_source("youtube-music"), None);
        assert_eq!(provider_to_voicefox_source("lx-js"), None);
        // Arbitrary junk.
        assert_eq!(voicefox_source_to_provider(""), None);
        assert_eq!(provider_to_voicefox_source("nope"), None);
    }
}
