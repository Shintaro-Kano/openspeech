//! Apple のオンデバイス音声認識(Speech framework の SpeechAnalyzer + SpeechTranscriber, macOS 26)を
//! 補助プロセス `openspeech-speech`(`tools/openspeech-speech/main.swift`)経由で使う認識エンジン。
//!
//! 補助プロセスとのやりとり:
//! - 標準入力(バイナリ): 4 バイト(リトルエンディアン)の中身の長さ、1 バイトの種類、中身。
//!   種類 1 = 音声(Int16 リトルエンディアン、モノラル 16 kHz)、2 = 発話の終わり、3 = 終了。
//! - 標準出力(1 行 1 JSON): `ready` / `status` / `partial` / `final` / `error`(`text` 付き)。
//!   種類 2 を送るたびに `final` がちょうど 1 行返る(何も聞き取れなければ空文字)。
//!
//! 登録単語(keywords)は Apple の認識には渡せないので使わない。

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use crate::core::recognizer::{Recognition, RecognizeOptions, Recognizer};

pub const HELPER_NAME: &str = "openspeech-speech";
const SAMPLE_RATE: u32 = 16_000;
/// 1 フレームに入れる標本数(0.1 秒)。
const FRAME_SAMPLES: usize = 1_600;
/// 起動(初回はモデルの準備を含む)を待つ時間。
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// 発話の終わりを送ってから確定結果を待つ時間(本機の実測は 35〜120 ms)。
const FINAL_TIMEOUT: Duration = Duration::from_secs(10);

const FRAME_AUDIO: u8 = 1;
const FRAME_END: u8 = 2;
const FRAME_QUIT: u8 = 3;

/// 補助プロセスへ送る 1 フレーム。
pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(payload);
    out
}

/// [-1, 1] の f32 を Int16 リトルエンディアンのバイト列にする。
fn to_i16_bytes(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let s = if s.is_finite() {
            s.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        out.extend_from_slice(&((s * 32767.0).round() as i16).to_le_bytes());
    }
    out
}

/// 1 発話ぶんのフレーム列(音声を 0.1 秒ずつ、最後に発話の終わり)。
fn utterance_frames(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2 + (samples.len() / FRAME_SAMPLES + 2) * 5);
    for chunk in samples.chunks(FRAME_SAMPLES) {
        out.extend_from_slice(&frame(FRAME_AUDIO, &to_i16_bytes(chunk)));
    }
    out.extend_from_slice(&frame(FRAME_END, &[]));
    out
}

/// 複数チャンネルを平均してモノラルにする。
fn mix_to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / frame.len() as f32)
        .collect()
}

