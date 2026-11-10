//! 録音・切り出し・認識の診断(`--mic-check`・`--latency-check`・`--segment-check`・`--recognize-check`)。

use super::*;

/// 声の判定役(Silero VAD)を用意する。`--no-vad` か、モデル・ライブラリが無ければ None(音量だけ)。
/// 使えない理由と、使うモデルの名前を印字する。
pub(crate) fn make_judge(args: &[String]) -> Option<Box<dyn VoiceJudge>> {
    if args.iter().any(|a| a == "--no-vad") {
        println!("声の判定: なし(--no-vad)");
        return None;
    }
    let (Some(model), Some(library)) = (speechcpp::find_vad_model(), speechcpp::find_library())
    else {
        println!("声の判定: なし({})", speechcpp::VAD_MISSING);
        return None;
    };
    // `--vad-threshold 0.3`: Silero の声の確からしさの境(既定 0.5)を変えて試す(机上の試験用)。
    let threshold = argument_value(args, "--vad-threshold").and_then(|v| v.parse::<f32>().ok());
    match VoiceDetector::load(&library, &model).and_then(|detector| detector.start(threshold)) {
        Ok(detection) => {
            println!(
                "声の判定: {}{}",
                detection.name(),
                threshold.map_or(String::new(), |t| format!(" threshold={t}"))
            );
            Some(Box::new(detection))
        }
        Err(message) => {
            println!("声の判定: なし({message})");
            None
        }
    }
}

/// `--recognize-check 音声.wav...`: 切り出し済みの音をいつもの認識エンジンで文にして印字する(録音の仕分け用)。
pub(crate) fn run_recognize_check(args: &[String]) -> i32 {
    // 音声ファイルは `--recognize-check` の後ろ(`--backend 値` を挟んでもよい)。
    let paths: Vec<&String> = {
        let mut paths = Vec::new();
        let mut take = false;
        let mut skip_value = false;
        for arg in args {
            if skip_value {
                skip_value = false;
            } else if arg == "--backend" {
                skip_value = true;
            } else if arg.starts_with("--") {
                take = arg == "--recognize-check";
            } else if take {
                paths.push(arg);
            }
        }
        paths
    };
    if args.iter().any(|a| a == "--backend") && argument_value(args, "--backend").is_none_or(|v| v.starts_with("--")) {
        eprintln!("--backend の後ろに remote と書いてください");
        return 2;
    }
    if paths.is_empty() {
        eprintln!("音声ファイルを指定してください");
        return 1;
    }
    let store = Store::open();
    let global = store.load_global();
    let data = store.load_user();
    let options = openspeech::app::recognize_options(&data);
    // `--backend remote`: 設定の外部 API(と鍵)を、エンジンの選択に関わらず使う。
    let remote = match argument_value(args, "--backend").map(String::as_str) {
        None => None,
        Some("remote") => {
            let chosen = GlobalSettings {
                recognizer: RecognizerKind::Remote,
                ..global.clone()
            };
            let Some(provider) = openspeech::app::make_remote(&store, &chosen) else {
                eprintln!("外部 API を用意できません(設定の「認識」で API と鍵を確かめてください)");
                return 1;
            };
            let words = if global.remote.send_glossary {
                remote_asr::hint_words(&data.dictionary)
            } else {
                Vec::new()
            };
            let hints = remote_asr::Hints {
                words,
                language: remote_asr::LANGUAGE,
            };
            let timeout = Duration::from_secs(u64::from(global.remote.timeout_secs.clamp(1, 120)));
            Some((provider, hints, timeout))
        }
        Some(other) => {
            eprintln!("--backend は remote だけです: {other}");
            return 2;
        }
    };
    let Some(mut engine) = openspeech::app::make_recognizer(&global) else {
        eprintln!("認識エンジンを用意できません");
        return 1;
    };
    if let Err(message) = engine.prepare() {
        eprintln!("{message}");
        return 1;
    }
    for path in paths {
        let file = std::path::Path::new(path);
        match engine.recognize_file(file, &options) {
            Ok(recognition) => {
                let haas = format!("{path}\t{}\t{}", recognition.text.chars().count(), recognition.text);
                match &remote {
                    None => println!("{haas}"),
                    Some((provider, hints, timeout)) => {
                        let tidy = Tidy {
                            data: &data,
                        };
                        let line = remote_check_line(file, &recognition.text, &tidy, provider.as_ref(), hints, *timeout);
                        println!("{haas}\t{line}");
                    }
                }
            }
            Err(message) => println!("{path}\tERROR\t{message}"),
        }
    }
    0
}

