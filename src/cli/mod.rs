//! 診断と画面確認の引数。引数ごとの処理は子モジュール、使い方と知っている引数の一覧は `MODES` 1 つから作る。

use openspeech::app::VoiceApp;
use openspeech::core::annotate;
use openspeech::core::audio;
use openspeech::core::caps_lock;
use openspeech::core::assistant::{
    self, Assistant, AssistantRequest, EditCheck, ReplyKind, Target, MAX_REQUEST_GUIDANCE_CHARS, MAX_TARGET_CHARS,
};
use openspeech::core::completion;
use openspeech::core::guidance;
use openspeech::core::corrector::CorrectionRequest;
use openspeech::core::judge::changed_ranges;
use openspeech::core::mic_access;
use openspeech::core::recognizer::Recognition;
use openspeech::core::segmenter::VoiceJudge;
use openspeech::core::remote_asr;
use openspeech::core::settings::{CorrectorBackend, GlobalSettings, RecognizerKind, TransferMethod};
use openspeech::core::speechcpp::{self, VoiceDetector};
use openspeech::core::dictionary::Template;
use openspeech::core::store::Store;
use openspeech::core::structure::{self, LineKind, StructureFile, TemplateStructure, Vocabulary};
use openspeech::core::text_filter::apply_prefilters;
use openspeech::core::voice_command::{match_after_correction, match_utterance, Match};
use openspeech::core::transfer::{self, TransferRequest};
use std::time::{Duration, Instant};

mod ai;
mod preview;
mod speech;
mod system;

pub(crate) use ai::*;
pub(crate) use preview::*;
pub(crate) use speech::*;
pub(crate) use system::*;

pub(crate) fn argument_value<'a>(args: &'a [String], name: &str) -> Option<&'a String> {
    let index = args.iter().position(|a| a == name)?;
    args.get(index + 1)
}

/// 引数の使い方 1 行(か 2 行)と、その行が使う引数。`--help` の表示と、知っている引数の一覧はここから作る。
/// `--say` は並び順に意味がある(`apply_preview_arguments`)ので、名前だけを持つ。
pub(crate) struct Mode {
    pub usage: &'static str,
    pub flags: &'static [&'static str],
}

pub(crate) const MODES: &[Mode] = &[
    Mode { usage: "  (引数なし)                      画面を開く", flags: &[] },
    Mode {
        usage: "  --install-app [フォルダ]        アプリとして組み立てて入れる(既定は ~/Applications)",
        flags: &["--install-app"],
    },
    Mode { usage: "  --add-to-dock                   Dock に加える", flags: &["--add-to-dock"] },
    Mode { usage: "  --list-devices                  マイクの一覧", flags: &["--list-devices"] },
    Mode {
        usage: "  --mic-check 秒 [--device 名|--iphone] [--out wav]   マイクの音量を測る",
        flags: &["--mic-check", "--device", "--iphone", "--out"],
    },
    Mode {
        usage: "  --release-check [--device 名]     名前で開いて 1 秒録って閉じ、デバイスが止まったかを読む(動いたままなら 2)",
        flags: &["--release-check", "--device"],
    },
    Mode {
        usage: "  --latency-check 秒 [--device 名|--iphone] [--save DIR] [--ratio R] [--no-vad]   切り出しと認識の遅れを測る",
        flags: &["--latency-check", "--device", "--iphone", "--save", "--ratio", "--no-vad"],
    },
    Mode {
        usage: "  --segment-check wav [--ratio R] [--start-ms N] [--no-vad] [--trace] [--vad-threshold T]\n                                                                   切り出しを録音で試す(--no-vad で音量だけ、--trace で 20 ms ごとの内訳)",
        flags: &["--segment-check", "--ratio", "--start-ms", "--no-vad", "--trace", "--vad-threshold"],
    },
    Mode {
        usage: "  --recognize-check 音声.wav... [--backend remote]     切り出し済みの音を認識して印字する(remote: 設定の外部 API にも送り、アプリと同じ順に整えて門番を当て、Haas の文と並べる)",
        flags: &["--recognize-check", "--backend"],
    },
    Mode {
        usage: "  --correct-check 文 [--context 文] [--backend api|claude-code]   AI 訂正を試す",
        flags: &["--correct-check", "--context", "--backend"],
    },
    Mode {
        usage: "  --complete-check 依頼 [--previous 文] [--target 文|--target-file 場所 [--whole-text]] [--backend api|claude-code] [--compare]\n                                                                   「ヘイ、クロード」の依頼(テンプレートの補完・文章の手直し)を試す(--compare で構造なしの全文経路と並べる)",
        flags: &["--complete-check", "--previous", "--target", "--target-file", "--whole-text", "--backend", "--compare"],
    },
    Mode {
        usage: "  --annotate-templates [--model モデル] [--only 名前] [--backend api|claude-code]\n                                                                   テンプレートに構造を付けて structure.json に書く(既定は Sonnet。辞書は書き換えない)",
        flags: &["--annotate-templates", "--model", "--only", "--backend"],
    },
    Mode {
        usage: "  --caps-check 秒 [--set-on|--set-off]                Caps Lock と画面のロックの変化を見る",
        flags: &["--caps-check", "--set-on", "--set-off"],
    },
    Mode {
        usage: "  --transfer-check 文 [--method ax|paste|type] [--wait 秒]   前面のアプリへ転送を試す(前面に文字が入る)",
        flags: &["--transfer-check", "--method", "--wait"],
    },
    Mode {
        usage: "  --screenshot 画像 [--open 画面名] [--say 文]...   画面を撮って終わる",
        flags: &["--screenshot", "--open", "--say"],
    },
    Mode { usage: "  --version / --help", flags: &["--version", "--help"] },
];

