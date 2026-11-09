//! What a transcript means before it reaches the agent (spec: voice-mode,
//! Transcription hallucination filter; Spoken and typed stop phrases).
//!
//! Speech-to-text models fed silence or noise invent text they saw often
//! in training ("thanks for watching", one word repeated). Such a
//! transcript counts as a recording with no speech. A whole utterance
//! equal to a stop phrase ends the voice conversation instead of being
//! sent; anything longer ("stop the docker container") is a request.

/// Known silence hallucinations, compared after [`normalize`].
const HALLUCINATIONS: &[&str] = &[
    "thank you for watching",
    "thanks for watching",
    "thank you so much for watching",
    "thank you very much for watching",
    "thanks for watching and see you next time",
    "please subscribe",
    "subscribe",
    "like and subscribe",
    "please like and subscribe",
    "don't forget to subscribe",
    "thank you",
    "you",
    "bye",
    "bye bye",
    "subtitles by the amara.org community",
    // French, Spanish, Portuguese, Italian, German, Dutch.
    "merci d'avoir regardé",
    "merci de votre attention",
    "sous-titres réalisés par la communauté d'amara.org",
    "gracias por ver",
    "gracias por ver el video",
    "suscríbete",
    "subtítulos realizados por la comunidad de amara.org",
    "obrigado por assistir",
    "inscreva-se",
    "legendas pela comunidade amara.org",
    "grazie per la visione",
    "iscriviti",
    "sottotitoli creati dalla comunità amara.org",
    "danke fürs zuschauen",
    "vielen dank fürs zuschauen",
    "untertitel im auftrag des zdf",
    "untertitelung des zdf",
    "abonniert",
    "bedankt voor het kijken",
    // Russian, Polish, Turkish.
    "спасибо за просмотр",
    "продолжение следует",
    "subskrybuj",
    "dziękuję za obejrzenie",
    "izlediğiniz için teşekkürler",
    // Japanese, Chinese, Korean.
    "ご視聴ありがとうございました",
    "チャンネル登録お願いします",
    "谢谢观看",
    "謝謝觀看",
    "请订阅",
    "시청해주셔서 감사합니다",
    "구독과 좋아요",
];

/// Lower-cased, surrounding punctuation and whitespace trimmed, inner
/// whitespace collapsed; typographic apostrophes become `'`.
#[must_use]
pub fn normalize(text: &str) -> String {
    let lowered = text.to_lowercase().replace(['\u{2019}', '\u{2018}'], "'");
    let words: Vec<&str> = lowered.split_whitespace().collect();
    let joined = words.join(" ");
    joined
        .trim_matches(|c: char| c.is_whitespace() || is_punctuation(c))
        .to_owned()
}

fn is_punctuation(c: char) -> bool {
    c.is_ascii_punctuation()
        || matches!(
            c,
            '。' | '、' | '！' | '？' | '¡' | '¿' | '…' | '«' | '»' | '“' | '”' | '„' | '・'
        )
}

/// Whether `transcript` is a silence hallucination: a known phantom
/// phrase, or one phrase repeated three or more times.
#[must_use]
pub fn is_hallucination(transcript: &str) -> bool {
    let text = normalize(transcript);
    if text.is_empty() {
        return true;
    }
    let bare: String = text.chars().filter(|c| !is_punctuation(*c)).collect();
    let bare = bare.split_whitespace().collect::<Vec<_>>().join(" ");
    HALLUCINATIONS
        .iter()
        .any(|known| *known == text || normalize_bare(known) == bare)
        || repeats(&bare)
}

fn normalize_bare(text: &str) -> String {
    text.chars().filter(|c| !is_punctuation(*c)).collect()
}

/// One phrase of one or more words said three or more times in a row,
/// and nothing else.
fn repeats(text: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    (1..=words.len() / 3).any(|size| {
        words.len().is_multiple_of(size) && words.chunks(size).all(|chunk| chunk == &words[..size])
    })
}

/// Whether the whole of `utterance` is one of `phrases` (case and
/// surrounding punctuation ignored).
#[must_use]
pub fn is_stop_phrase(utterance: &str, phrases: &[String]) -> bool {
    let said = normalize(utterance);
    !said.is_empty() && phrases.iter().any(|phrase| normalize(phrase) == said)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Vec<String> {
        pmt_config::schema::VoiceModeConfig::default().stop_phrases
    }

    #[test]
    fn phantom_outros_and_repetitions_are_hallucinations() {
        assert!(is_hallucination("Thank you for watching."));
        assert!(is_hallucination("  you  "));
        assert!(is_hallucination("Merci d’avoir regardé !"));
        assert!(is_hallucination("ご視聴ありがとうございました。"));
        assert!(is_hallucination("Okay. Okay. Okay."));
        assert!(is_hallucination("the end the end the end the end"));
        assert!(!is_hallucination(
            "Thank you for watching the build, now deploy it"
        ));
        assert!(!is_hallucination("okay okay"));
        assert!(!is_hallucination("what time is it"));
    }

    #[test]
    fn only_a_whole_utterance_is_a_stop_phrase() {
        assert!(is_stop_phrase("Stop.", &defaults()));
        assert!(is_stop_phrase("  That’s all!  ", &defaults()));
        assert!(is_stop_phrase("never   mind", &defaults()));
        assert!(!is_stop_phrase("stop the docker container", &defaults()));
        assert!(!is_stop_phrase("stop", &[]));
    }
}