/// 線形補間で標本化周波数を変える。
fn resample_linear(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() || from == 0 {
        return samples.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let len = ((samples.len() as f64) / ratio).round().max(1.0) as usize;
    (0..len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let index = pos.floor() as usize;
            let frac = (pos - index as f64) as f32;
            let a = samples[index.min(samples.len() - 1)];
            let b = samples[(index + 1).min(samples.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

/// WAV を読み、モノラル 16 kHz の f32 にする。
fn read_wav_16k_mono(path: &Path) -> Result<Vec<f32>, String> {
    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let spec = reader.spec();
    let interleaved: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<_, _>>()
            .map_err(|e| format!("{}: {e}", path.display()))?,
        hound::SampleFormat::Int => {
            let scale = (1i64 << (spec.bits_per_sample.clamp(1, 32) - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<Result<_, _>>()
                .map_err(|e| format!("{}: {e}", path.display()))?
        }
    };
    let mono = mix_to_mono(&interleaved, spec.channels as usize);
    Ok(resample_linear(&mono, spec.sample_rate, SAMPLE_RATE))
}

/// 補助プロセスの 1 行。
#[derive(Debug, Clone, PartialEq)]
enum Event {
    Ready,
    Status(String),
    Partial(String),
    Final(String),
    Error(String),
}

fn parse_event(line: &str) -> Option<Event> {
    let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let text = value
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    match value.get("type")?.as_str()? {
        "ready" => Some(Event::Ready),
        "status" => Some(Event::Status(text)),
        "partial" => Some(Event::Partial(text)),
        "final" => Some(Event::Final(text)),
        "error" => Some(Event::Error(text)),
        _ => None,
    }
}

/// macOS 26 以降か(SpeechAnalyzer はそれより前には無い)。
fn os_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let Ok(output) = Command::new("/usr/bin/sw_vers")
            .arg("-productVersion")
            .output()
        else {
            return false;
        };
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .split('.')
            .next()
            .and_then(|major| major.parse::<u32>().ok())
            .is_some_and(|major| major >= 26)
    })
}

pub struct AppleSpeech {
    helper: PathBuf,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    events: Option<Receiver<Event>>,
    partial: Option<String>,
}

impl AppleSpeech {
    /// 補助プロセスを探す: 実行ファイルの隣、`../Resources/`、`../MacOS/`、最後にこの crate の
    /// `tools/openspeech-speech/`。
    pub fn find_helper() -> Option<PathBuf> {
        let mut candidates = Vec::new();
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
        {
            candidates.push(dir.join(HELPER_NAME));
            candidates.push(dir.join("../Resources").join(HELPER_NAME));
            candidates.push(dir.join("../MacOS").join(HELPER_NAME));
        }
        if let Some(root) = crate::core::mic_access::development_root() {
            candidates.push(root.join("tools/openspeech-speech").join(HELPER_NAME));
        }
        candidates.into_iter().find(|path| path.is_file())
    }

    pub fn is_available() -> bool {
        os_supported() && Self::find_helper().is_some()
    }

    pub fn new(helper: PathBuf) -> Self {
        AppleSpeech {
            helper,
            child: None,
            stdin: None,
            events: None,
            partial: None,
        }
    }

    /// 最後に届いた途中経過(取り出すと消える)。
    pub fn take_partial(&mut self) -> Option<String> {
        self.partial.take()
    }

    fn is_alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn spawn(&mut self) -> Result<(), String> {
        self.stop();
        let mut child = Command::new(&self.helper)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("{} を起動できません: {e}", self.helper.display()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or("補助プロセスの出力を受け取れません")?;
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("apple-speech-reader".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if let Some(event) = parse_event(&line) {
                        if sender.send(event).is_err() {
                            break;
                        }
                    }
                }
            })
            .map_err(|e| format!("読み取りスレッドを作れません: {e}"))?;
        self.stdin = child.stdin.take();
        self.child = Some(child);
        self.events = Some(receiver);

        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            match self.next_event(deadline) {
                Ok(Event::Ready) => return Ok(()),
                Ok(Event::Status(text)) => eprintln!("Apple 音声認識: {text}"),
                Ok(Event::Error(text)) => {
                    self.stop();
                    return Err(format!("Apple 音声認識を準備できません: {text}"));
                }
                Ok(_) => {}
                Err(e) => {
                    self.stop();
                    return Err(format!("Apple 音声認識を準備できません: {e}"));
                }
            }
        }
    }

    fn next_event(&self, deadline: Instant) -> Result<Event, String> {
        let events = self.events.as_ref().ok_or("補助プロセスが動いていません")?;
        let wait = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(wait) {
            Ok(event) => Ok(event),
            Err(RecvTimeoutError::Timeout) => Err("応答がありません(時間切れ)".into()),
            Err(RecvTimeoutError::Disconnected) => Err("補助プロセスが終了しました".into()),
        }
    }

    fn run(&mut self, samples: &[f32]) -> Result<Recognition, String> {
        // 前の発話の残り(時間切れのあとに遅れて届いた行など)を捨てる。
        if let Some(events) = self.events.as_ref() {
            while events.try_recv().is_ok() {}
        }
        self.partial = None;
        let bytes = utterance_frames(samples);
        let stdin = self.stdin.as_mut().ok_or("補助プロセスが動いていません")?;
        stdin
            .write_all(&bytes)
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("音声を送れません: {e}"))?;

        let deadline = Instant::now() + FINAL_TIMEOUT;
        let mut error = None;
        loop {
            match self.next_event(deadline)? {
                Event::Final(text) => {
                    return match error {
                        Some(e) if text.is_empty() => Err(format!("Apple 音声認識: {e}")),
                        _ => Ok(Recognition {
                            text: text.trim().to_string(),
                            unsure: Vec::new(),
                        }),
                    };
                }
                Event::Partial(text) => self.partial = Some(text),
                Event::Status(text) => eprintln!("Apple 音声認識: {text}"),
                Event::Error(text) => {
                    eprintln!("Apple 音声認識: {text}");
                    error = Some(text);
                }
                Event::Ready => {}
            }
        }
    }

    fn stop(&mut self) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = stdin.write_all(&frame(FRAME_QUIT, &[]));
            let _ = stdin.flush();
        }
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.events = None;
    }
}

impl Recognizer for AppleSpeech {
    fn name(&self) -> String {
        "Apple 音声認識".to_string()
    }

    fn prepare(&mut self) -> Result<(), String> {
        if self.is_alive() && self.events.is_some() {
            return Ok(());
        }
        self.spawn()
    }

