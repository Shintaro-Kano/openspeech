//! 音声入力の流れ: 録音 → 発話の切り出し → 認識。
//! 画面が隠れていても止まらないよう、画面とは別のスレッドで動かす。

use super::audio::{CpalMic, MicSource};
use super::diag;
use super::recognizer::{Recognition, RecognizeOptions, Recognizer};
use super::segmenter::{rms, Segmenter, SegmenterConfig, VoiceJudge, SAMPLE_RATE};
use super::speechcpp::VoiceDetector;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum SessionEvent {
    Recognized {
        recognition: Recognition,
        audio: Arc<Vec<f32>>,
    },
    FileRecognized {
        recognition: Recognition,
    },
    /// 話している途中の認識結果(途中経過の表示用)。空なら表示を消す(発話が終わったか捨てられた)。
    Partial {
        text: String,
    },
    /// 録音デバイスが開いた。音はまだ届いていないことがある。
    MicOpened {
        generation: u64,
        /// 開く処理そのものにかかった時間。
        took: Duration,
    },
    /// 録音デバイスを開けなかった。
    MicFailed {
        generation: u64,
        message: String,
    },
    Error(String),
}

/// 話し始めの前に含める、途中認識用の音の長さ。
const PARTIAL_PREROLL_MS: usize = 300;
/// 途中認識を送る間隔。
const PARTIAL_INTERVAL_MS: usize = 500;
/// 途中認識に回す最短の長さ(短すぎると当てにならない)。
const PARTIAL_MIN_MS: usize = 1_000;
/// 音が来るのを 1 回待つ長さ。
const SAMPLE_WAIT: Duration = Duration::from_millis(100);
/// 前の録音が閉じるのを待つ上限。閉じる処理が返ってこなくても次の録音へ進めるように。
const CLOSE_WAIT: Duration = Duration::from_secs(8);

enum Job {
    Speech(Vec<f32>),
    File(PathBuf),
    /// 認識エンジンの準備(モデルの読み込み)。画面を待たせないよう認識スレッドで行う。
    Prepare,
    /// 話している途中の音(途中経過の表示用)。新しいものだけ認識し、確定した発話を優先する。
    Partial(Vec<f32>),
    /// 発話が終わった(途中経過の表示を消す)。
    PartialEnd,
}

/// 画面から読む録音の状態。
#[derive(Default)]
pub struct LiveState {
    level_bits: AtomicU32,
    /// いま声とみなす音の大きさ(実効値)。雑音の床に応じて動く。
    threshold_bits: AtomicU32,
    pub speaking: AtomicBool,
    pub too_loud: AtomicBool,
    pending: AtomicU32,
    /// デバイスを開いてから、無音以外の音が一度でも届いたか。
    signal: AtomicBool,
    /// 無音以外の音が届かなくなってからの長さ(サンプル数)。音が来ない時間も数える。
    silent_samples: AtomicU64,
}

/// 完全な 0 だけの音は、マイクの許可が無いときに届くもの。実際のマイクには必ず雑音がある。
pub fn has_signal(samples: &[f32]) -> bool {
    samples.iter().any(|s| *s != 0.0)
}

impl LiveState {
    /// マイクから音が届いているか。
    pub fn has_signal(&self) -> bool {
        self.signal.load(Ordering::Relaxed)
    }

    /// 0 だけの音が続いている長さ。
    pub fn silence(&self) -> Duration {
        let samples = self.silent_samples.load(Ordering::Relaxed);
        Duration::from_secs_f64(samples as f64 / SAMPLE_RATE as f64)
    }

    /// デバイスを開いた時点の状態に戻す。
    fn reset_signal(&self) {
        self.signal.store(false, Ordering::Relaxed);
        self.silent_samples.store(0, Ordering::Relaxed);
    }

    /// 届いた音の塊を数える。
    fn note_chunk(&self, chunk: &[f32]) {
        if has_signal(chunk) {
            self.signal.store(true, Ordering::Relaxed);
            self.silent_samples.store(0, Ordering::Relaxed);
        } else {
            self.silent_samples
                .fetch_add(chunk.len() as u64, Ordering::Relaxed);
        }
    }