/// アプリが発話の文を整える手順(訂正役の無いときと同じ)。命令語を切り離し、整形する。
pub(crate) struct Tidy<'a> {
    pub(crate) data: &'a openspeech::core::user_data::UserData,
}

impl Tidy<'_> {
    /// Haas の文を、アプリが表示する形に(文末の命令語を切り離し、整形する)。(命令語を除いた文, 整形した文)。
    fn haas(&self, text: &str) -> (String, String) {
        let settings = &self.data.settings;
        let heard = match match_utterance(text, &settings.command_words, &self.data.dictionary.symbols) {
            Match::Command { prefix, .. } => prefix,
            _ => text.to_string(),
        };
        let shown = apply_prefilters(&heard, &settings.filters, &self.data.dictionary);
        (heard, shown)
    }

    /// API の文を、アプリが使う形に(改行を除き、整形し、文末の命令語を外す)。
    fn api(&self, text: &str) -> String {
        let settings = &self.data.settings;
        let text = apply_prefilters(&text.replace(['\r', '\n'], ""), &settings.filters, &self.data.dictionary);
        match match_after_correction(&text, &settings.command_words) {
            Match::Command { prefix, .. } => prefix,
            _ => text,
        }
    }
}

/// `--recognize-check … --backend remote` の後半の欄(字数か SKIP・LONG・SHORT・ERROR, 文か理由)。
/// アプリと同じ順に、命令語を切り離して整形してから門番(短い発話は送らない、長すぎ・短すぎる答えは使わない)
/// を当てる。学習の置き換えは当てない。
pub(crate) fn remote_check_line(
    file: &std::path::Path,
    recognized: &str,
    tidy: &Tidy,
    provider: &dyn remote_asr::RemoteProvider,
    hints: &remote_asr::Hints,
    timeout: Duration,
) -> String {
    let (heard, haas) = tidy.haas(recognized);
    if !remote_asr::worth_sending(&heard) {
        return "SKIP\t声と判定しない短さなので送らない".to_string();
    }
    let wav = audio::read_wav(file).and_then(|samples| openspeech::core::recognizer::wav_bytes(&samples));
    let wav = match wav {
        Ok(wav) => wav,
        Err(message) => return format!("ERROR\t{message}"),
    };
    match provider.transcribe(&wav, hints, timeout).map(|text| tidy.api(&text)) {
        Ok(text) if text.trim().is_empty() => "EMPTY\t".to_string(),
        Ok(text) if remote_asr::too_long(&haas, &text) => format!("LONG\t{text}"),
        Ok(text) if remote_asr::too_short(&haas, &text) => format!("SHORT\t{text}"),
        Ok(text) => format!("{}\t{text}", text.chars().count()),
        Err(error) => format!("ERROR\t{error}"),
    }
}

/// `--iphone` で使う iPhone のマイク。アプリと同じく、最後に音が届いたものを選ぶ。
pub(crate) fn chosen_iphone() -> Option<String> {
    let last = Store::open().load_global().last_iphone;
    audio::iphone_device_name(last.as_deref())
}

