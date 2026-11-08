//! 認識エンジンの共通の形(`Recognizer`)と、エンジンに渡す手がかり・結果の後始末。
//! エンジンは端末内で動く(Haas-ja を speech.cpp で、macOS 26 では Mac 内蔵の音声認識も)。音声は外部へ送らない。

use super::dictionary::{template_key, Dictionary, TEMPLATE_WORD};
use super::segmenter::SAMPLE_RATE;
use super::voice_command::{CommandAction, CommandWords};
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

/// 無音に近い音で認識エンジンが作り出しがちな決まり文句。文全体が一致したら捨てる。
/// 実際に話しうる語(「ありがとうございました」など)は消さないよう入れない。
const HALLUCINATIONS: &[&str] = &[
    "ご視聴ありがとうございました",
    "ご視聴ありがとうございました。",
    "ご視聴ありがとうございます",
    "ご視聴ありがとうございます。",
    "字幕は視聴者によって作成されました",
    "チャンネル登録お願いします",
    "次回予告",
    "おやすみなさい",
];

/// 雑音に対して認識エンジンが出しがちな、同じ語の繰り返し(「肺炎、肺炎、肺炎、肺炎」)。
/// 句読点と空白を除いた文全体が 2〜6 字の繰り返し 3 回以上、または同じ 2〜6 字が 4 回以上続いたら捨てる。
/// 1 字の繰り返し(「ははは」)は笑い声などでありうるので、5 回以上(全体)・6 回以上(途中)に限る。
pub fn looks_like_repetition(text: &str) -> bool {
    let chars: Vec<char> = text
        .chars()
        .filter(|c| {
            !c.is_whitespace()
                && !matches!(
                    c,
                    '、' | '。' | ',' | '.' | '，' | '．' | '・' | '！' | '!' | '？' | '?'
                )
        })
        .collect();
    let n = chars.len();
    for width in 1..=6 {
        let (whole, run_limit) = if width == 1 { (5, 6) } else { (3, 4) };
        if n >= width * whole
            && n.is_multiple_of(width)
            && chars.chunks(width).all(|c| c == &chars[..width])
        {
            return true;
        }
        if n < width * run_limit {
            continue;
        }
        // 繰り返しの前に別の語が付いていても見つかるよう、区切りの位置をずらして見る。
        for phase in 0..width {
            let mut run = 1;
            let mut start = phase + width;
            while start + width <= n {
                if chars[start..start + width] == chars[start - width..start] {
                    run += 1;
                    if run >= run_limit {
                        return true;
                    }
                } else {
                    run = 1;
                }
                start += width;
            }
        }
    }
    false
}

/// 分野の語(登録単語と同じ扱いで、認識エンジンの keywords に渡す)。AI への補足の雛形にも載せている。
pub(crate) const FIELD_WORDS: &[&str] = &[
    "胸部CT",
    "腹部MRI",
    "頭部MRI",
    "造影",
    "単純",
    "拡散強調像",
    "肺野",
    "縦隔",
    "胸水",
    "結節影",
    "腫瘤",
    "リンパ節腫大",
    "肝嚢胞",
    "腎結石",
    "脳梗塞",
    "白質病変",
];

#[derive(Debug, Clone, PartialEq)]
pub struct RecognizeOptions {
    /// 用途の語と登録単語。Haas は keywords として使う。
    pub words: Vec<String>,
    /// 登録単語の読み(表記 → ひらがな)。Haas の keywords に「表記=読み」で渡す。読みの無い語は入らない。
    pub readings: HashMap<String, String>,
}

/// 分野の語・音声コマンドの名前と言葉・登録単語・テンプレートのキーワードを、重複を除いて並べる
/// (Haas の keywords 用。使えない語は `speechcpp::keyword_lines` が 1 語ずつ外す)。
/// 件数の上限で切られるので、数の少ないコマンドの言葉を登録単語より前に置く。
/// コマンドの名前は漢字(「改行」)だけ渡す(読みは `RecognizeOptions.readings` で添える。
/// 同じ音の語を 2 つ渡すとエンジンがかなで書くことがある)。名前の読みと同じ登録語も渡さない。
/// Claude を呼ぶ言葉(`call_words`)は、ならして同じになる表記(「ヘイ、クロード」「へいくろーど」)を 1 つにして渡す。
pub fn build_words(
    dictionary: &Dictionary,
    commands: &[CommandWords],
    call_words: &[String],
) -> Vec<String> {
    let is_name = |word: &str| {
        CommandAction::ALL
            .iter()
            .any(|action| action.names().contains(&word))
    };
    let mut call_keys: Vec<String> = Vec::new();
    let calls: Vec<String> = call_words
        .iter()
        .map(|w| w.trim().to_string())
        .filter(|w| {
            let key = template_key(w);
            !key.is_empty() && !call_keys.contains(&key) && {
                call_keys.push(key);
                true
            }
        })
        .collect();
    let mut words: Vec<String> = Vec::new();
    for word in FIELD_WORDS
        .iter()
        .copied()
        .chain(CommandAction::ALL.iter().map(|action| action.label()))
        .map(|w| w.to_string())
        .chain(calls)
        .chain(
            commands
                .iter()
                .flat_map(|c| c.words.iter().map(|w| w.trim().to_string()))
                .filter(|w| !is_name(w)),
        )
        .chain(dictionary.words.iter().map(|w| w.word.trim().to_string()))
        .chain(dictionary.template_keywords().map(|k| k.trim().to_string()))
        // テンプレートがあれば、呼ぶときの「テンプレート」も手がかりにする。
        .chain((!dictionary.templates.is_empty()).then(|| TEMPLATE_WORD.to_string()))
    {
        if !word.is_empty() && !words.contains(&word) {
            words.push(word);
        }
    }
    words
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Recognition {
    pub text: String,
    /// あやしい語の範囲(文字位置)。
    pub unsure: Vec<(usize, usize)>,
}

pub trait Recognizer: Send {
    fn name(&self) -> String;
    /// 使う前の準備(モデルの読み込みなど)。省略可。
    fn prepare(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn recognize(
        &mut self,
        samples: &[f32],
        options: &RecognizeOptions,
    ) -> Result<Recognition, String>;
    fn recognize_file(
        &mut self,
        path: &Path,
        options: &RecognizeOptions,
    ) -> Result<Recognition, String>;
    /// 話している途中の音を認識して途中経過を出せるか。速い認識エンジンだけが true。
    fn supports_partial(&self) -> bool {
        false
    }
    /// 話している途中の音を認識する(途中経過の表示用)。あやしい語の印は付けない。
    fn recognize_partial(
        &mut self,
        samples: &[f32],
        options: &RecognizeOptions,
    ) -> Result<String, String> {
        self.recognize(samples, options).map(|r| r.text)
    }
}

fn wav_spec() -> hound::WavSpec {
    hound::WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE as u32,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    }
}

fn write_samples<W: Write + std::io::Seek>(
    mut writer: hound::WavWriter<W>,
    samples: &[f32],
) -> Result<(), String> {
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        writer
            .write_sample(value)
            .map_err(|e| format!("音声を保存できません: {e}"))?;
    }
    writer
        .finalize()
        .map_err(|e| format!("音声を保存できません: {e}"))
}

