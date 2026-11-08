//! マイクの録音と音声の再生。
//! cpal のストリームは別スレッドへ渡せないため、それぞれ専用スレッドの中で持ち続ける。

use super::segmenter::SAMPLE_RATE;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, Stream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::path::Path;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// 録音デバイスの入口。試験では偽のデバイスに差し替える。
pub trait MicSource: Send + Sync {
    fn device_names(&self) -> Vec<String>;
    /// OS の既定のマイクの名前。
    fn default_device(&self) -> Option<String>;
    /// デバイスを開き、16kHz モノラルの音を `samples` へ流し始める。連係の接続で数秒かかることがある。
    /// 戻り値を落とすと録音が止まり、デバイスが閉じる。
    fn open(
        &self,
        device: Option<String>,
        samples: Sender<Vec<f32>>,
    ) -> Result<Box<dyn Send>, String>;
}

/// 本物のデバイス。
pub struct CpalMic;

impl MicSource for CpalMic {
    fn device_names(&self) -> Vec<String> {
        input_device_names()
    }

    fn default_device(&self) -> Option<String> {
        cpal::default_host()
            .default_input_device()
            .and_then(|device| device.name().ok())
    }

    fn open(
        &self,
        device: Option<String>,
        samples: Sender<Vec<f32>>,
    ) -> Result<Box<dyn Send>, String> {
        Capture::start(device, samples).map(|capture| Box::new(capture) as Box<dyn Send>)
    }
}

pub fn input_device_names() -> Vec<String> {
    cpal::default_host()
        .input_devices()
        .map(|devices| devices.filter_map(|d| d.name().ok()).collect())
        .unwrap_or_default()
}

/// 近くの iPhone が Apple の「連係」で録音デバイスとして現れたときの名前は「〇〇のiPhoneのマイク」。
/// Mac は iPhone を Bluetooth のマイクとしては受け付けないため、これがワイヤレスで使う唯一の形。
pub fn is_iphone_device(name: &str) -> bool {
    name.to_lowercase().contains("iphone")
}