    /// 音の塊が来ないまま待った分を数える。
    fn note_gap(&self, gap: Duration) {
        let samples = gap.as_secs_f64() * SAMPLE_RATE as f64;
        self.silent_samples
            .fetch_add(samples as u64, Ordering::Relaxed);
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level_bits.load(Ordering::Relaxed))
    }

    fn set_level(&self, value: f32) {
        self.level_bits.store(value.to_bits(), Ordering::Relaxed);
    }

    /// 録音中の、声とみなす音の大きさ(実効値)。録音していなければ 0。
    pub fn threshold(&self) -> f32 {
        f32::from_bits(self.threshold_bits.load(Ordering::Relaxed))
    }

    fn set_threshold(&self, value: f32) {
        self.threshold_bits
            .store(value.to_bits(), Ordering::Relaxed);
    }

    /// 認識を待っている発話の数。
    pub fn pending(&self) -> u32 {
        self.pending.load(Ordering::Relaxed)
    }
}

/// 認識の設定。話している最中にも変えられるよう共有する。
pub type SharedOptions = Arc<Mutex<RecognizeOptions>>;

pub struct Session {
    pub live: Arc<LiveState>,
    jobs: Sender<Job>,
    events: Receiver<SessionEvent>,
    event_sender: Sender<SessionEvent>,
    feeder: Option<Sender<FeederCommand>>,
    /// 最後に始めた録音のスレッド。次の録音はこれが終わって(デバイスが閉じ切って)から開く。
    capture_thread: Option<JoinHandle<()>>,
    /// 録音を始めるたびに増える番号。
    generation: u64,
    source: Arc<dyn MicSource>,
    wake: Arc<dyn Fn() + Send + Sync>,
    options: SharedOptions,
    /// 認識エンジンが途中認識に対応しているか。
    supports_partial: bool,
    /// 声の判定モデル(Silero VAD)。録音ごとに判定を作って切り出しに付ける。無ければ音量だけで切り出す。
    detector: Option<Arc<VoiceDetector>>,
}

enum FeederCommand {
    Stop,
}

impl Session {
    pub fn new(
        recognizer: Option<Box<dyn Recognizer>>,
        options: SharedOptions,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (event_tx, event_rx) = mpsc::channel();
        let live = Arc::new(LiveState::default());
        let supports_partial = recognizer.as_ref().is_some_and(|r| r.supports_partial());
        spawn_worker(
            recognizer,
            job_rx,
            event_tx.clone(),
            options.clone(),
            live.clone(),
            wake.clone(),
        );
        Session {
            live,
            jobs: job_tx,
            events: event_rx,
            event_sender: event_tx,
            feeder: None,
            capture_thread: None,
            generation: 0,
            source: Arc::new(CpalMic),
            wake,
            options,
            supports_partial,
            detector: None,
        }
    }

    /// 声の判定モデルを付ける(次の録音から効く)。
    pub fn set_detector(&mut self, detector: Option<Arc<VoiceDetector>>) {
        self.detector = detector;
    }

    /// 声の判定を持っているか。
    pub fn has_detector(&self) -> bool {
        self.detector.is_some()
    }

    /// 認識エンジンが途中認識(話している途中の表示)に対応しているか。
    pub fn supports_partial(&self) -> bool {
        self.supports_partial
    }

    pub fn is_recording(&self) -> bool {
        self.feeder.is_some()
    }

    pub fn set_options(&self, options: RecognizeOptions) {
        if let Ok(mut shared) = self.options.lock() {
            *shared = options;
        }
    }

    /// 録音デバイスの入口。
    pub fn source(&self) -> &Arc<dyn MicSource> {
        &self.source
    }

    /// 録音デバイスの入口を差し替える(試験用)。
    pub fn set_source(&mut self, source: Arc<dyn MicSource>) {
        self.source = source;
    }

    /// エンジンの作り直しで次のセッションへ移るとき、閉じている途中の録音とデバイスの入口を引き継ぐ。
    pub fn hand_over_to(&mut self, next: &mut Session) {
        next.capture_thread = self.capture_thread.take();
        next.source = self.source.clone();
        next.detector = self.detector.clone();
    }