/// `--mic-check [秒数]`(`--iphone` か `--device 名前`、`--out ファイル` を併用可):
/// 録音デバイスを開き、音が届くかを確かめる。iPhone 連係の確認に使う。届いたら 0、無音なら 2。
/// 開くのにかかった時間と、0 だけの音の始まりと終わりを時刻付きで出す。
/// iPhone を手に取る・置き直すと、連係が切れて戻るまでの様子が見える。
pub(crate) fn run_mic_check(args: &[String]) -> i32 {
    /// これより短い 0 の並びは切断と見なさない。
    const ZERO_RUN: Duration = Duration::from_millis(500);
    let seconds = argument_value(args, "--mic-check")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(3);
    let device = argument_value(args, "--device")
        .cloned()
        .or_else(|| args.iter().any(|a| a == "--iphone").then(chosen_iphone).flatten());
    let (tx, rx) = std::sync::mpsc::channel();
    let began = Instant::now();
    let lost = std::sync::Arc::new(|error: audio::MicError| {
        if error == audio::MicError::Gone {
            println!("デバイスが無くなった(iPhone なら連係が切れた)");
        }
    });
    let capture = match audio::Capture::start(device.clone(), tx, lost) {
        Ok(capture) => capture,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let elapsed = |at: Instant| at.duration_since(began).as_secs_f32();
    println!("{:6.1}s 開いた", elapsed(Instant::now()));
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let (mut samples, mut signal, mut peak) = (0usize, false, 0f32);
    let mut zero_since: Option<Instant> = None;
    let mut zero_reported = false;
    while Instant::now() < deadline {
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
            samples += chunk.len();
            peak = peak.max(openspeech::core::segmenter::rms(&chunk));
            let now = Instant::now();
            if openspeech::core::session::has_signal(&chunk) {
                if !signal {
                    println!("{:6.1}s 音が届いた", elapsed(now));
                } else if zero_reported {
                    let lost = zero_since.map_or(0.0, |at| now.duration_since(at).as_secs_f32());
                    println!("{:6.1}s 音が戻った(0 だけの音 {lost:.1} 秒)", elapsed(now));
                }
                signal = true;
                zero_since = None;
                zero_reported = false;
            } else {
                let since = *zero_since.get_or_insert(now);
                if !zero_reported && now.duration_since(since) >= ZERO_RUN {
                    zero_reported = true;
                    println!("{:6.1}s 0 だけの音が続いている", elapsed(since));
                }
            }
        }
    }
    let report = format!(
        "device: {}\nsamples: {samples}\nsignal: {signal}\npeak_rms: {peak:.5}\n",
        device
            .as_deref()
            .map_or("(既定)".to_string(), audio::display_device_name)
    );
    print!("{report}");
    if let Some(out) = argument_value(args, "--out") {
        let _ = std::fs::write(out, &report);
    }
    drop(capture);
    if signal {
        0
    } else {
        2
    }
}

