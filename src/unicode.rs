//! Unicode classification used to decide how a terminal cell is drawn.
//!
//! The renderer has two glyph paths (see [`crate::shaping`]):
//!
//! * the original per-scalar [`fontdue`] path, which is exact, fast, and the
//!   only thing that ever touched ordinary ASCII, box drawing, and the rest of
//!   what 1.1.5 rendered, and
//! * a shaped path, which runs a text shaper over a run of cells so combining
//!   marks, joining scripts, reordering scripts, and color emoji come out
//!   right.
//!
//! Everything here answers one question: *does this cell need the shaper?* A
//! cell only gets the shaped path when the fast path cannot be correct, so a
//! screenshot of ordinary output renders byte for byte the way it always did.
//!
//! The tables are deliberately small and hand-maintained rather than pulled
//! from a full character-database crate: the renderer needs three coarse
//! properties (emoji presentation, variation selectors, "this script needs a
//! shaper"), not the whole UCD, and a table that ships in the binary is one
//! more thing that has to stay static-musl friendly.

use unicode_script::{Script, UnicodeScript};

/// Variation selector 15: force *text* presentation of the preceding
/// character.
pub(crate) const VS15: char = '\u{FE0E}';

/// Variation selector 16: force *emoji* presentation of the preceding
/// character.
pub(crate) const VS16: char = '\u{FE0F}';

/// Zero-width joiner, which glues emoji into a single picture (👨‍👩‍👧) that
/// vt100 nevertheless stores as several cells.
pub(crate) const ZWJ: char = '\u{200D}';

/// Combining enclosing keycap, the second half of `1️⃣`.
pub(crate) const KEYCAP: char = '\u{20E3}';

/// Emoji modifiers (skin tones), which vt100 stores in cells of their own.
pub(crate) const SKIN_TONES: std::ops::RangeInclusive<char> = '\u{1F3FB}'..='\u{1F3FF}';

/// Regional indicator symbols; a pair of them is one flag.
pub(crate) const REGIONAL_INDICATORS: std::ops::RangeInclusive<char> = '\u{1F1E6}'..='\u{1F1FF}';

/// Characters whose default presentation is a color emoji
/// (`Emoji_Presentation=Yes`), condensed to ranges.
///
/// Characters *outside* this list can still be emoji - `❤` is one - but they
/// default to text presentation and only become emoji when followed by
/// [`VS16`], which [`cluster_is_emoji`] handles separately.
const EMOJI_PRESENTATION: &[(u32, u32)] = &[
    (0x231A, 0x231B),
    (0x23E9, 0x23EC),
    (0x23F0, 0x23F0),
    (0x23F3, 0x23F3),
    (0x25FD, 0x25FE),
    (0x2614, 0x2615),
    (0x2648, 0x2653),
    (0x267F, 0x267F),
    (0x2693, 0x2693),
    (0x26A1, 0x26A1),
    (0x26AA, 0x26AB),
    (0x26BD, 0x26BE),
    (0x26C4, 0x26C5),
    (0x26CE, 0x26CE),
    (0x26D4, 0x26D4),
    (0x26EA, 0x26EA),
    (0x26F2, 0x26F3),
    (0x26F5, 0x26F5),
    (0x26FA, 0x26FA),
    (0x26FD, 0x26FD),
    (0x2705, 0x2705),
    (0x270A, 0x270B),
    (0x2728, 0x2728),
    (0x274C, 0x274C),
    (0x274E, 0x274E),
    (0x2753, 0x2755),
    (0x2757, 0x2757),
    (0x2795, 0x2797),
    (0x27B0, 0x27B0),
    (0x27BF, 0x27BF),
    (0x2B1B, 0x2B1C),
    (0x2B50, 0x2B50),
    (0x2B55, 0x2B55),
    (0x1F004, 0x1F004),
    (0x1F0CF, 0x1F0CF),
    (0x1F18E, 0x1F18E),
    (0x1F191, 0x1F19A),
    (0x1F1E6, 0x1F1FF),
    (0x1F201, 0x1F201),
    (0x1F21A, 0x1F21A),
    (0x1F22F, 0x1F22F),
    (0x1F232, 0x1F236),
    (0x1F238, 0x1F23A),
    (0x1F250, 0x1F251),
    (0x1F300, 0x1F320),
    (0x1F32D, 0x1F335),
    (0x1F337, 0x1F37C),
    (0x1F37E, 0x1F393),
    (0x1F3A0, 0x1F3CA),
    (0x1F3CF, 0x1F3D3),
    (0x1F3E0, 0x1F3F0),
    (0x1F3F4, 0x1F3F4),
    (0x1F3F8, 0x1F43E),
    (0x1F440, 0x1F440),
    (0x1F442, 0x1F4FC),
    (0x1F4FF, 0x1F53D),
    (0x1F54B, 0x1F54E),
    (0x1F550, 0x1F567),
    (0x1F57A, 0x1F57A),
    (0x1F595, 0x1F596),
    (0x1F5A4, 0x1F5A4),
    (0x1F5FB, 0x1F64F),
    (0x1F680, 0x1F6C5),
    (0x1F6CC, 0x1F6CC),
    (0x1F6D0, 0x1F6D2),
    (0x1F6D5, 0x1F6D7),
    (0x1F6DC, 0x1F6DF),
    (0x1F6EB, 0x1F6EC),
    (0x1F6F4, 0x1F6FC),
    (0x1F7E0, 0x1F7EB),
    (0x1F7F0, 0x1F7F0),
    (0x1F90C, 0x1F93A),
    (0x1F93C, 0x1F945),
    (0x1F947, 0x1F9FF),
    (0x1FA70, 0x1FA7C),
    (0x1FA80, 0x1FA89),
    (0x1FA8F, 0x1FAC6),
    (0x1FACE, 0x1FADC),
    (0x1FADF, 0x1FAE9),
    (0x1FAF0, 0x1FAF8),
];