    /// 録音を始める。デバイスを開く処理は連係の接続で数秒かかるので裏のスレッドで行い、
    /// 結果は `MicOpened` か `MicFailed` で知らせる。戻り値はこの録音の番号。
    pub fn start(
        &mut self,
        device: Option<String>,
        config: SegmenterConfig,
        sensitivity: f32,
        gain: f32,
    ) -> u64 {
        if self.is_recording() {
            return self.generation;
        }
        self.generation += 1;
        let generation = self.generation;
        let (sample_tx, sample_rx) = mpsc::channel::<Vec<f32>>();
        let (command_tx, command_rx) = mpsc::channel();
        let previous = self.capture_thread.take();
        let source = self.source.clone();
        let events = self.event_sender.clone();
        let jobs = self.jobs.clone();
        let live = self.live.clone();
        let wake = self.wake.clone();
        let preview = self.supports_partial;
        let detector = self.detector.clone();
        self.capture_thread = Some(thread::spawn(move || {
            wait_until_closed(previous);
            if stop_requested(&command_rx) {
                return;
            }
            let began = Instant::now();
            let capture = match source.open(device, sample_tx) {
                Ok(capture) => capture,
                Err(message) => {
                    let _ = events.send(SessionEvent::MicFailed {
                        generation,
                        message,
                    });
                    wake();
                    return;
                }
            };
            // 開いている間に止められていたら、すぐ閉じる。
            if stop_requested(&command_rx) {
                return;
            }
            let took = began.elapsed();
            live.reset_signal();
            let _ = events.send(SessionEvent::MicOpened { generation, took });
            wake();
            let judge = detector.as_ref().and_then(|detector| match detector.start(None) {
                Ok(detection) => Some(Box::new(detection) as Box<dyn VoiceJudge>),
                Err(message) => {
                    diag::log(format!("声の判定を始められない: {message}"));
                    None
                }
            });
            feed(
                sample_rx,
                command_rx,
                jobs,
                live,
                wake,
                config,
                sensitivity,
                gain,
                preview,
                judge,
            );
            // 語尾まで録り終えてからマイクを閉じる。
            drop(capture);
        }));
        self.feeder = Some(command_tx);
        generation
    }

    /// 録音を止める。語尾が切れないよう少し録り続け、話している途中の音は認識に回す。
    pub fn stop(&mut self) {
        if let Some(feeder) = self.feeder.take() {
            let _ = feeder.send(FeederCommand::Stop);
        }
        self.live.set_level(0.0);
        self.live.set_threshold(0.0);
        self.live.speaking.store(false, Ordering::Relaxed);
    }

    pub fn recognize_file(&self, path: PathBuf) {
        let _ = self.jobs.send(Job::File(path));
    }

    /// 認識エンジンを先に準備しておく(最初の発話を待たせないため)。
    pub fn warm_up(&self) {
        let _ = self.jobs.send(Job::Prepare);
    }

    /// 録音を通さず発話の音を認識に回す(音声ファイルの試験用にも使う)。
    pub fn recognize_samples(&self, samples: Vec<f32>) {
        let _ = self.jobs.send(Job::Speech(samples));
    }

    /// 話している途中の音を途中認識に回す(試験と遅延の計測用)。
    pub fn recognize_partial_samples(&self, samples: Vec<f32>) {
        let _ = self.jobs.send(Job::Partial(samples));
    }

    pub fn poll(&self) -> Vec<SessionEvent> {
        self.events.try_iter().collect()
    }

    pub fn report(&self, message: String) {
        let _ = self.event_sender.send(SessionEvent::Error(message));
    }
}