/// `--release-check [--device 名前]`: 名前で開いて約 1 秒録り、閉じたあとでデバイスがまだ動いているかを
/// CoreAudio から読む(docs/iphone-release-fix.md の B)。名前で開いたデバイスを閉じても IO が止まらない不具合
/// (cpal 0.15 の循環参照)を確かめる。デバイスを省くと、見えている iPhone、無ければ OS の既定のマイクを名前で開く。
/// 止まれば 0、動いたままなら 2、開けなければ 1。ほかのアプリが同じマイクを使っていると結果が濁るので、開く前の状態も出す。
pub(crate) fn run_release_check(args: &[String]) -> i32 {
    use openspeech::core::device_state::{device_running, DeviceRunning};
    use openspeech::core::audio::MicSource as _;
    /// 閉じたあと、止まるのを待つ上限。
    const SETTLE: Duration = Duration::from_secs(2);
    let named = argument_value(args, "--device").filter(|value| !value.starts_with("--"));
    if args.iter().any(|a| a == "--device") && named.is_none() {
        eprintln!("--device の後ろにデバイスの名前を書いてください(--list-devices で一覧)");
        return 1;
    }
    let device = named
        .cloned()
        .or_else(chosen_iphone)
        .or_else(|| audio::CpalMic.default_device());
    let Some(device) = device else {
        eprintln!("録音デバイスが見つかりません。--device で名前を指定してください");
        return 1;
    };
    let show = |state: Result<DeviceRunning, String>| match state {
        Ok(state) => format!(
            "{}(このプロセス: {})",
            if state.somewhere { "動いている" } else { "止まっている" },
            if state.here { "動いている" } else { "止まっている" }
        ),
        Err(message) => message,
    };
    println!("device: {}", audio::display_device_name(&device));
    let before = device_running(&device);
    println!("開く前: {}", show(before.clone()));
    // 見つからない名前を開くと既定のマイクへ落ちる(聞き役が付かない道)ので、確かめにならない。開く前に断る。
    if before.is_err() {
        return 1;
    }
    if before.as_ref().is_ok_and(|state| state.somewhere) {
        println!("  (ほかのアプリがこのマイクを使っている。閉じたあとの結果は当てにならない)");
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let began = Instant::now();
    let capture = match audio::Capture::start(Some(device.clone()), tx, std::sync::Arc::new(|_| {})) {
        Ok(capture) => capture,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    println!("開いた: {:.2} 秒", began.elapsed().as_secs_f32());
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut samples = 0usize;
    while Instant::now() < deadline {
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(50)) {
            samples += chunk.len();
        }
    }
    println!("録音中: {}(届いた標本 {samples})", show(device_running(&device)));
    let closing = Instant::now();
    drop(capture);
    println!("閉じた: {:.2} 秒", closing.elapsed().as_secs_f32());
    let closed = Instant::now();
    let mut state = device_running(&device);
    while state.as_ref().is_ok_and(|state| state.somewhere) && closed.elapsed() < SETTLE {
        std::thread::sleep(Duration::from_millis(100));
        state = device_running(&device);
    }
    println!(
        "閉じたあと({:.1} 秒): {}",
        closed.elapsed().as_secs_f32(),
        show(state.clone())
    );
    match state {
        Ok(state) if state.somewhere => {
            println!("結果: 動いている(閉じても放していない)");
            2
        }
        Ok(_) => {
            println!("結果: 止まっている");
            0
        }
        Err(_) => 1,
    }
}

/// `--latency-check [秒]`(`--iphone` / `--device 名前` / `--save フォルダ` を併用可):
/// 録音 → 切り出し → 認識の各段階の時刻を出し、どこで待っているかを見る。設定はいつものものを使う。
/// `--save` を付けると、切り出した音をそのフォルダに utt_NN.wav として残す(認識の切り分け用)。
/// 途中結果(live preview)に対応したエンジンなら、話している最中の途中認識の時刻も出す。
pub(crate) fn run_latency_check(args: &[String]) -> i32 {
    use openspeech::core::segmenter::{rms, Segmenter, SegmenterConfig, SAMPLE_RATE};
    let seconds = argument_value(args, "--latency-check")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(12);
    let device = argument_value(args, "--device")
        .cloned()
        .or_else(|| args.iter().any(|a| a == "--iphone").then(chosen_iphone).flatten());
    let store = Store::open();
    let global = store.load_global();
    let data = store.load_user();
    let gain = global.mic_gain;
    let sensitivity = openspeech::core::segmenter::DEFAULT_SENSITIVITY;
    let options = openspeech::app::recognize_options(&data);
    let mut recognizer = openspeech::app::make_recognizer(&global);
    if let Some(engine) = recognizer.as_ref() {
        println!("engine: {}", engine.name());
    }
    if let Some(engine) = recognizer.as_mut() {
        let began = Instant::now();
        match engine.prepare() {
            Ok(()) => println!("engine ready in {:.2}s", began.elapsed().as_secs_f32()),
            Err(message) => println!("{message}"),
        }
    }
    let partial = recognizer.as_ref().is_some_and(|r| r.supports_partial());
    println!(
        "device={} gain={gain:.2} sensitivity={sensitivity:.2} engine={} live_preview={partial}",
        device.as_deref().map_or("(既定)".to_string(), audio::display_device_name),
        recognizer.as_ref().map_or("なし".to_string(), |r| r.name())
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let capture = match audio::Capture::start(device, tx, std::sync::Arc::new(|_| {})) {
        Ok(capture) => capture,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let mut config = SegmenterConfig {
        post_ms: data.settings.engine.post_silence_ms as usize,
        ..SegmenterConfig::default()
    };
    if let Some(ratio) = argument_value(args, "--ratio").and_then(|v| v.parse::<f64>().ok()) {
        config.speech_ratio = ratio;
    }
    println!("speech_ratio={} post_ms={}", config.speech_ratio, config.post_ms);
    let mut segmenter = Segmenter::new(config, sensitivity).with_judge(make_judge(args));
    let start = Instant::now();
    let deadline = start + Duration::from_secs(seconds);
    let mut last_log = Instant::now();
    let mut speaking = false;
    let mut signal_seen = false;
    let save_dir = argument_value(args, "--save").map(std::path::PathBuf::from);
    if let Some(dir) = &save_dir {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut saved = 0;
    let mut full: Vec<f32> = Vec::new();
    let mut live: Vec<f32> = Vec::new();
    let mut live_sent = 0usize;
    let elapsed = |at: Instant| at.duration_since(start).as_secs_f32();
    while Instant::now() < deadline {
        let Ok(mut chunk) = rx.recv_timeout(Duration::from_millis(100)) else {
            continue;
        };
        for sample in &mut chunk {
            *sample = (*sample * gain).clamp(-1.0, 1.0);
        }
        if !signal_seen && openspeech::core::session::has_signal(&chunk) {
            signal_seen = true;
            println!("{:6.2}s 音が届き始めた", elapsed(Instant::now()));
        }
        if save_dir.is_some() {
            full.extend_from_slice(&chunk);
        }
        let level = rms(&chunk);
        let utterances = segmenter.push(&chunk);
        if segmenter.is_speaking() != speaking {
            speaking = segmenter.is_speaking();
            println!(
                "{:6.2}s {}",
                elapsed(Instant::now()),
                if speaking {
                    "発話開始"
                } else {
                    "発話終了(切り出し完了)"
                }
            );
            if !speaking {
                live.clear();
                live_sent = 0;
            }
        }
        if speaking && partial {
            live.extend_from_slice(&chunk);
            if live.len() >= SAMPLE_RATE && live.len() - live_sent >= SAMPLE_RATE / 2 {
                live_sent = live.len();
                if let Some(engine) = recognizer.as_mut() {
                    let began = Instant::now();
                    let text = engine
                        .recognize_partial(&live, &options)
                        .unwrap_or_default();
                    println!(
                        "{:6.2}s 途中認識 {:.2}s 分({:.2}s): {text}",
                        elapsed(Instant::now()),
                        live.len() as f32 / SAMPLE_RATE as f32,
                        began.elapsed().as_secs_f32()
                    );
                }
            }
        }
        if last_log.elapsed() >= Duration::from_millis(250) {
            last_log = Instant::now();
            println!(
                "{:6.2}s level={:.4} volume={:5} floor={:5} threshold={:5} movement={:4} speaking={}",
                elapsed(last_log),
                level,
                segmenter.last_volume(),
                segmenter.noise_floor(),
                segmenter.effective_threshold(),
                segmenter.last_movement(),
                segmenter.is_speaking()
            );
        }
        for utterance in utterances {
            let length = utterance.len() as f32 / SAMPLE_RATE as f32;
            println!(
                "{:6.2}s 切り出し {length:.2}s 分 → 認識開始",
                elapsed(Instant::now())
            );
            if let Some(dir) = &save_dir {
                saved += 1;
                let path = dir.join(format!("utt_{saved:02}.wav"));
                match openspeech::core::recognizer::write_wav(&path, &utterance) {
                    Ok(()) => println!("        保存 {}", path.display()),
                    Err(message) => println!("        保存できず: {message}"),
                }
            }
            let Some(engine) = recognizer.as_mut() else {
                continue;
            };
            let began = Instant::now();
            let result = engine.recognize(&utterance, &options);
            println!(
                "{:6.2}s 認識完了({:.2}s): {}",
                elapsed(Instant::now()),
                began.elapsed().as_secs_f32(),
                result.map_or_else(|e| format!("エラー {e}"), |r| r.text)
            );
        }
    }
    drop(capture);
    if let Some(dir) = &save_dir {
        let path = dir.join("full.wav");
        match openspeech::core::recognizer::write_wav(&path, &full) {
            Ok(()) => println!("録音全体を保存 {}", path.display()),
            Err(message) => println!("録音全体を保存できず: {message}"),
        }
    }
    0
}

/// `--segment-check 音声.wav [--ratio 比] [--start-ms 長さ] [--no-vad] [--trace] [--vad-threshold 境]`: 録音済みの音(16kHz モノラル、増幅済み)を
/// 発話の切り出しに通し、どこで話し始め・話し終わりと判定されたかを出す。切り出しの調整用。
pub(crate) fn run_segment_check(args: &[String]) -> i32 {
    use openspeech::core::segmenter::{Segmenter, SegmenterConfig, SAMPLE_RATE};
    let Some(path) = argument_value(args, "--segment-check") else {
        eprintln!("音声ファイルを指定してください");
        return 1;
    };
    let reader = match hound::WavReader::open(path) {
        Ok(reader) => reader,
        Err(error) => {
            eprintln!("音声ファイルを読めません: {error}");
            return 1;
        }
    };
    let samples: Vec<f32> = reader
        .into_samples::<i16>()
        .filter_map(Result::ok)
        .map(|s| s as f32 / i16::MAX as f32)
        .collect();
    let store = Store::open();
    let data = store.load_user();
    let mut config = SegmenterConfig {
        post_ms: data.settings.engine.post_silence_ms as usize,
        ..SegmenterConfig::default()
    };
    if let Some(ratio) = argument_value(args, "--ratio").and_then(|v| v.parse::<f64>().ok()) {
        config.speech_ratio = ratio;
    }
    if let Some(ms) = argument_value(args, "--start-ms").and_then(|v| v.parse::<usize>().ok()) {
        config.speech_start_min_ms = ms;
    }
    let sensitivity = openspeech::core::segmenter::DEFAULT_SENSITIVITY;
    println!(
        "{} ({:.2} 秒) ratio={} start_ms={} post_ms={} sensitivity={:.2}",
        path,
        samples.len() as f32 / SAMPLE_RATE as f32,
        config.speech_ratio,
        config.speech_start_min_ms,
        config.post_ms,
        sensitivity
    );
    let mut segmenter = Segmenter::new(config, sensitivity).with_judge(make_judge(args));
    let trace = args.iter().any(|a| a == "--trace");
    let mut pushed = 0usize;
    let mut speaking = false;
    let mut count = 0;
    for chunk in samples.chunks(SAMPLE_RATE / 50) {
        let done = segmenter.push(chunk);
        pushed += chunk.len();
        let at = pushed as f32 / SAMPLE_RATE as f32;
        if trace {
            if let Some(f) = segmenter.last_frame() {
                println!(
                    "{at:6.2}s vol={:5} thr={:5} va={} ma={} spk={} kept={} end={} loud={}{}{}",
                    f.volume,
                    f.threshold,
                    u8::from(f.volume_active),
                    u8::from(f.movement_active),
                    u8::from(f.verdict.speaking),
                    u8::from(f.verdict.kept),
                    f.verdict.ended.map_or("-".to_string(), |e| format!("{e:.2}")),
                    u8::from(f.loud),
                    if f.dead { " dead" } else { "" },
                    if segmenter.is_speaking() { " SPEECH" } else { "" },
                );
            }
        }
        if segmenter.is_speaking() != speaking {
            speaking = segmenter.is_speaking();
            if speaking {
                println!(
                    "{at:6.2}s 話し始め volume={} floor={} threshold={}",
                    segmenter.last_volume(),
                    segmenter.noise_floor(),
                    segmenter.effective_threshold()
                );
            }
        }
        for utterance in done {
            count += 1;
            let length = utterance.len() as f32 / SAMPLE_RATE as f32;
            println!(
                "{at:6.2}s 切り出し {count}: {length:.2} 秒分({:.2}s〜)",
                at - length
            );
        }
    }
    if let Some(utterance) = segmenter.flush() {
        count += 1;
        println!(
            "  終端 切り出し {count}: {:.2} 秒分",
            utterance.len() as f32 / SAMPLE_RATE as f32
        );
    }
    println!("切り出し {count} 件");
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use openspeech::core::remote_asr::{Hints, RemoteError, RemoteProvider};

    /// 決まった答えを返す偽の外部 API。受けた音声が WAV かを確かめる。
    struct Fixed(Result<String, RemoteError>);

    impl RemoteProvider for Fixed {
        fn label(&self) -> String {
            "偽".to_string()
        }

        fn transcribe(&self, wav: &[u8], _hints: &Hints, _timeout: Duration) -> Result<String, RemoteError> {
            assert!(wav.starts_with(b"RIFF") && wav.len() > 44);
            self.0.clone()
        }
    }

    #[test]
    fn the_remote_check_applies_the_same_gates_as_the_app() {
        let file = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/sample-ja.wav"));
        let hints = Hints::default();
        let second = Duration::from_secs(1);
        let mut data = openspeech::core::user_data::UserData::default();
        let row = data
            .settings
            .command_words
            .iter_mut()
            .find(|row| row.action == openspeech::core::voice_command::CommandAction::NewLine)
            .expect("改行の行");
        row.words = vec!["改行".to_string()];
        let tidy = Tidy { data: &data };
        let ok = Fixed(Ok("右肺上葉に5mmの結節を認めます。\n改行".to_string()));
        assert_eq!(
            remote_check_line(file, "右肺上葉に結節", &tidy, &ok, &hints, second),
            "18\t右肺上葉に5mm の結節を認めます。",
            "改行を除き、アプリと同じ整形をして、文末の命令語を外してから比べる"
        );
        assert!(remote_check_line(file, "うん", &tidy, &ok, &hints, second).starts_with("SKIP\t"));
        assert!(remote_check_line(file, "うん 改行", &tidy, &ok, &hints, second).starts_with("SKIP\t"));
        let long = Fixed(Ok("あ".repeat(60)));
        assert!(remote_check_line(file, "右肺上葉に結節", &tidy, &long, &hints, second).starts_with("LONG\t"));
        let short = Fixed(Ok("右肺".to_string()));
        assert!(remote_check_line(file, "右肺上葉に結節。胸水なし。", &tidy, &short, &hints, second).starts_with("SHORT\t"));
        let failed = Fixed(Err(RemoteError::Temporary("時間切れ".to_string())));
        assert_eq!(remote_check_line(file, "右肺上葉に結節", &tidy, &failed, &hints, second), "ERROR\t時間切れ");
    }
}