    fn recognize(
        &mut self,
        samples: &[f32],
        _options: &RecognizeOptions,
    ) -> Result<Recognition, String> {
        self.prepare()?;
        match self.run(samples) {
            Ok(recognition) => Ok(recognition),
            Err(e) if self.is_alive() => {
                // 生きているのに答えない: 止めておき、次の発話で起動し直す。
                self.stop();
                Err(e)
            }
            Err(e) => {
                eprintln!("Apple 音声認識: {e}。起動し直します");
                self.spawn()?;
                self.run(samples)
            }
        }
    }

    fn recognize_file(
        &mut self,
        path: &Path,
        options: &RecognizeOptions,
    ) -> Result<Recognition, String> {
        let samples = read_wav_16k_mono(path)?;
        self.recognize(&samples, options)
    }
}

impl Drop for AppleSpeech {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_has_length_type_and_payload() {
        assert_eq!(frame(2, &[]), vec![0, 0, 0, 0, 2]);
        assert_eq!(
            frame(1, &[0xAA, 0xBB, 0xCC]),
            vec![3, 0, 0, 0, 1, 0xAA, 0xBB, 0xCC]
        );
        let big = vec![7u8; 0x01_0203];
        let encoded = frame(1, &big);
        assert_eq!(&encoded[..5], &[0x03, 0x02, 0x01, 0x00, 1]);
        assert_eq!(encoded.len(), 5 + big.len());
    }

    #[test]
    fn utterance_is_split_into_audio_frames_then_end() {
        let samples = vec![0.5f32; FRAME_SAMPLES + 10];
        let bytes = utterance_frames(&samples);
        let first = 5 + FRAME_SAMPLES * 2;
        let mut header = (FRAME_SAMPLES as u32 * 2).to_le_bytes().to_vec();
        header.push(1);
        assert_eq!(&bytes[..5], &header[..]);
        assert_eq!(&bytes[first..first + 5], &[20, 0, 0, 0, 1]);
        assert_eq!(&bytes[bytes.len() - 5..], &[0, 0, 0, 0, 2]);
        assert_eq!(bytes.len(), first + 5 + 20 + 5);
    }

    #[test]
    fn converts_float_to_i16_with_clamping() {
        assert_eq!(
            to_i16_bytes(&[0.0, 1.0, -1.0, 2.0, f32::NAN]),
            [0, 0, 0xFF, 0x7F, 0x01, 0x80, 0xFF, 0x7F, 0, 0]
        );
    }

    #[test]
    fn mixes_and_resamples() {
        assert_eq!(mix_to_mono(&[1.0, 0.0, 0.5, 0.5], 2), vec![0.5, 0.5]);
        let up = resample_linear(&[0.0, 1.0], 8_000, 16_000);
        assert_eq!(up, vec![0.0, 0.5, 1.0, 1.0]);
        assert_eq!(
            resample_linear(&[0.0; 48_000], 48_000, 16_000).len(),
            16_000
        );
    }

    #[test]
    fn parses_helper_lines() {
        assert_eq!(parse_event(r#"{"type":"ready"}"#), Some(Event::Ready));
        assert_eq!(
            parse_event(r#"{"type":"final","text":"結節"}"#),
            Some(Event::Final("結節".into()))
        );
        assert_eq!(parse_event("garbage"), None);
    }

    #[test]
    #[ignore = "Apple の音声認識(macOS 26)と補助プロセスのビルドが要る"]
    fn recognizes_sample_with_real_helper() {
        let helper =
            AppleSpeech::find_helper().expect("tools/openspeech-speech/build.sh でビルドする");
        let mut engine = AppleSpeech::new(helper);
        engine.prepare().expect("prepare");
        let options = RecognizeOptions {
            words: Vec::new(),
            readings: std::collections::HashMap::new(),
        };
        let wav = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/sample-ja.wav");
        for round in 0..2 {
            let started = Instant::now();
            let result = engine.recognize_file(&wav, &options).expect("recognize");
            eprintln!(
                "round {round}: {:?} ({} ms)",
                result.text,
                started.elapsed().as_millis()
            );
            assert!(result.text.contains("結節"), "{:?}", result.text);
        }
        // 補助プロセスが落ちても、1 度だけ起動し直してやり直す。
        engine.child.as_mut().unwrap().kill().unwrap();
        engine.child.as_mut().unwrap().wait().unwrap();
        let result = engine
            .recognize_file(&wav, &options)
            .expect("recognize after respawn");
        eprintln!("after respawn: {:?}", result.text);
        assert!(result.text.contains("結節"), "{:?}", result.text);
    }
}