/// 表示用の名前。連係のデバイス名には先頭に見えない向き指定の文字(U+200E)が付くので外す。
pub fn display_device_name(name: &str) -> String {
    name.chars()
        .filter(|c| {
            !matches!(
                c,
                '\u{200E}' | '\u{200F}' | '\u{FEFF}' | '\u{202A}'..='\u{202E}'
            )
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// 名前が同じデバイスか。見えない文字の有無は無視する。
pub fn same_device(a: &str, b: &str) -> bool {
    a == b || display_device_name(a) == display_device_name(b)
}

/// いま見つかっている iPhone のマイク。無ければ None。
pub fn iphone_device_name(last: Option<&str>) -> Option<String> {
    choose_iphone(&input_device_names(), last)
}

/// 見えている iPhone のマイクから 1 つ選ぶ。2 台あれば、最後に音が届いた方を選ぶ
/// (先に見つかった方だと、置いていない iPhone を選ぶことがある)。
pub fn choose_iphone(names: &[String], last: Option<&str>) -> Option<String> {
    let mut iphones = names.iter().filter(|name| is_iphone_device(name));
    let first = iphones.clone().next()?;
    let preferred = last.and_then(|last| iphones.find(|name| same_device(name, last)));
    Some(preferred.unwrap_or(first).clone())
}

/// iPhone を諦めたときに使うデバイス。None は OS の既定のマイク。
pub fn fallback_device(
    configured: Option<&str>,
    default: Option<&str>,
    names: &[String],
) -> Option<String> {
    if let Some(name) = configured.filter(|name| !is_iphone_device(name)) {
        return Some(name.to_string());
    }
    if default.is_none_or(|name| !is_iphone_device(name)) {
        return None;
    }
    names.iter().find(|name| !is_iphone_device(name)).cloned()
}

/// 録音中の印。落とすとデバイスが閉じ切るまで待つ。
pub struct Capture {
    stop: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// 16kHz モノラルの音を `samples` へ流し続ける。デバイスが開くまで呼び元を待たせるので、
    /// 画面のスレッドからは呼ばない(`Session::start` は裏のスレッドで呼ぶ)。
    pub fn start(
        device_name: Option<String>,
        samples: Sender<Vec<f32>>,
    ) -> Result<Capture, String> {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let thread = thread::spawn(move || match open_input(device_name, samples) {
            Ok(stream) => {
                let _ = ready_tx.send(Ok(()));
                let _ = stop_rx.recv();
                drop(stream);
            }
            Err(message) => {
                let _ = ready_tx.send(Err(message));
            }
        });
        ready_rx
            .recv()
            .map_err(|_| "録音を開始できません".to_string())??;
        Ok(Capture {
            stop: stop_tx,
            thread: Some(thread),
        })
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn open_input(device_name: Option<String>, samples: Sender<Vec<f32>>) -> Result<Stream, String> {
    let host = cpal::default_host();
    let named = device_name.as_ref().and_then(|name| {
        host.input_devices()
            .ok()?
            .find(|d| d.name().is_ok_and(|n| same_device(&n, name)))
    });
    // 消えた iPhone の代わりに黙って Mac のマイクを開くと、iPhone で録っていると見誤る。
    if named.is_none() && device_name.as_deref().is_some_and(is_iphone_device) {
        return Err("iPhone のマイクが見つかりません".to_string());
    }
    let device = named
        .or_else(|| host.default_input_device())
        .ok_or("選択中の録音デバイスが見つかりません")?;
    let config = device
        .default_input_config()
        .map_err(|e| format!("録音デバイスを使えません: {e}"))?;
    let channels = config.channels() as usize;
    let rate = config.sample_rate().0 as usize;
    super::diag::log(format!(
        "録音開始 device={} rate={rate} channels={channels} format={:?}",
        device
            .name()
            .map_or_else(|_| "?".to_string(), |n| display_device_name(&n)),
        config.sample_format()
    ));
    let stream_config = config.config();
    let on_error = |error| super::diag::log(format!("録音エラー: {error}"));
    let mut resampler = Resampler::new(rate);
    let mut deliver = move |mono: Vec<f32>| {
        let _ = samples.send(resampler.process(&mono));
    };
    let stream = match config.sample_format() {
        SampleFormat::F32 => device.build_input_stream(
            &stream_config,
            move |data: &[f32], _: &_| deliver(downmix(data, channels, |s| s)),
            on_error,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream(
            &stream_config,
            move |data: &[i16], _: &_| {
                deliver(downmix(data, channels, |s| s as f32 / i16::MAX as f32))
            },
            on_error,
            None,
        ),
        SampleFormat::U16 => device.build_input_stream(
            &stream_config,
            move |data: &[u16], _: &_| {
                deliver(downmix(data, channels, |s| (s as f32 - 32768.0) / 32768.0))
            },
            on_error,
            None,
        ),
        other => return Err(format!("対応していない音声形式です: {other:?}")),
    }
    .map_err(|e| format!("録音を開始できません: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("録音を開始できません: {e}"))?;
    Ok(stream)
}

fn downmix<T: Copy>(data: &[T], channels: usize, convert: impl Fn(T) -> f32) -> Vec<f32> {
    data.chunks(channels.max(1))
        .map(|frame| frame.iter().map(|s| convert(*s)).sum::<f32>() / frame.len() as f32)
        .collect()
}

/// 線形補間で 16kHz に直す。呼び出しをまたいで位置を保つ。
pub struct Resampler {
    step: f64,
    position: f64,
    last: f32,
}

impl Resampler {
    pub fn new(source_rate: usize) -> Self {
        Resampler {
            step: source_rate as f64 / SAMPLE_RATE as f64,
            position: 0.0,
            last: 0.0,
        }
    }

    pub fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let mut output = Vec::with_capacity((input.len() as f64 / self.step) as usize + 1);
        while self.position < input.len() as f64 {
            let index = self.position.floor() as isize;
            let fraction = (self.position - index as f64) as f32;
            let before = if index <= 0 {
                self.last
            } else {
                input[index as usize - 1]
            };
            let after = input[index.max(0) as usize];
            output.push(before + (after - before) * fraction);
            self.position += self.step;
        }
        self.position -= input.len() as f64;
        if let Some(last) = input.last() {
            self.last = *last;
        }
        output
    }
}

/// 再生中の印。`stop` で止める。
#[derive(Clone)]
pub struct Playback {
    stopped: Arc<AtomicBool>,
}

impl Playback {
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    pub fn is_playing(&self) -> bool {
        !self.stopped.load(Ordering::Relaxed)
    }
}

/// 16kHz モノラルの音を既定の出力へ再生する。
pub fn play(samples: Arc<Vec<f32>>) -> Result<Playback, String> {
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    thread::spawn(move || {
        let result = open_output(samples, flag.clone());
        match result {
            Ok(stream) => {
                let _ = ready_tx.send(Ok(()));
                while !flag.load(Ordering::Relaxed) {
                    thread::sleep(std::time::Duration::from_millis(30));
                }
                drop(stream);
            }
            Err(message) => {
                flag.store(true, Ordering::Relaxed);
                let _ = ready_tx.send(Err(message));
            }
        }
    });
    ready_rx
        .recv()
        .map_err(|_| "再生を開始できません".to_string())??;
    Ok(Playback { stopped })
}

fn open_output(samples: Arc<Vec<f32>>, finished: Arc<AtomicBool>) -> Result<Stream, String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("再生デバイスが見つかりません")?;
    let config = device
        .default_output_config()
        .map_err(|e| format!("再生デバイスを使えません: {e}"))?;
    if config.sample_format() != SampleFormat::F32 {
        return Err("対応していない再生形式です".to_string());
    }
    let channels = config.channels() as usize;
    let step = SAMPLE_RATE as f64 / config.sample_rate().0 as f64;
    let mut position = 0f64;
    let stream = device
        .build_output_stream(
            &config.config(),
            move |data: &mut [f32], _: &_| {
                for frame in data.chunks_mut(channels) {
                    let value = samples.get(position as usize).copied().unwrap_or(0.0);
                    frame.fill(value);
                    position += step;
                }
                if position as usize >= samples.len() {
                    finished.store(true, Ordering::Relaxed);
                }
            },
            |error| eprintln!("再生エラー: {error}"),
            None,
        )
        .map_err(|e| format!("再生を開始できません: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("再生を開始できません: {e}"))?;
    Ok(stream)
}

/// WAV を 16kHz モノラルの標本にする(試験と診断用。アプリ本体は切り出した標本をそのまま渡す)。
pub fn read_wav(path: &Path) -> Result<Vec<f32>, String> {
    let mut reader =
        hound::WavReader::open(path).map_err(|e| format!("音声ファイルを開けません: {e}"))?;
    let spec = reader.spec();
    let channels = spec.channels.max(1) as usize;
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let full_scale = (1u64 << (spec.bits_per_sample.max(1) - 1)) as f32;
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / full_scale))
                .collect::<Result<_, _>>()
        }
        hound::SampleFormat::Float => reader.samples::<f32>().collect(),
    }
    .map_err(|e| format!("音声ファイルを読めません: {e}"))?;
    let mono: Vec<f32> = raw
        .chunks(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect();
    if spec.sample_rate as usize == SAMPLE_RATE || mono.is_empty() {
        return Ok(mono);
    }
    let ratio = spec.sample_rate as f64 / SAMPLE_RATE as f64;
    let length = (mono.len() as f64 / ratio) as usize;
    let last = mono.len() - 1;
    Ok((0..length)
        .map(|i| {
            let position = i as f64 * ratio;
            let index = position as usize;
            let fraction = (position - index as f64) as f32;
            let a = mono[index.min(last)];
            let b = mono[(index + 1).min(last)];
            a + (b - a) * fraction
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_halves_a_32k_signal() {
        let mut resampler = Resampler::new(32_000);
        let output = resampler.process(&vec![0.5; 3200]);
        assert_eq!(output.len(), 1600);
    }

    #[test]
    fn resampling_keeps_position_across_calls() {
        let mut resampler = Resampler::new(48_000);
        let total: usize = (0..10)
            .map(|_| resampler.process(&vec![0.0; 100]).len())
            .sum();
        assert!((332..=335).contains(&total), "{total}");
    }

    #[test]
    fn downmix_averages_channels() {
        assert_eq!(downmix(&[1.0f32, 0.0, 0.5, 0.5], 2, |s| s), vec![0.5, 0.5]);
    }

    #[test]
    fn iphone_devices_are_recognized_by_name() {
        assert!(is_iphone_device("\u{200E}shinのiPhoneのマイク"));
        assert!(is_iphone_device("Shin's iPhone Microphone"));
        assert!(!is_iphone_device("MacBook Proのマイク"));
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn the_iphone_that_last_worked_is_chosen() {
        let two = names(&[
            "MacBook Proのマイク",
            "\u{200E}shinのiPhoneのマイク",
            "山田 花子のiPhoneのマイク",
        ]);
        assert_eq!(
            choose_iphone(&two, None).as_deref(),
            Some("\u{200E}shinのiPhoneのマイク")
        );
        assert_eq!(
            choose_iphone(&two, Some("山田 花子のiPhoneのマイク")).as_deref(),
            Some("山田 花子のiPhoneのマイク")
        );
        assert_eq!(
            choose_iphone(&two, Some("shinのiPhoneのマイク")).as_deref(),
            Some("\u{200E}shinのiPhoneのマイク"),
            "見えない文字の差は無視する"
        );
        assert_eq!(
            choose_iphone(&two, Some("別のiPhoneのマイク")).as_deref(),
            Some("\u{200E}shinのiPhoneのマイク")
        );
        assert_eq!(choose_iphone(&names(&["Macのマイク"]), None), None);
    }

    #[test]
    fn giving_up_on_the_iphone_never_picks_an_iphone() {
        let all = names(&["shinのiPhoneのマイク", "MacBook Proのマイク"]);
        assert_eq!(
            fallback_device(Some("USBマイク"), Some("MacBook Proのマイク"), &all).as_deref(),
            Some("USBマイク")
        );
        assert_eq!(
            fallback_device(None, Some("MacBook Proのマイク"), &all),
            None,
            "既定が Mac ならそのまま"
        );
        assert_eq!(
            fallback_device(
                Some("shinのiPhoneのマイク"),
                Some("shinのiPhoneのマイク"),
                &all
            )
            .as_deref(),
            Some("MacBook Proのマイク")
        );
    }

    #[test]
    fn display_name_drops_invisible_marks() {
        assert_eq!(
            display_device_name("\u{200E}shinのiPhoneのマイク"),
            "shinのiPhoneのマイク"
        );
        assert!(same_device(
            "\u{200E}shinのiPhoneのマイク",
            "shinのiPhoneのマイク"
        ));
        assert!(!same_device("MacBook Proのマイク", "shinのiPhoneのマイク"));
    }
}