/// 16 kHz・mono・16 bit の WAV のバイト列(外部の音声認識 API に送る)。
pub fn wav_bytes(samples: &[f32]) -> Result<Vec<u8>, String> {
    let mut cursor = std::io::Cursor::new(Vec::new());
    let writer = hound::WavWriter::new(&mut cursor, wav_spec())
        .map_err(|e| format!("音声を保存できません: {e}"))?;
    write_samples(writer, samples)?;
    Ok(cursor.into_inner())
}

pub fn write_wav(path: &Path, samples: &[f32]) -> Result<(), String> {
    let writer = hound::WavWriter::create(path, wav_spec())
        .map_err(|e| format!("音声を保存できません: {e}"))?;
    write_samples(writer, samples)
}

/// 雑音に対して出がちな決まり文句や同じ語の繰り返しなら、何も聞こえなかったことにする。
pub(crate) fn drop_hallucination(recognition: Recognition) -> Recognition {
    let text = recognition.text.trim().trim_end_matches('。');
    if HALLUCINATIONS.contains(&text) || looks_like_repetition(&recognition.text) {
        return Recognition::default();
    }
    recognition
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_words_come_first_and_registered_words_last() {
        let mut dictionary = Dictionary::default();
        dictionary.add_word("肺気腫", "はいきしゅ").unwrap();
        let words = build_words(&dictionary, &[], &[]);
        assert_eq!(words.first().map(String::as_str), Some("胸部CT"));
        assert_eq!(words.last().map(String::as_str), Some("肺気腫"));
    }

    #[test]
    fn command_words_come_before_registered_words() {
        use crate::core::voice_command::CommandAction;
        let mut dictionary = Dictionary::default();
        dictionary.add_word("肺気腫", "はいきしゅ").unwrap();
        let commands = [CommandWords {
            action: CommandAction::NewLine,
            words: vec!["次の行".to_string(), "肺気腫".to_string()],
        }];
        let calls = ["ヘイクロード", "ヘイ、クロード", "へいくろーど", " "].map(str::to_string);
        let words = build_words(&dictionary, &commands, &calls);
        let at = |word: &str| words.iter().position(|w| w == word).unwrap();
        assert!(at("改行") < at("ヘイクロード"));
        assert!(at("ヘイクロード") < at("次の行"));
        assert!(at("次の行") < at("肺気腫"));
        assert_eq!(words.iter().filter(|w| *w == "肺気腫").count(), 1);
        // ならして同じになる呼びかけ語は 1 つだけ渡す(同じ音の語を 2 つ渡すとかなで書くことがある)。
        assert_eq!(words.iter().filter(|w| w.contains("クロード") || w.contains("くろーど")).count(), 1);
    }

    #[test]
    fn repeated_words_are_treated_as_hallucination() {
        assert!(looks_like_repetition(
            "肺炎、肺炎、肺炎、肺炎、肺炎、肺炎。"
        ));
        assert!(looks_like_repetition("肝嚼、肝嚼、肝嚼、肝嚼"));
        assert!(looks_like_repetition("胸部長、肝嚼、肝嚼、肝嚼、肝嚼。"));
        assert!(looks_like_repetition("あああああああ"));
        assert!(!looks_like_repetition("右肺上葉に5mmの結節を認めます"));
        assert!(!looks_like_repetition("異常なし。異常なし。"));
        assert!(!looks_like_repetition("ははは"));
        for phrase in ["次回予告", "次回予告。", " おやすみなさい。 ", "ご視聴ありがとうございました。"] {
            let recognition = Recognition {
                text: phrase.to_string(),
                unsure: Vec::new(),
            };
            assert_eq!(drop_hallucination(recognition), Recognition::default(), "{phrase}");
        }
        // 雑音から出た同じ語の繰り返しも、何も聞こえなかったことにする。
        let repeated = Recognition {
            text: "肺炎、肺炎、肺炎、肺炎。".to_string(),
            unsure: Vec::new(),
        };
        assert_eq!(drop_hallucination(repeated), Recognition::default());
        let spoken = Recognition {
            text: "右肺上葉に5mmの結節を認めます".to_string(),
            unsure: Vec::new(),
        };
        assert_eq!(drop_hallucination(spoken.clone()), spoken);
    }
}