/// 前の録音が閉じ切るまで待つ。同じ連係デバイスを二重に開くと再接続の邪魔になる。
fn wait_until_closed(previous: Option<JoinHandle<()>>) {
    let Some(previous) = previous else {
        return;
    };
    let limit = Instant::now() + CLOSE_WAIT;
    while !previous.is_finished() {
        if Instant::now() >= limit {
            diag::log("前の録音が閉じ切らないまま次を開く");
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = previous.join();
}

/// 録音を止める指示が来ているか。
fn stop_requested(commands: &Receiver<FeederCommand>) -> bool {
    !matches!(commands.try_recv(), Err(TryRecvError::Empty))
}

/// 途中認識に回す音を集める。話し始めの少し前から、話している間の音をためる。
struct Preview {
    /// 直近の音(話し始めの前に含める分)。
    preroll: VecDeque<f32>,
    /// いま話している発話の音。話していなければ None。
    buffer: Option<Vec<f32>>,
    /// 前回途中認識を送ってからの音の長さ(サンプル数)。
    since_sent: usize,
}

impl Preview {
    fn new() -> Self {
        Preview {
            preroll: VecDeque::new(),
            buffer: None,
            since_sent: 0,
        }
    }

    /// 1 塊の音を受け取る。話していれば途中認識を送る。
    fn push(&mut self, chunk: &[f32], speaking: bool, jobs: &Sender<Job>) {
        if speaking {
            match &mut self.buffer {
                Some(buffer) => buffer.extend_from_slice(chunk),
                None => {
                    let mut buffer: Vec<f32> = self.preroll.iter().copied().collect();
                    buffer.extend_from_slice(chunk);
                    self.buffer = Some(buffer);
                    self.since_sent = 0;
                }
            }
            self.since_sent += chunk.len();
            if let Some(buffer) = &self.buffer {
                if self.since_sent >= SAMPLE_RATE * PARTIAL_INTERVAL_MS / 1000
                    && buffer.len() >= SAMPLE_RATE * PARTIAL_MIN_MS / 1000
                {
                    self.since_sent = 0;
                    let _ = jobs.send(Job::Partial(buffer.clone()));
                }
            }
        } else {
            self.end(jobs);
        }
        let keep = SAMPLE_RATE * PARTIAL_PREROLL_MS / 1000;
        self.preroll.extend(chunk.iter().copied());
        while self.preroll.len() > keep {
            self.preroll.pop_front();
        }
    }

    /// 発話の終わり。ためた音を捨て、途中経過の表示を消させる。
    fn end(&mut self, jobs: &Sender<Job>) {
        if self.buffer.take().is_some() {
            self.since_sent = 0;
            let _ = jobs.send(Job::PartialEnd);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn feed(
    samples: Receiver<Vec<f32>>,
    commands: Receiver<FeederCommand>,
    jobs: Sender<Job>,
    live: Arc<LiveState>,
    wake: Arc<dyn Fn() + Send + Sync>,
    config: SegmenterConfig,
    sensitivity: f32,
    gain: f32,
    preview: bool,
    judge: Option<Box<dyn VoiceJudge>>,
) {
    let mut partial = preview.then(Preview::new);
    let tail = Duration::from_millis(config.post_forced_ms as u64);
    let mut segmenter = Segmenter::new(config, sensitivity).with_judge(judge);
    diag::log(match segmenter.judge_name() {
        Some(name) => format!("切り出し: 音量と {name}"),
        None => "切り出し: 音量だけ(声の判定モデルなし)".to_string(),
    });
    let interval = SAMPLE_RATE / 20;
    let mut since_wake = 0;
    let mut stop_at: Option<Instant> = None;
    let began = Instant::now();
    let mut first_chunk = true;
    let mut signal_seen = false;
    let mut was_speaking = false;
    loop {
        if stop_at.is_none() && commands.try_recv().is_ok() {
            stop_at = Some(Instant::now() + tail);
        }
        if stop_at.is_some_and(|at| Instant::now() >= at) {
            break;
        }
        let Ok(mut chunk) = samples.recv_timeout(SAMPLE_WAIT) else {
            if stop_at.is_none() {
                live.note_gap(SAMPLE_WAIT);
            }
            continue;
        };
        for sample in &mut chunk {
            *sample = (*sample * gain).clamp(-1.0, 1.0);
        }
        if stop_at.is_none() {
            live.set_level(rms(&chunk));
            live.note_chunk(&chunk);
        }
        if first_chunk {
            first_chunk = false;
            if !has_signal(&chunk) {
                diag::log("完全な無音が届いている(iPhone なら連係の接続待ち)");
            }
        }
        if !signal_seen && has_signal(&chunk) {
            signal_seen = true;
            diag::log(format!(
                "音が届いた(録音開始から {:.2} 秒)",
                began.elapsed().as_secs_f32()
            ));
        }
        let utterances = segmenter.push(&chunk);
        let had_utterances = !utterances.is_empty();
        if segmenter.is_speaking() != was_speaking {
            was_speaking = segmenter.is_speaking();
            if was_speaking {
                diag::log(format!(
                    "話し始め volume={} floor={} threshold={}",
                    segmenter.last_volume(),
                    segmenter.noise_floor(),
                    segmenter.effective_threshold()
                ));
            }
        }
        if stop_at.is_none() {
            live.set_threshold(segmenter.threshold_rms());
        }
        for utterance in utterances {
            let seconds = utterance.len() as f32 / SAMPLE_RATE as f32;
            let forced = utterance.len() * 1000 / SAMPLE_RATE >= config.max_ms;
            diag::log(format!(
                "切り出し {seconds:.2} 秒分{} floor={} threshold={}",
                if forced { "(上限で切断)" } else { "" },
                segmenter.noise_floor(),
                segmenter.effective_threshold()
            ));
            live.pending.fetch_add(1, Ordering::Relaxed);
            let _ = jobs.send(Job::Speech(utterance));
        }
        if let Some(partial) = &mut partial {
            if had_utterances {
                // 確定した発話を先に送ってから表示を消させる(確定結果が届くまで途中経過を残す)。
                partial.end(&jobs);
            } else {
                partial.push(&chunk, segmenter.is_speaking(), &jobs);
            }
        }
        if stop_at.is_none() {
            live.too_loud
                .store(segmenter.is_too_loud(), Ordering::Relaxed);
            live.speaking
                .store(segmenter.is_speaking(), Ordering::Relaxed);
        }
        since_wake += chunk.len();
        if since_wake >= interval {
            since_wake = 0;
            wake();
        }
    }
    if let Some(utterance) = segmenter.flush() {
        diag::log(format!(
            "切り出し(停止時) {:.2} 秒分",
            utterance.len() as f32 / SAMPLE_RATE as f32
        ));
        live.pending.fetch_add(1, Ordering::Relaxed);
        let _ = jobs.send(Job::Speech(utterance));
    }
    if let Some(partial) = &mut partial {
        partial.end(&jobs);
    }
    diag::log("録音停止");
    wake();
}

/// 途中認識を取り出したときに、待っている仕事を並べ直す。
/// 確定した発話などがあれば、それより前の途中認識は捨てる(確定を優先する)。その後の途中認識は最新の 1 件だけ残す。
fn arrange_after_partial(waiting: Vec<Job>) -> Vec<Job> {
    let last_final = waiting
        .iter()
        .rposition(|job| !matches!(job, Job::Partial(_)));
    let (before, after) = match last_final {
        Some(index) => {
            let mut waiting = waiting;
            let after = waiting.split_off(index + 1);
            (waiting, after)
        }
        None => (Vec::new(), waiting),
    };
    let mut arranged: Vec<Job> = before
        .into_iter()
        .filter(|job| !matches!(job, Job::Partial(_)))
        .collect();
    if let Some(newest) = after.into_iter().next_back() {
        arranged.push(newest);
    }
    arranged
}

/// 途中認識の記録(発話ごとに最初の 1 回だけ記録して、記録を小さく保つ)。
#[derive(Default)]
struct PartialLog {
    logged: bool,
    error_logged: bool,
}

fn spawn_worker(
    mut recognizer: Option<Box<dyn Recognizer>>,
    jobs: Receiver<Job>,
    events: Sender<SessionEvent>,
    options: SharedOptions,
    live: Arc<LiveState>,
    wake: Arc<dyn Fn() + Send + Sync>,
) {
    thread::spawn(move || {
        let mut backlog: VecDeque<Job> = VecDeque::new();
        let mut partial_log = PartialLog::default();
        loop {
            let job = match backlog.pop_front() {
                Some(job) => job,
                None => match jobs.recv() {
                    Ok(job) => job,
                    Err(_) => break,
                },
            };
            let job = match job {
                Job::Partial(samples) => {
                    let mut waiting = vec![Job::Partial(samples)];
                    waiting.extend(backlog.drain(..));
                    waiting.extend(jobs.try_iter());
                    let mut arranged = arrange_after_partial(waiting);
                    if arranged.len() == 1 && matches!(arranged[0], Job::Partial(_)) {
                        arranged.pop().expect("1 件ある")
                    } else {
                        backlog.extend(arranged);
                        continue;
                    }
                }
                job => job,
            };
            let current = options.lock().map(|o| o.clone());
            let Ok(current) = current else {
                continue;
            };
            let event = match (&mut recognizer, job) {
                (None, Job::Prepare) => continue,
                (Some(engine), Job::Prepare) => {
                    let began = Instant::now();
                    match engine.prepare() {
                        Ok(()) => {
                            diag::log(format!(
                                "認識エンジン準備 {:.2} 秒",
                                began.elapsed().as_secs_f32()
                            ));
                            continue;
                        }
                        Err(message) => {
                            diag::log(format!("認識エンジン準備エラー: {message}"));
                            SessionEvent::Error(message)
                        }
                    }
                }
                (_, Job::PartialEnd) => {
                    partial_log = PartialLog::default();
                    SessionEvent::Partial {
                        text: String::new(),
                    }
                }
                (None, Job::Partial(_)) => continue,
                (Some(engine), Job::Partial(samples)) => {
                    let began = Instant::now();
                    match engine.recognize_partial(&samples, &current) {
                        Ok(text) => {
                            if !partial_log.logged {
                                partial_log.logged = true;
                                diag::log(format!(
                                    "途中認識 {:.2} 秒分 → {:.2} 秒",
                                    samples.len() as f32 / SAMPLE_RATE as f32,
                                    began.elapsed().as_secs_f32()
                                ));
                            }
                            SessionEvent::Partial { text }
                        }
                        Err(message) => {
                            // 途中経過は表示だけのものなので、失敗しても知らせない。
                            if !partial_log.error_logged {
                                partial_log.error_logged = true;
                                diag::log(format!("途中認識エラー: {message}"));
                            }
                            continue;
                        }
                    }
                }
                (None, Job::Speech(_)) => {
                    partial_log = PartialLog::default();
                    live.pending.fetch_sub(1, Ordering::Relaxed);
                    SessionEvent::Error("認識エンジンが設定されていません".to_string())
                }
                (None, Job::File(_)) => {
                    SessionEvent::Error("認識エンジンが設定されていません".to_string())
                }
                (Some(engine), Job::Speech(samples)) => {
                    partial_log = PartialLog::default();
                    let began = Instant::now();
                    let result = engine.recognize(&samples, &current);
                    live.pending.fetch_sub(1, Ordering::Relaxed);
                    match &result {
                        Ok(recognition) => diag::log(format!(
                            "認識 {:.2} 秒分 → {:.2} 秒で {} 字",
                            samples.len() as f32 / SAMPLE_RATE as f32,
                            began.elapsed().as_secs_f32(),
                            recognition.text.chars().count()
                        )),
                        Err(message) => diag::log(format!("認識エラー: {message}")),
                    }
                    match result {
                        Ok(recognition) => SessionEvent::Recognized {
                            recognition,
                            audio: Arc::new(samples),
                        },
                        Err(message) => SessionEvent::Error(message),
                    }
                }
                (Some(engine), Job::File(path)) => match engine.recognize_file(&path, &current) {
                    Ok(recognition) => SessionEvent::FileRecognized { recognition },
                    Err(message) => SessionEvent::Error(message),
                },
            };
            let _ = events.send(event);
            wake();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// 決まった文を返すだけの認識エンジン。
    struct Fixed;

    impl Recognizer for Fixed {
        fn name(&self) -> String {
            "fixed".to_string()
        }
        fn recognize(
            &mut self,
            _samples: &[f32],
            _options: &RecognizeOptions,
        ) -> Result<Recognition, String> {
            Ok(Recognition {
                text: "途中です".to_string(),
                unsure: Vec::new(),
            })
        }
        fn recognize_file(
            &mut self,
            _path: &Path,
            _options: &RecognizeOptions,
        ) -> Result<Recognition, String> {
            Err("使わない".to_string())
        }
        fn supports_partial(&self) -> bool {
            true
        }
    }

    fn options() -> SharedOptions {
        Arc::new(Mutex::new(RecognizeOptions {
            words: Vec::new(),
            readings: std::collections::HashMap::new(),
        }))
    }

    #[test]
    fn silence_counts_zero_chunks_and_gaps_until_sound_returns() {
        let live = LiveState::default();
        live.note_chunk(&[0.1; 160]);
        assert!(live.has_signal());
        assert_eq!(live.silence(), Duration::ZERO);
        live.note_chunk(&vec![0.0; SAMPLE_RATE]);
        live.note_gap(Duration::from_secs(1));
        assert_eq!(live.silence(), Duration::from_secs(2));
        live.note_chunk(&[0.1; 160]);
        assert_eq!(live.silence(), Duration::ZERO);
        live.reset_signal();
        assert!(!live.has_signal());
    }

    #[test]
    fn partial_samples_produce_a_partial_event() {
        let session = Session::new(Some(Box::new(Fixed)), options(), Arc::new(|| {}));
        assert!(session.supports_partial());
        session.recognize_partial_samples(vec![0.1; SAMPLE_RATE]);
        let began = Instant::now();
        let mut text = None;
        while text.is_none() && began.elapsed() < Duration::from_secs(2) {
            for event in session.poll() {
                if let SessionEvent::Partial { text: t } = event {
                    text = Some(t);
                }
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(text.as_deref(), Some("途中です"));
        assert_eq!(session.live.pending(), 0, "途中認識は待ち数を変えない");
    }

    /// 手放されたら印をつける認識エンジン。
    struct Tracked(Arc<AtomicBool>);

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl Recognizer for Tracked {
        fn name(&self) -> String {
            "tracked".to_string()
        }
        fn recognize(
            &mut self,
            _samples: &[f32],
            _options: &RecognizeOptions,
        ) -> Result<Recognition, String> {
            Ok(Recognition::default())
        }
        fn recognize_file(
            &mut self,
            _path: &Path,
            _options: &RecognizeOptions,
        ) -> Result<Recognition, String> {
            Err("使わない".to_string())
        }
    }

    #[test]
    fn dropping_the_session_drops_the_engine_on_its_worker() {
        let dropped = Arc::new(AtomicBool::new(false));
        let session = Session::new(
            Some(Box::new(Tracked(dropped.clone()))),
            options(),
            Arc::new(|| {}),
        );
        drop(session);
        let began = Instant::now();
        while !dropped.load(Ordering::SeqCst) && began.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(dropped.load(Ordering::SeqCst), "切り替えでもエンジンは手放される");
    }

    #[test]
    fn no_recognizer_means_no_partial_support() {
        let session = Session::new(None, options(), Arc::new(|| {}));
        assert!(!session.supports_partial());
    }

    #[test]
    fn finals_win_over_queued_partials() {
        let waiting = vec![
            Job::Partial(vec![1.0]),
            Job::Partial(vec![2.0]),
            Job::Speech(vec![3.0]),
            Job::PartialEnd,
            Job::Partial(vec![4.0]),
            Job::Partial(vec![5.0]),
        ];
        let arranged = arrange_after_partial(waiting);
        assert_eq!(arranged.len(), 3);
        assert!(matches!(arranged[0], Job::Speech(_)));
        assert!(matches!(arranged[1], Job::PartialEnd));
        assert!(matches!(&arranged[2], Job::Partial(s) if s == &vec![5.0]));
        let only = arrange_after_partial(vec![Job::Partial(vec![1.0]), Job::Partial(vec![2.0])]);
        assert!(matches!(&only[..], [Job::Partial(s)] if s == &vec![2.0]));
    }

    #[test]
    fn preview_sends_partials_while_speaking_and_ends_after() {
        let (tx, rx) = mpsc::channel();
        let mut preview = Preview::new();
        let chunk = vec![0.1f32; SAMPLE_RATE / 10];
        for _ in 0..5 {
            preview.push(&chunk, false, &tx);
        }
        for _ in 0..20 {
            preview.push(&chunk, true, &tx);
        }
        preview.push(&chunk, false, &tx);
        let jobs: Vec<Job> = rx.try_iter().collect();
        let partials: Vec<usize> = jobs
            .iter()
            .filter_map(|job| match job {
                Job::Partial(s) => Some(s.len()),
                _ => None,
            })
            .collect();
        // 300 ms の前置きを含めて 1.0 秒たまったら送り、その後は 0.5 秒ごと。
        assert_eq!(
            partials,
            vec![SAMPLE_RATE, SAMPLE_RATE * 3 / 2, SAMPLE_RATE * 2]
        );
        assert!(matches!(jobs.last(), Some(Job::PartialEnd)));
    }
}