/// Whether `ch` defaults to color emoji presentation.
pub(crate) fn has_emoji_presentation(ch: char) -> bool {
    let cp = ch as u32;
    EMOJI_PRESENTATION
        .binary_search_by(|&(lo, hi)| {
            if cp < lo {
                std::cmp::Ordering::Greater
            } else if cp > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// Whether a cell's contents should be drawn as color emoji.
///
/// [`VS15`] wins over everything (it is the explicit request for the text
/// form), then [`VS16`], then the character's own default presentation. The
/// Modifiers and keycaps count too because vt100 can split those sequences
/// across cells and each fragment still has to reach an emoji font. A ZWJ by
/// itself does not: non-emoji scripts such as Devanagari also use U+200D.
pub(crate) fn cluster_is_emoji(cluster: &str) -> bool {
    if cluster.contains(VS15) {
        return false;
    }
    cluster
        .chars()
        .any(|c| c == VS16 || c == KEYCAP || SKIN_TONES.contains(&c) || has_emoji_presentation(c))
}

/// Whether `script` cannot be drawn one character at a time.
///
/// These are the scripts where a glyph depends on its neighbours: Arabic-style
/// joining, Brahmic reordering and conjuncts, Southeast Asian mark stacking,
/// and Hebrew's point positioning. Latin, Greek, Cyrillic, Han, Kana and Hangul
/// are all absent on purpose - drawing them cell by cell is exactly right, and
/// keeping them off the shaped path is what keeps existing screenshots stable.
pub(crate) fn script_needs_shaping(script: Script) -> bool {
    matches!(
        script,
        // Arabic-style joining scripts.
        Script::Arabic
            | Script::Syriac
            | Script::Thaana
            | Script::Nko
            | Script::Mandaic
            | Script::Hanifi_Rohingya
            | Script::Adlam
            | Script::Mongolian
            | Script::Phags_Pa
            | Script::Manichaean
            | Script::Psalter_Pahlavi
            | Script::Sogdian
            | Script::Old_Uyghur
            | Script::Chorasmian
            // Hebrew: points and marks need positioning.
            | Script::Hebrew
            // Brahmic scripts: reordering, conjuncts, split vowels.
            | Script::Devanagari
            | Script::Bengali
            | Script::Gurmukhi
            | Script::Gujarati
            | Script::Oriya
            | Script::Tamil
            | Script::Telugu
            | Script::Kannada
            | Script::Malayalam
            | Script::Sinhala
            | Script::Tibetan
            | Script::Myanmar
            | Script::Khmer
            | Script::Thai
            | Script::Lao
            | Script::Javanese
            | Script::Balinese
            | Script::Sundanese
            | Script::Batak
            | Script::Buginese
            | Script::Cham
            | Script::Kayah_Li
            | Script::Lepcha
            | Script::Limbu
            | Script::Meetei_Mayek
            | Script::New_Tai_Lue
            | Script::Tai_Le
            | Script::Tai_Tham
            | Script::Tai_Viet
            | Script::Tagalog
            | Script::Hanunoo
            | Script::Buhid
            | Script::Tagbanwa
            | Script::Syloti_Nagri
            | Script::Saurashtra
            | Script::Rejang
            | Script::Kharoshthi
            | Script::Brahmi
            | Script::Chakma
            | Script::Sharada
            | Script::Takri
            | Script::Khojki
            | Script::Khudawadi
            | Script::Grantha
            | Script::Tirhuta
            | Script::Siddham
            | Script::Modi
            | Script::Ahom
            | Script::Multani
            | Script::Newa
            | Script::Bhaiksuki
            | Script::Marchen
            | Script::Masaram_Gondi
            | Script::Gunjala_Gondi
            | Script::Soyombo
            | Script::Zanabazar_Square
            | Script::Dogra
            | Script::Nandinagari
            | Script::Makasar
            | Script::Yezidi
            | Script::Dives_Akuru
            | Script::Tangsa
            | Script::Toto
            | Script::Vithkuqi
            | Script::Kawi
            | Script::Nag_Mundari
    )
}

/// Whether any character of `cluster` belongs to a script that needs a shaper.
pub(crate) fn cluster_needs_shaping_script(cluster: &str) -> bool {
    cluster.chars().any(|c| script_needs_shaping(c.script()))
}

/// Whether the shaped run containing `cluster` must stay contiguous.
///
/// Joining and reordering scripts have to be laid out as one piece: centering
/// each Arabic letter in its own cell would break every connecting stroke, and
/// a Devanagari vowel sign belongs wherever the shaper put it, not in the cell
/// its code point happened to land in. Everything else is projected cluster by
/// cluster so the monospace grid survives.
pub(crate) fn cluster_forces_contiguous_run(cluster: &str) -> bool {
    cluster_needs_shaping_script(cluster)
}

/// Whether `ch` is a variation selector (`VS1`-`VS16`).
pub(crate) fn is_variation_selector(ch: char) -> bool {
    ('\u{FE00}'..='\u{FE0F}').contains(&ch)
}

/// Whether the cell's contents are one of the fragments vt100 leaves behind
/// when it splits an emoji sequence: a lone skin tone modifier, a lone
/// regional indicator, or a piece ending in a joiner.
///
/// Only used for diagnostics and tests today; Phase 3 is where the terminal
/// core learns to keep these clusters together.
pub(crate) fn is_split_emoji_fragment(cluster: &str) -> bool {
    let mut chars = cluster.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if cluster.ends_with(ZWJ) {
        return true;
    }
    if chars.next().is_none() {
        return SKIN_TONES.contains(&first) || REGIONAL_INDICATORS.contains(&first);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emoji_presentation_table_is_sorted_and_disjoint() {
        let mut previous: Option<(u32, u32)> = None;
        for &(lo, hi) in EMOJI_PRESENTATION {
            assert!(lo <= hi, "range {lo:X}..{hi:X} is inverted");
            if let Some((_, prev_hi)) = previous {
                assert!(
                    prev_hi < lo,
                    "range starting at {lo:X} overlaps or follows {prev_hi:X} out of order"
                );
            }
            previous = Some((lo, hi));
        }
    }

    #[test]
    fn default_emoji_presentation_is_detected() {
        assert!(has_emoji_presentation('\u{1F600}'));
        assert!(has_emoji_presentation('\u{1F44D}'));
        assert!(has_emoji_presentation('\u{1F1FA}'));
        // Text-default characters that only become emoji with VS16.
        assert!(!has_emoji_presentation('\u{2764}'));
        assert!(!has_emoji_presentation('1'));
        assert!(!has_emoji_presentation('\u{250C}'));
    }

    #[test]
    fn variation_selectors_decide_presentation() {
        assert!(cluster_is_emoji("\u{2764}\u{FE0F}"));
        assert!(!cluster_is_emoji("\u{2764}\u{FE0E}"));
        assert!(!cluster_is_emoji("\u{2764}"));
        // A keycap is emoji because of its VS16, not because "1" is.
        assert!(cluster_is_emoji("1\u{FE0F}\u{20E3}"));
        assert!(!cluster_is_emoji("1"));
        // ZWJ is also used by ordinary scripts and is not sufficient to
        // select an emoji font on its own.
        assert!(!cluster_is_emoji("\u{0915}\u{094D}\u{200D}\u{0937}"));
        assert!(!cluster_is_emoji("\u{200D}"));
    }

    #[test]
    fn ascii_and_box_drawing_never_look_like_emoji() {
        for ch in "abcXYZ0123 =>!=<-|+*/".chars() {
            assert!(!cluster_is_emoji(&ch.to_string()), "{ch:?}");
        }
        for ch in "\u{250C}\u{2500}\u{2510}\u{2502}\u{2514}\u{2518}\u{2588}".chars() {
            assert!(!cluster_is_emoji(&ch.to_string()), "{ch:?}");
        }
    }

    #[test]
    fn scripts_that_need_a_shaper_are_recognized() {
        assert!(cluster_needs_shaping_script("\u{0645}")); // Arabic meem
        assert!(cluster_needs_shaping_script("\u{0915}")); // Devanagari ka
        assert!(cluster_needs_shaping_script("\u{0E01}")); // Thai ko kai
        // Scripts that are correct one cell at a time stay on the fast path.
        assert!(!cluster_needs_shaping_script("a"));
        assert!(!cluster_needs_shaping_script("\u{4F60}")); // CJK
        assert!(!cluster_needs_shaping_script("\u{0416}")); // Cyrillic
        assert!(!cluster_needs_shaping_script("\u{03B1}")); // Greek
        assert!(!cluster_needs_shaping_script("\u{250C}")); // box drawing
    }

    #[test]
    fn split_emoji_fragments_are_recognized() {
        assert!(is_split_emoji_fragment("\u{1F468}\u{200D}"));
        assert!(is_split_emoji_fragment("\u{1F3FD}"));
        assert!(is_split_emoji_fragment("\u{1F1FA}"));
        assert!(!is_split_emoji_fragment("\u{1F600}"));
        assert!(!is_split_emoji_fragment("a"));
    }
}