/// `--help` に出す使い方。
pub(crate) fn usage() -> String {
    let mut text = String::from("使い方: openspeech [引数]");
    for mode in MODES {
        text.push('\n');
        text.push_str(mode.usage);
    }
    text
}

/// 知っている引数か。知らない `--` 引数で画面を開いてしまわないよう、`main` は振り分けの前にこれで調べる。
pub(crate) fn is_known_flag(flag: &str) -> bool {
    MODES.iter().any(|mode| mode.flags.contains(&flag))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 引数の名前は運用の手順に残っているので、集合を変えない(計画書 §6)。1.0 より前の版の 37 個から、照合の経路を
    /// 外したとき(P5)に `--gemini-check`・`--together`・`--model`・`--heard`・`--checking` の 5 個を外した。
    /// 1.1.0 の後(post-release §7)に、手直しの依頼を試す `--target`・`--target-file`・`--whole-text` を足した。
    /// 単位 S2(§16.8)で、テンプレートの注釈 `--annotate-templates`・`--model`・`--only` を足した(`--model` は
    /// P5 で外した照合の物とは別。注釈役のモデル)。単位 S3 で、補完の構造の経路と全文経路を並べる `--compare` を足した。
    /// iPhone を放さない不具合(docs/iphone-release-fix.md)で、閉じたあとにデバイスが止まったかを読む `--release-check` を足した。
    const FLAGS: [&str; 40] = [
        "--add-to-dock", "--annotate-templates", "--backend", "--caps-check", "--compare", "--complete-check", "--context",
        "--correct-check", "--device", "--help", "--install-app", "--iphone",
        "--latency-check", "--list-devices", "--method", "--mic-check", "--model", "--only", "--vad-threshold",
        "--trace", "--no-vad", "--open", "--out", "--previous", "--ratio", "--recognize-check", "--release-check", "--save",
        "--say", "--screenshot", "--segment-check", "--set-off", "--set-on", "--start-ms",
        "--target", "--target-file", "--transfer-check", "--version", "--wait", "--whole-text",
    ];

    #[test]
    fn the_flag_set_is_unchanged() {
        let mut known: Vec<&str> = MODES.iter().flat_map(|mode| mode.flags.iter().copied()).collect();
        known.sort_unstable();
        known.dedup();
        let mut expected = FLAGS.to_vec();
        expected.sort_unstable();
        assert_eq!(known, expected);
    }

    /// 使い方の行は、その行が使う引数を全部書いている。逆に、使い方に書いた引数はどれも受け付ける
    /// (`--help` に出るのに「知らない引数」で断る、を起こさない)。表と文がずれない。
    #[test]
    fn every_flag_appears_in_its_usage_line() {
        for mode in MODES {
            let written: Vec<&str> = mode
                .usage
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
                .filter(|word| word.starts_with("--") && word.len() > 2)
                .collect();
            for flag in mode.flags {
                assert!(written.contains(flag), "{flag} が「{}」に無い", mode.usage);
            }
            for word in written {
                assert!(is_known_flag(word), "「{}」の {word} は受け付けない", mode.usage);
            }
        }
        assert!(usage().starts_with("使い方: openspeech [引数]\n  (引数なし)"));
    }
}
