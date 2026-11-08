//! 発話の切り出し。音量で話し始めと話し終わりを判定する。
//! 時間の値・音量の式・音割れの判定の数値は、このアプリの既定値。
//!
//! 音量は「自分の声か(近いか)」は分かっても「声か物音か」は分からない。そこで声の判定役(`VoiceJudge`、
//! 実体は speech.cpp の Silero VAD)を付けられる。判定役があれば、話し始めは音量と判定役の両方が声と言ったとき、
//! 話し終わりはどちらかが声でなくなって無音の長さを数えたときになる。判定役が無ければ音量だけで今まで通り動く。

pub const SAMPLE_RATE: usize = 16_000;
/// 音量を計算する単位(サンプリング周波数 × 20 / 1000)。
const FRAME_MS: usize = 20;
const FRAME_SAMPLES: usize = SAMPLE_RATE * FRAME_MS / 1000;

/// 声とみなす音量の基準。音量の単位(0〜32767 の対数目盛)。
const VOLUME_THRESHOLD: i32 = 3500;
const MAX_VOLUME: i32 = 0x7FFF;
const VOLUME_LOG_SCALE: f64 = 4096.0;
const VOLUME_POWER_SCALE: f64 = 81.92;
/// 感度から増幅率を求める式の基準値と係数(このアプリの既定値)。
const VOLUME_FACTOR_BASE: f64 = 400.0;
const VOLUME_FACTOR_COEFFICIENT: f64 = 4.0;
/// しきい値に足される騒音分。固定で使う。
const NOISE_OFFSET: i32 = 512;
/// 声とみなす音量の揺れの基準(しきい値に対する百分率)。
const MV_THRESHOLD: i32 = 25;
/// 音量の揺れを見るフレーム数。
const MV_WINDOW_FRAMES: usize = 10;
/// 判定器の立ち上がりと立ち下がりのフレーム数。
const ATTACK_FRAMES: u16 = 3;
const RELEASE_FRAMES: u16 = 30;
/// 音割れとみなす既定の振幅。
const CLIP_DEFAULT_LIMIT: i32 = 0x7D00;
/// 波形の山の 1/32 以内を「張り付き」とみなす。
const CLIP_PEAK_DIVISOR: i32 = 32;
/// 張り付きの数 × 320 がサンプリング周波数を超えたら大きすぎる。
const CLIP_COUNT_FACTOR: f32 = 320.0;

// ---- 騒音の床への追従 ----
//
// 固定の基準(3500 + 騒音分 512)はヘッドセット向けで、ノート PC 内蔵マイクや iPhone の室内雑音
// (音量目盛で 8000〜13000)を常に超えてしまい、話し終わりが検出されなかった(本機で実測)。
// そこで雑音の床を追い続け、床よりパワーで 6 倍(約 +7.8dB)大きい音だけを声として扱う。

/// 声とみなすのに必要な、雑音の水準に対するパワーの比。
const SPEECH_OVER_NOISE_RATIO: f64 = 6.0;

/// 感度(入力の増幅率の元)。設定からは 2026-11-08 に外し、中央の値に固定した。
/// 声の判定役と雑音の床の追従が入ってからは、静かな部屋での最低の基準をわずかに動かすだけで、
/// マイクの増幅と役目が重なる。
pub const DEFAULT_SENSITIVITY: f32 = 0.5;

/// 雑音の水準の追従の速さ(1 フレームあたり)。無音状態でだけ動かし、下がる方を速くして
/// 話し始めの数フレームで上がった分をすぐ戻す。発話中は凍結する(声に追いついて切れないように)。
const NOISE_FALL_RATE: f32 = 0.1;
const NOISE_RISE_RATE: f32 = 0.02;
/// 録音を始めた直後、水準を素早く合わせる間のフレーム数(300ms)。この間は話し始めを判定しない。
const WARMUP_FRAMES: usize = 15;
const WARMUP_RATE: f32 = 0.3;
/// これより小さい音量は「音がまだ届いていない」とみなす(連係の iPhone はつながるまで完全な無音を届ける)。
/// 実際のマイクは静かな部屋でも増幅後 7000 以上になる(本機で実測)ので、十分に離れている。
const DEAD_VOLUME: i32 = 64;
/// 判定役が声と確かめるまでの遅れ(Silero の min_speech_duration_ms に音量の立ち上がりを足した分)に備えて、
/// 話し始めの前の音を余分に保つ長さ。切り出しは話し始めの `pre_ms` 前から始める。
const JUDGE_LAG_MS: usize = 1000;
/// 判定役が声と言っている間に見つけた話し始め(`gate_onset`)を、音量が基準を下回り続けたらこれより後に忘れる。
/// 語句の間(0.3〜0.5 秒)はまたぐが、離れた人の声が続く中で基準を超えた物音を 1 秒後の声の始まりに結びつけない。
const GATE_GAP_MS: usize = 500;

/// 判定役が 1 フレームの音を聞いた答え。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Verdict {
    /// 声が続いている(直近の短い無音を含む)。
    pub speaking: bool,
    /// 続いている声が十分な長さになり、短い物音ではないと確かめられた。
    pub kept: bool,
    /// いま確定した声の区間の終わり(判定役に聞かせた音の先頭からの秒)。判定役は声の終わりを
    /// 少し遅れて確定するので、切り出しはこの時刻から無音を数え直す。
    pub ended: Option<f64>,
}

impl Verdict {
    /// 判定役が無い(または答えられない)ときの答え。音量だけで判定する。
    pub const UNJUDGED: Verdict = Verdict {
        speaking: true,
        kept: true,
        ended: None,
    };
}

/// 音が声かどうかを判定する役(Silero VAD など)。20 ms のフレームを順に聞かせる。
/// 診断用: 直近のフレームの判定の内訳(`--segment-check --trace`)。
#[derive(Debug, Clone, Copy)]
pub struct FrameTrace {
    pub volume: i32,
    pub threshold: i32,
    pub volume_active: bool,
    pub movement_active: bool,
    pub verdict: Verdict,
    pub loud: bool,
    pub dead: bool,
}

pub trait VoiceJudge: Send {
    fn hear(&mut self, frame: &[f32]) -> Verdict;
    fn name(&self) -> String;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SegmenterConfig {
    /// 話し終わりと判断する無音の長さ。
    pub post_ms: usize,
    /// マイクを切ったときに残す長さ。
    pub post_forced_ms: usize,
    /// 話し始めの前に含める長さ。
    pub pre_ms: usize,
    /// 話し始めと判断する連続音の長さ。
    pub speech_start_min_ms: usize,
    /// これより短い発話は捨てる。
    pub min_ms: usize,
    /// 認識エンジンの 1 回の上限に合わせた最大長。
    pub max_ms: usize,
    /// 声とみなすのに必要な、雑音の水準に対するパワーの比(既定は SPEECH_OVER_NOISE_RATIO)。
    pub speech_ratio: f64,
}

impl Default for SegmenterConfig {
    fn default() -> Self {
        SegmenterConfig {
            post_ms: 800,
            post_forced_ms: 600,
            pre_ms: 600,
            speech_start_min_ms: 50,
            min_ms: 100,
            max_ms: 25_000,
            speech_ratio: SPEECH_OVER_NOISE_RATIO,
        }
    }
}

/// 感度から入力の増幅率(100 倍の値)を求める。
/// 基準値 × 2^((2 × 感度 − 1) × 係数)。感度が負なら 0.5、1 より上なら 1 として扱う。
pub fn volume_factor100(sensitivity: f32) -> i32 {
    let sensitivity = if sensitivity < 0.0 {
        0.5
    } else {
        f64::from(sensitivity).min(1.0)
    };
    let exponent = (2.0 * sensitivity - 1.0) * VOLUME_FACTOR_COEFFICIENT;
    (VOLUME_FACTOR_BASE * 2f64.powf(exponent)) as i32
}

/// 1 フレームの音量: 4096 × ln(1 + 平均パワー × 増幅率 × 81.92)。
fn volume_from_power(mean_power: f64, factor100: i32) -> i32 {
    let value = (1.0 + mean_power * f64::from(factor100) * VOLUME_POWER_SCALE).ln();
    ((value * VOLUME_LOG_SCALE) as i32).min(MAX_VOLUME)
}

/// 音量の目盛の値を 0.0〜1.0 の実効値に戻す(`volume_from_power` の逆)。
fn rms_for_volume(volume: i32, factor100: i32) -> f32 {
    let power = ((f64::from(volume) / VOLUME_LOG_SCALE).exp() - 1.0)
        / (f64::from(factor100) * VOLUME_POWER_SCALE);
    power.max(0.0).sqrt() as f32
}

/// 静かな場所で話し始めと判断する音の大きさを 0.0〜1.0 の実効値で返す(メーターの印に使う)。
/// 録音中は雑音の床に応じて上がるので、`Segmenter::threshold_rms` を使う。
pub fn threshold_for_sensitivity(sensitivity: f32) -> f32 {
    rms_for_volume(VOLUME_THRESHOLD, volume_factor100(sensitivity))
}

/// 雑音の床の上に取る余裕(音量目盛)。パワー比 `ratio` を対数目盛に直したもの。
fn speech_margin(ratio: f64) -> i32 {
    (VOLUME_LOG_SCALE * ratio.ln()) as i32
}

/// 値が基準を超えたかを 4 段階で追う判定器。
#[derive(Debug, Clone, Copy, PartialEq)]
enum DetectorState {
    Off,
    Rising,
    On,
    Falling,
}

#[derive(Debug)]
struct Detector {
    threshold: i32,
    state: DetectorState,
    rise: u16,
    fall: u16,
}

impl Detector {
    fn new(threshold: i32) -> Self {
        Detector {
            threshold,
            state: DetectorState::Off,
            rise: 0,
            fall: 0,
        }
    }

    fn update(&mut self, value: i32) {
        let above = value >= self.threshold;
        match self.state {
            DetectorState::Off if above => {
                self.state = DetectorState::Rising;
                self.rise = 0;
                self.fall = 0;
            }
            DetectorState::Off => {}
            DetectorState::Rising if above => {
                self.rise += 1;
                if self.rise >= ATTACK_FRAMES {
                    self.state = DetectorState::On;
                    self.rise = 0;
                    self.fall = 0;
                }
            }
            DetectorState::Rising => {
                self.fall += 1;
                if self.fall >= ATTACK_FRAMES {
                    self.state = DetectorState::Off;
                    self.rise = 0;
                    self.fall = 0;
                }
            }
            DetectorState::On if !above => self.state = DetectorState::Falling,
            DetectorState::On => {}
            DetectorState::Falling if above => {
                self.state = DetectorState::On;
                self.fall = 0;
            }
            DetectorState::Falling => {
                self.fall += 1;
                if self.fall >= RELEASE_FRAMES {
                    self.state = DetectorState::Off;
                    self.fall = 0;
                }
            }
        }
    }

    /// 話し始めの判定に使う状態か。
    fn is_active(&self) -> bool {
        matches!(self.state, DetectorState::Rising | DetectorState::On)
    }

    fn set_threshold(&mut self, threshold: i32) {
        self.threshold = threshold;
    }

    fn is_off(&self) -> bool {
        self.state == DetectorState::Off
    }
}

/// 直近の音量の標準偏差を、しきい値に対する百分率で返す。
/// 一定の騒音は音量が大きくても揺れないので、話し声と区別できる。
fn volume_movement(history: &std::collections::VecDeque<i32>) -> i32 {
    let count = history.len().max(1) as i64;
    let mean = history.iter().map(|v| i64::from(*v)).sum::<i64>() / count;
    let variance: i64 = history
        .iter()
        .map(|v| (i64::from(*v) - mean).pow(2) / count)
        .sum();
    (variance as f64).sqrt() as i32 * 100 / (VOLUME_THRESHOLD + 1)
}

/// 音量の計算と音割れの判定。直流成分は前のフレームの平均を引く。
#[derive(Debug, Default)]
struct LevelMeter {
    offset: i32,
    peak: i32,
    clip_count: f32,
}

impl LevelMeter {
    fn clip_limit(&self) -> i32 {
        let limit = self.peak - self.peak / CLIP_PEAK_DIVISOR;
        if limit < VOLUME_THRESHOLD {
            CLIP_DEFAULT_LIMIT
        } else {
            limit
        }
    }

    /// 1 フレームを処理して音量を返す。
    fn process(&mut self, frame: &[f32], factor100: i32) -> i32 {
        let mut power = 0f64;
        let mut sum = 0i64;
        let mut limit = self.clip_limit();
        for sample in frame {
            let value = (sample * 32768.0) as i32;
            sum += i64::from(value);
            let magnitude = (value - self.offset).abs();
            if magnitude >= limit {
                self.clip_count += 1.0;
            }
            if magnitude > self.peak {
                // 山が高くなったら、これまでの数を (古い山 / 新しい山)^3 に縮める。
                let ratio = self.peak as f32 / magnitude as f32;
                self.clip_count *= ratio * ratio * ratio;
                self.peak = magnitude;
                limit = self.clip_limit();
            }
            let scaled = f64::from(magnitude) / 32768.0;
            power += scaled * scaled;
        }
        self.offset = (sum / frame.len().max(1) as i64) as i32;
        self.peak /= 2;
        volume_from_power(power / frame.len().max(1) as f64, factor100)
    }

    fn is_clipping(&self) -> bool {
        self.clip_count * CLIP_COUNT_FACTOR > SAMPLE_RATE as f32
    }
}

#[derive(Debug, PartialEq)]
enum State {
    Silence,
    Speech,
}

pub struct Segmenter {
    config: SegmenterConfig,
    factor100: i32,
    meter: LevelMeter,
    volume_detector: Detector,
    movement_detector: Detector,
    history: std::collections::VecDeque<i32>,
    state: State,
    /// 声かどうかの判定役。無ければ音量だけで判定する。
    judge: Option<Box<dyn VoiceJudge>>,
    /// 処理した音の長さ(サンプル)。判定役の時刻(秒)をこの目盛に直して突き合わせる。
    pushed: usize,
    /// 話し始めの前の音を保つ。
    pre_buffer: Vec<f32>,
    /// `pre_buffer` の先頭が全体の何サンプル目か。
    pre_buffer_start: usize,
    current: Vec<f32>,
    /// `current` の先頭が全体の何サンプル目か。
    utterance_start: usize,
    /// `current` の先頭のうち、話し始めより前の音の長さ。
    lead_samples: usize,
    /// 判定役が声と言っている間に、音量が基準を連続して超えている長さ(ms)。下回ったら 0 に戻す。
    gate_run_ms: usize,
    /// `gate_run_ms` を数え始めた位置(全体のサンプル番号)。
    gate_run_from: usize,
    /// 判定役が声と言っている間に、音量が `speech_start_min_ms` 連続して基準を超えた最初の位置(話し始め)。
    /// 話し始めは、判定役が声と確かめた(kept)時点でこれがあれば良い(その瞬間の音量は見ない)。
    /// 声でなくなったか、基準を `GATE_GAP_MS` より長く下回ったら忘れる。
    gate_onset: Option<usize>,
    /// `gate_onset` があり、音量が基準を下回り続けている長さ(ms)。
    gate_gap_ms: usize,
    last_frame: Option<FrameTrace>,
    pending: Vec<f32>,
    voiced_ms: usize,
    silent_ms: usize,
    /// 雑音の水準(音量目盛)。無音状態の音量を平滑化したもの。
    noise_floor: f32,
    /// 処理したフレーム数。録音直後の合わせ込みに使う。
    frames: usize,
    /// 今の発話の中で最も静かだった音量。上限で切れたときの床の立て直しに使う。
    quietest: i32,
}

impl Segmenter {
    pub fn new(config: SegmenterConfig, sensitivity: f32) -> Self {
        Segmenter {
            config,
            factor100: volume_factor100(sensitivity),
            meter: LevelMeter::default(),
            volume_detector: Detector::new(VOLUME_THRESHOLD + NOISE_OFFSET),
            movement_detector: Detector::new(MV_THRESHOLD),
            history: std::collections::VecDeque::with_capacity(MV_WINDOW_FRAMES + 1),
            state: State::Silence,
            judge: None,
            pushed: 0,
            pre_buffer: Vec::new(),
            pre_buffer_start: 0,
            current: Vec::new(),
            utterance_start: 0,
            lead_samples: 0,
            gate_run_ms: 0,
            gate_run_from: 0,
            gate_onset: None,
            gate_gap_ms: 0,
            last_frame: None,
            pending: Vec::new(),
            voiced_ms: 0,
            silent_ms: 0,
            noise_floor: NOISE_OFFSET as f32,
            frames: 0,
            quietest: MAX_VOLUME,
        }
    }

    /// 声の判定役を付ける。None なら音量だけで判定する。
    pub fn with_judge(mut self, judge: Option<Box<dyn VoiceJudge>>) -> Self {
        self.judge = judge;
        self
    }

    /// 付いている判定役の名前。
    pub fn judge_name(&self) -> Option<String> {
        self.judge.as_ref().map(|judge| judge.name())
    }

    /// 話し始めの前に保つ長さ(サンプル)。判定役があれば、声と確かめるまでの遅れの分も保つ。
    fn pre_keep_samples(&self) -> usize {
        let lag = if self.judge.is_some() { JUDGE_LAG_MS } else { 0 };
        (self.config.pre_ms + lag) * SAMPLE_RATE / 1000
    }

    /// いま声とみなす音量の基準。固定の基準(3500 + 512)か、雑音の水準 + 余裕の大きい方。
    pub fn effective_threshold(&self) -> i32 {
        (VOLUME_THRESHOLD + NOISE_OFFSET)
            .max(self.noise_floor as i32 + speech_margin(self.config.speech_ratio))
    }

    /// 雑音の水準(音量目盛)。診断用。
    pub fn noise_floor(&self) -> i32 {
        self.noise_floor as i32
    }

    /// いまの基準を 0.0〜1.0 の実効値で返す(メーターの印に使う)。
    pub fn threshold_rms(&self) -> f32 {
        rms_for_volume(self.effective_threshold(), self.factor100)
    }

    /// 雑音の水準を更新する。無音状態の音量を平滑化して追い、発話中は動かさない。
    /// 最小値を追うと雑音の山のたびに無音の数え直しが起きて話し終わりが遅れるので、平均を追う。
    fn track_noise(&mut self, volume: i32) {
        if self.state == State::Speech {
            return;
        }
        let value = volume as f32;
        let rate = if self.frames < WARMUP_FRAMES {
            WARMUP_RATE
        } else if value < self.noise_floor {
            NOISE_FALL_RATE
        } else {
            NOISE_RISE_RATE
        };
        self.noise_floor += (value - self.noise_floor) * rate;
    }

    pub fn is_speaking(&self) -> bool {
        self.state == State::Speech
    }

    /// 直近のフレームの判定の内訳。診断用。
    pub fn last_frame(&self) -> Option<FrameTrace> {
        self.last_frame
    }

    /// 直近フレームの音量(0〜32767 の対数目盛)。診断用。
    pub fn last_volume(&self) -> i32 {
        self.history.back().copied().unwrap_or(0)
    }

    /// 直近の音量の揺れ(しきい値に対する百分率)。診断用。
    pub fn last_movement(&self) -> i32 {
        volume_movement(&self.history)
    }

    /// 今の発話で音割れが起きているか(「声が大きすぎます」)。
    pub fn is_too_loud(&self) -> bool {
        self.meter.is_clipping()
    }

    /// 音を入れる。発話が完成したら返す。
    pub fn push(&mut self, samples: &[f32]) -> Vec<Vec<f32>> {
        let mut done = Vec::new();
        self.pending.extend_from_slice(samples);
        let mut offset = 0;
        while offset + FRAME_SAMPLES <= self.pending.len() {
            let frame: Vec<f32> = self.pending[offset..offset + FRAME_SAMPLES].to_vec();
            offset += FRAME_SAMPLES;
            if let Some(utterance) = self.push_frame(&frame) {
                done.push(utterance);
            }
        }
        self.pending.drain(..offset);
        done
    }

    fn push_frame(&mut self, frame: &[f32]) -> Option<Vec<f32>> {
        let frame_start = self.pushed;
        self.pushed += frame.len();
        // 判定役には音が届いていない間も聞かせる(時刻の目盛を合わせる)。
        let verdict = match self.judge.as_mut() {
            Some(judge) => judge.hear(frame),
            None => Verdict::UNJUDGED,
        };
        let volume = self.meter.process(frame, self.factor100);
        if self.state == State::Silence && volume < DEAD_VOLUME {
            // 音がまだ届いていない。ここで床を合わせると、音が届いた瞬間に部屋の雑音が声に見える
            // (iPhone で起きた不具合)ので、合わせ込みも判定もせずに待つ。音そのものは話し始めの前の音として
            // 残す(位置の計算が続くように。判定役があると声の始まりから確かめるまで 0.2 秒ほど Silence のままで、
            // その間に 1 フレームでも途切れがあったとき、溜めた話し始めを失わないため)。
            self.history.clear();
            self.keep_before_speech(frame, frame_start);
            self.gate(false, verdict.speaking, frame_start);
            self.last_frame = Some(FrameTrace {
                volume,
                threshold: self.effective_threshold(),
                volume_active: false,
                movement_active: false,
                verdict,
                loud: false,
                dead: true,
            });
            return None;
        }
        self.history.push_back(volume);
        if self.history.len() > MV_WINDOW_FRAMES {
            self.history.pop_front();
        }
        self.track_noise(volume);
        let warming_up = self.frames < WARMUP_FRAMES;
        self.frames += 1;
        let threshold = self.effective_threshold();
        self.volume_detector.set_threshold(threshold);
        self.volume_detector.update(volume);
        self.movement_detector
            .update(volume_movement(&self.history));
        // 話し始めは音量と揺れの両方を見る。発話中は揺れが消えたら音なしとして扱う。
        // 判定役があれば、どちらも判定役が声と言っていることも要る。
        let loud = match self.state {
            State::Silence => {
                !warming_up
                    && self.volume_detector.is_active()
                    && self.movement_detector.is_active()
                    && verdict.speaking
            }
            State::Speech => {
                volume >= threshold && !self.movement_detector.is_off() && verdict.speaking
            }
        };
        self.last_frame = Some(FrameTrace {
            volume,
            threshold,
            volume_active: self.volume_detector.is_active(),
            movement_active: self.movement_detector.is_active(),
            verdict,
            loud,
            dead: false,
        });
        match self.state {
            State::Silence => {
                self.keep_before_speech(frame, frame_start);
                self.voiced_ms = if loud { self.voiced_ms + FRAME_MS } else { 0 };
                self.gate(loud, verdict.speaking, frame_start);
                if self.judge.is_none() {
                    if self.voiced_ms >= self.config.speech_start_min_ms {
                        let onset = self.pushed.saturating_sub(self.voiced_ms * SAMPLE_RATE / 1000);
                        self.begin_utterance(onset);
                    }
                } else if verdict.kept {
                    // 判定役が声と確かめる(kept)のは声の始まりから 0.2 秒ほど後。その瞬間に音量の判定が
                    // 一息ついていても、区間の中で基準を超えた話し始めがあれば始める。短い物音はここで捨てる。
                    if let Some(onset) = self.gate_onset {
                        self.begin_utterance(onset);
                    }
                }
                None
            }
            State::Speech => {
                self.current.extend_from_slice(frame);
                self.quietest = self.quietest.min(volume);
                self.silent_ms = if loud { 0 } else { self.silent_ms + FRAME_MS };
                if let Some(ended) = verdict.ended {
                    // 判定役は声の終わりを少し遅れて確定する。その終わりから無音を数え直す。
                    let ended = (ended * SAMPLE_RATE as f64) as usize;
                    if ended >= self.utterance_start && ended < self.pushed {
                        let since_ms = (self.pushed - ended) * 1000 / SAMPLE_RATE;
                        self.silent_ms = self.silent_ms.max(since_ms);
                    }
                }
                let length_ms = self.current.len() * 1000 / SAMPLE_RATE;
                if length_ms >= self.config.max_ms {
                    // 上限まで終わらなかったのは、床が低すぎて雑音を声と見ている疑いが強い。
                    // 発話の中で最も静かだった音量(声の合間の雑音)まで床を上げて立て直す。
                    self.noise_floor = self.noise_floor.max(self.quietest as f32);
                    self.finish_utterance()
                } else if self.silent_ms >= self.config.post_ms {
                    self.finish_utterance()
                } else {
                    None
                }
            }
        }
    }

    /// 判定役があるときの話し始め(`gate_onset`)を追う。音量だけの経路と同じく `speech_start_min_ms` 連続して
    /// 基準を超えたら話し始めとし、判定役が声と確かめるまで覚えておく。
    fn gate(&mut self, loud: bool, speaking: bool, frame_start: usize) {
        if !speaking {
            self.gate_run_ms = 0;
            self.gate_onset = None;
            return;
        }
        if loud {
            if self.gate_run_ms == 0 {
                self.gate_run_from = frame_start;
            }
            self.gate_run_ms += FRAME_MS;
            self.gate_gap_ms = 0;
            if self.gate_onset.is_none() && self.gate_run_ms >= self.config.speech_start_min_ms {
                self.gate_onset = Some(self.gate_run_from);
            }
        } else {
            self.gate_run_ms = 0;
            if self.gate_onset.is_some() {
                self.gate_gap_ms += FRAME_MS;
                if self.gate_gap_ms > GATE_GAP_MS {
                    self.gate_onset = None;
                }
            }
        }
    }

    /// 話し始めの前の音を保つ。`pre_buffer` は常に `pushed` の直前まで続いた音(先頭は `pre_buffer_start`)。
    fn keep_before_speech(&mut self, frame: &[f32], frame_start: usize) {
        if self.pre_buffer.is_empty() {
            self.pre_buffer_start = frame_start;
        }
        self.pre_buffer.extend_from_slice(frame);
        let keep = self.pre_keep_samples();
        if self.pre_buffer.len() > keep {
            let excess = self.pre_buffer.len() - keep;
            self.pre_buffer.drain(..excess);
            self.pre_buffer_start += excess;
        }
    }

    /// 話し始め。`onset`(音量が基準を超え始めた位置)の `pre_ms` 前からの音を `current` に移す。
    /// 時刻は音量から取る(判定役の区間の始まりは、離れた人の声から続いていることがある)。
    fn begin_utterance(&mut self, onset: usize) {
        self.state = State::Speech;
        self.gate_run_ms = 0;
        self.gate_onset = None;
        self.gate_gap_ms = 0;
        let buffer = std::mem::take(&mut self.pre_buffer);
        let buffer_start = self.pre_buffer_start;
        let onset = onset.clamp(buffer_start, self.pushed);
        let pre = self.config.pre_ms * SAMPLE_RATE / 1000;
        let from = onset.saturating_sub(pre).max(buffer_start) - buffer_start;
        self.current = buffer[from..].to_vec();
        self.utterance_start = buffer_start + from;
        self.lead_samples = onset - self.utterance_start;
        self.silent_ms = 0;
        self.quietest = MAX_VOLUME;
        // 音割れの数は発話ごとに数え直す。
        self.meter.clip_count = 0.0;
    }

    fn finish_utterance(&mut self) -> Option<Vec<f32>> {
        let trailing_ms = self.silent_ms;
        self.state = State::Silence;
        self.voiced_ms = 0;
        self.silent_ms = 0;
        let audio = std::mem::take(&mut self.current);
        // 話し始めの前の音と、話し終わりの無音を除いた長さで判定する。
        let recorded_ms = audio.len().saturating_sub(self.lead_samples) * 1000 / SAMPLE_RATE;
        let speech_ms = recorded_ms.saturating_sub(trailing_ms);
        if speech_ms >= self.config.min_ms {
            return Some(audio);
        }
        // 短い物音だった。すぐ後に声が始まっても話し始めの前の音を欠かないよう、終わりの分を保っておく。
        let keep = self.pre_keep_samples().min(audio.len());
        self.pre_buffer_start = self.utterance_start + audio.len() - keep;
        self.pre_buffer = audio[audio.len() - keep..].to_vec();
        None
    }

    /// マイクを切ったとき、話している途中の音を締めて返す。
    pub fn flush(&mut self) -> Option<Vec<f32>> {
        self.pending.clear();
        self.pre_buffer.clear();
        if self.state == State::Speech {
            self.finish_utterance()
        } else {
            None
        }
    }
}

pub fn rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(ms: usize, amplitude: f32) -> Vec<f32> {
        (0..ms * SAMPLE_RATE / 1000)
            .map(|i| amplitude * (i as f32 * 0.3).sin())
            .collect()
    }

    fn silence(ms: usize) -> Vec<f32> {
        vec![0.0; ms * SAMPLE_RATE / 1000]
    }

    /// 静かな部屋のマイク。完全な 0 ではなく、わずかな雑音がある(実際のマイクは必ずそうなる)。
    fn quiet_room(ms: usize) -> Vec<f32> {
        noise(ms, 0.002)
    }

    /// 話し声のように音量が上下する音(0.25 秒ごとに大小が入れ替わる)。
    fn speech(ms: usize) -> Vec<f32> {
        (0..ms * SAMPLE_RATE / 1000)
            .map(|i| {
                let loud = (i / (SAMPLE_RATE / 4)).is_multiple_of(2);
                (if loud { 0.3 } else { 0.03 }) * (i as f32 * 0.3).sin()
            })
            .collect()
    }

    #[test]
    fn steady_noise_never_starts_speech() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        assert!(segmenter.push(&noise(3000, 0.3)).is_empty());
        assert!(!segmenter.is_speaking());
    }

    #[test]
    fn hum_that_appears_suddenly_ends_on_its_own() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        segmenter.push(&silence(500));
        segmenter.push(&noise(4000, 0.3));
        assert!(!segmenter.is_speaking());
    }

    #[test]
    fn detector_needs_three_frames_to_turn_on_and_thirty_to_turn_off() {
        let mut detector = Detector::new(10);
        detector.update(20);
        assert_eq!(detector.state, DetectorState::Rising);
        for _ in 0..ATTACK_FRAMES {
            detector.update(20);
        }
        assert_eq!(detector.state, DetectorState::On);
        detector.update(0);
        assert_eq!(detector.state, DetectorState::Falling);
        for _ in 0..RELEASE_FRAMES - 1 {
            detector.update(0);
        }
        assert_eq!(detector.state, DetectorState::Falling);
        detector.update(0);
        assert!(detector.is_off());
    }

    #[test]
    fn detector_drops_a_short_blip() {
        let mut detector = Detector::new(10);
        detector.update(20);
        for _ in 0..ATTACK_FRAMES {
            detector.update(0);
        }
        assert!(detector.is_off());
    }

    #[test]
    fn detects_one_utterance_after_trailing_silence() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        assert!(segmenter.push(&quiet_room(300)).is_empty());
        assert!(segmenter.push(&tone(500, 0.3)).is_empty());
        assert!(segmenter.is_speaking());
        assert!(segmenter.push(&silence(700)).is_empty());
        let done = segmenter.push(&silence(200));
        assert_eq!(done.len(), 1);
        assert!(!segmenter.is_speaking());
    }

    #[test]
    fn keeps_audio_from_before_speech_started() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        segmenter.push(&quiet_room(1000));
        segmenter.push(&tone(500, 0.3));
        let done = segmenter.push(&silence(1000));
        let ms = done[0].len() * 1000 / SAMPLE_RATE;
        assert!(ms >= 500 + 600, "length {ms}");
    }

    #[test]
    fn short_click_is_dropped() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        segmenter.push(&quiet_room(1000));
        segmenter.push(&tone(60, 0.3));
        let done = segmenter.push(&silence(300));
        assert!(done.is_empty());
        assert!(segmenter.flush().is_none());
    }

    #[test]
    fn quiet_noise_is_ignored() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        segmenter.push(&tone(2000, 0.001));
        assert!(!segmenter.is_speaking());
    }

    #[test]
    fn long_speech_is_cut_at_the_limit() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        let done = segmenter.push(&speech(26_000));
        assert_eq!(done.len(), 1);
    }

    #[test]
    fn flush_returns_speech_in_progress() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        // 録音直後の 300ms は話し始めを判定しないので、その後に声が来る長さにする。
        segmenter.push(&speech(1000));
        assert!(segmenter.flush().is_some());
    }

    #[test]
    fn volume_factor_follows_the_formula() {
        assert_eq!(volume_factor100(0.5), 400);
        assert_eq!(volume_factor100(0.0), 25);
        assert_eq!(volume_factor100(1.0), 6400);
        assert_eq!(volume_factor100(-1.0), 400);
        assert_eq!(volume_factor100(2.0), 6400);
    }

    #[test]
    fn volume_scale_matches_the_constants() {
        assert_eq!(volume_from_power(0.0, 400), 0);
        // 平均パワー 1 で上限に張り付く。
        assert_eq!(volume_from_power(1.0, 400), MAX_VOLUME);
        // しきい値ちょうどの実効値を入れると 3500 に戻る。
        let rms = f64::from(threshold_for_sensitivity(0.5));
        assert!((volume_from_power(rms * rms, 400) - VOLUME_THRESHOLD).abs() <= 2);
    }

    #[test]
    fn middle_sensitivity_threshold_is_about_minus_44_dbfs() {
        let threshold = threshold_for_sensitivity(0.5);
        assert!((0.0060..0.0068).contains(&threshold), "{threshold}");
    }

    /// 音声に近い、山がそろわない音。
    fn noise(ms: usize, amplitude: f32) -> Vec<f32> {
        let mut state = 12345u32;
        (0..ms * SAMPLE_RATE / 1000)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((state >> 8) as f32 / (1 << 24) as f32 * 2.0 - 1.0) * amplitude
            })
            .collect()
    }

    #[test]
    fn flat_topped_audio_is_too_loud() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        let clipped: Vec<f32> = noise(500, 8.0)
            .iter()
            .map(|s| s.clamp(-0.99, 0.99))
            .collect();
        segmenter.push(&clipped);
        assert!(segmenter.is_too_loud());
    }

    #[test]
    fn clean_loud_audio_is_not_too_loud() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        segmenter.push(&quiet_room(300));
        segmenter.push(&noise(500, 0.6));
        assert!(segmenter.is_speaking());
        assert!(!segmenter.is_too_loud());
    }

    /// 室内雑音の上で話す。雑音の床に追従して、話し終わりが検出されること(本機で起きた不具合の再現)。
    fn room_noise(ms: usize) -> Vec<f32> {
        // 実測した雑音(増幅後の実効値 0.007〜0.015、約 4dB の揺れ)に合わせ、
        // 振幅 0.5〜1.0(6dB)でゆっくり大小が変わる雑音にする。
        noise(ms, 0.02)
            .iter()
            .enumerate()
            .map(|(i, s)| s * (0.75 + 0.25 * ((i as f32) * 0.0007).sin()))
            .collect()
    }

    #[test]
    fn speech_over_room_noise_ends_after_the_pause() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.71);
        assert!(segmenter.push(&room_noise(1500)).is_empty());
        assert!(!segmenter.is_speaking(), "雑音だけで話し始めになっている");
        let loud: Vec<f32> = speech(2000)
            .iter()
            .zip(room_noise(2000))
            .map(|(a, b)| a + b)
            .collect();
        assert!(segmenter.push(&loud).is_empty());
        assert!(segmenter.is_speaking(), "声が始まっていない");
        let done = segmenter.push(&room_noise(1500));
        assert_eq!(done.len(), 1, "話し終わりが検出されていない");
        assert!(!segmenter.is_speaking());
    }

    #[test]
    fn threshold_rises_with_the_noise_floor_and_falls_back() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        let quiet = segmenter.effective_threshold();
        assert_eq!(
            quiet,
            (VOLUME_THRESHOLD + NOISE_OFFSET)
                .max(speech_margin(SPEECH_OVER_NOISE_RATIO) + NOISE_OFFSET)
        );
        segmenter.push(&room_noise(1500));
        let noisy = segmenter.effective_threshold();
        assert!(noisy > segmenter.noise_floor() + speech_margin(SPEECH_OVER_NOISE_RATIO) - 2);
        assert!(noisy > quiet, "{noisy} <= {quiet}");
        segmenter.push(&quiet_room(1000));
        assert!(segmenter.effective_threshold() < noisy);
        assert!(segmenter.threshold_rms() > 0.0);
    }

    #[test]
    fn capture_start_transient_does_not_start_speech() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        // 録音直後の大きな音(本機で実測した 0.26 秒の跳ね)は話し始めにしない。
        segmenter.push(&noise(200, 0.3));
        assert!(!segmenter.is_speaking());
        segmenter.push(&room_noise(500));
        assert!(!segmenter.is_speaking());
    }

    #[test]
    fn higher_sensitivity_lowers_the_threshold() {
        assert!(threshold_for_sensitivity(1.0) < threshold_for_sensitivity(0.5));
        assert!(threshold_for_sensitivity(0.5) < threshold_for_sensitivity(0.0));
    }

    /// 実際の部屋の雑音は一定ではなく、フレームごとに ±4dB ほど揺れる(iPhone と Mac で実測した揺れ 28〜67%)。
    fn busy_room_noise(ms: usize) -> Vec<f32> {
        let mut state = 777u32;
        let mut gain = 1.0f32;
        noise(ms, 0.02)
            .iter()
            .enumerate()
            .map(|(i, s)| {
                if i % FRAME_SAMPLES == 0 {
                    state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    gain = 0.6 + (state >> 8) as f32 / (1 << 24) as f32 * 0.9;
                }
                s * gain
            })
            .collect()
    }

    /// iPhone の連係は、つながるまで完全な無音(0 だけ)を届ける(本機で起きた不具合の再現)。
    /// その間に床を合わせてしまうと、音が届いた瞬間に部屋の雑音が声に見え、発話が終わらなくなる。
    #[test]
    fn dead_air_before_the_mic_connects_does_not_lower_the_floor() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.71);
        assert!(segmenter.push(&silence(3000)).is_empty());
        assert!(segmenter.push(&busy_room_noise(2000)).is_empty());
        assert!(!segmenter.is_speaking(), "雑音だけで話し始めになっている");
        let loud: Vec<f32> = speech(2000)
            .iter()
            .zip(busy_room_noise(2000))
            .map(|(a, b)| a + b)
            .collect();
        assert!(segmenter.push(&loud).is_empty());
        assert!(segmenter.is_speaking(), "声が始まっていない");
        let done = segmenter.push(&busy_room_noise(1500));
        assert_eq!(done.len(), 1, "話し終わりが検出されていない");
        assert!(!segmenter.is_speaking());
    }

    /// 床が低すぎて上限まで切れなかった発話の後は、その発話の中で最も静かだった音量まで床を上げて立て直す。
    #[test]
    fn forced_cut_raises_the_floor_to_the_quietest_part() {
        let config = SegmenterConfig {
            max_ms: 3000,
            ..SegmenterConfig::default()
        };
        let mut segmenter = Segmenter::new(config, 0.71);
        // とても静かな部屋で床を低く合わせてから、換気の音が始まったような状況。
        segmenter.push(&noise(1000, 0.002));
        let low_floor = segmenter.noise_floor();
        let cut = segmenter.push(&busy_room_noise(4000));
        assert_eq!(
            cut.len(),
            1,
            "雑音を声と見て上限まで続く状況が再現できていない"
        );
        assert!(
            segmenter.noise_floor() > low_floor + speech_margin(SPEECH_OVER_NOISE_RATIO) / 2,
            "床が上がっていない"
        );
        assert!(segmenter.push(&busy_room_noise(2000)).is_empty());
        assert!(!segmenter.is_speaking(), "立て直した後も雑音を声と見ている");
    }

    /// 台本どおりに答える判定役。フレーム番号(20 ms 単位)の区切りごとに答えを決め、尽きたら最後の答えを繰り返す。
    struct Script {
        /// (このフレーム番号の手前まで, 答え)
        spans: Vec<(usize, Verdict)>,
        at: usize,
    }

    impl Script {
        fn boxed(spans: &[(usize, Verdict)]) -> Box<dyn VoiceJudge> {
            Box::new(Script {
                spans: spans.to_vec(),
                at: 0,
            })
        }
    }

    impl VoiceJudge for Script {
        fn hear(&mut self, _frame: &[f32]) -> Verdict {
            let at = self.at;
            self.at += 1;
            self.spans
                .iter()
                .find(|(until, _)| at < *until)
                .map_or(self.spans.last().expect("台本").1, |(_, verdict)| *verdict)
        }

        fn name(&self) -> String {
            "台本".to_string()
        }
    }

    const NO_VOICE: Verdict = Verdict {
        speaking: false,
        kept: false,
        ended: None,
    };
    const VOICE_NOT_YET_KEPT: Verdict = Verdict {
        speaking: true,
        kept: false,
        ended: None,
    };

    /// 1 フレームずつ入れて、切り出しが返ったフレーム番号を集める。
    fn cut_frames(segmenter: &mut Segmenter, audio: &[f32]) -> Vec<usize> {
        let mut cuts = Vec::new();
        for (index, frame) in audio.chunks(FRAME_SAMPLES).enumerate() {
            if !segmenter.push(frame).is_empty() {
                cuts.push(index);
            }
        }
        cuts
    }

    #[test]
    fn a_judge_that_hears_no_voice_keeps_loud_noise_out() {
        let mut segmenter =
            Segmenter::new(SegmenterConfig::default(), 0.5).with_judge(Some(Script::boxed(&[(0, NO_VOICE)])));
        segmenter.push(&quiet_room(300));
        assert!(segmenter.push(&tone(2000, 0.3)).is_empty());
        assert!(!segmenter.is_speaking(), "判定役が声でないと言う音で話し始めになった");
        assert!(segmenter.push(&silence(1000)).is_empty());
        assert!(segmenter.flush().is_none());
    }

    #[test]
    fn speech_starts_only_once_the_judge_keeps_it() {
        // 1 秒の静かな部屋のあと声が始まる。判定役は 1.7 秒(85 フレーム)まで「声だがまだ確かめていない」。
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5)
            .with_judge(Some(Script::boxed(&[(85, VOICE_NOT_YET_KEPT), (usize::MAX, Verdict::UNJUDGED)])));
        segmenter.push(&quiet_room(1000));
        segmenter.push(&speech(600));
        assert!(!segmenter.is_speaking(), "確かめる前に話し始めになった");
        segmenter.push(&speech(200));
        assert!(segmenter.is_speaking(), "確かめた後も話し始めにならない");
        // 話し始めの前の音(pre_ms)は、確かめるのが遅れても欠けない。
        let done = segmenter.push(&quiet_room(1000));
        assert_eq!(done.len(), 1);
        let ms = done[0].len() * 1000 / SAMPLE_RATE;
        assert!(ms >= 800 + 600 + 800 - 40, "切り出し {ms} ms(声 800 + 前 600 + 後 800 のはず)");
    }

    #[test]
    fn the_judges_end_shortens_the_wait_after_the_voice() {
        // 0.5 秒の静かな部屋のあと、音量の大きい音が 3 秒続く。判定役は 1.6 秒(80 フレーム)で声が
        // 1.5 秒に終わったと確定する。終わりを知らせた判定役の方が 0.1 秒早く切り出す。
        let mut audio = quiet_room(500);
        audio.extend(speech(3000));
        let voice_ended = Verdict {
            speaking: false,
            kept: false,
            ended: Some(1.5),
        };
        let mut with_end = Segmenter::new(SegmenterConfig::default(), 0.5).with_judge(Some(
            Script::boxed(&[(80, Verdict::UNJUDGED), (81, voice_ended), (usize::MAX, NO_VOICE)]),
        ));
        let mut without_end = Segmenter::new(SegmenterConfig::default(), 0.5)
            .with_judge(Some(Script::boxed(&[(80, Verdict::UNJUDGED), (usize::MAX, NO_VOICE)])));
        let a = cut_frames(&mut with_end, &audio);
        let b = cut_frames(&mut without_end, &audio);
        assert_eq!(a.len(), 1, "{a:?}");
        assert_eq!(b.len(), 1, "{b:?}");
        // 無音 800 ms: 終わりを知らなければ 1.6 秒から数えて 2.4 秒(120 フレーム目)。
        // 知らされれば 1.5 秒から数えて 2.3 秒(115 フレーム目前後)。
        assert_eq!(b[0], 119, "{b:?}");
        assert!((113..=115).contains(&a[0]), "{a:?}");
    }

    #[test]
    fn a_dropped_false_start_keeps_the_audio_before_the_next_voice() {
        // 60 ms の物音(短いので捨てる)の 0.9 秒後に声が始まる。捨てた物音の後の音が
        // 話し始めの前の音として残り、声の切り出しが前 600 ms を欠かないこと。
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        segmenter.push(&quiet_room(1000));
        segmenter.push(&tone(60, 0.3));
        assert!(segmenter.push(&quiet_room(900)).is_empty());
        assert!(!segmenter.is_speaking());
        segmenter.push(&tone(500, 0.3));
        let done = segmenter.push(&quiet_room(1000));
        assert_eq!(done.len(), 1);
        let ms = done[0].len() * 1000 / SAMPLE_RATE;
        assert!(ms >= 500 + 600 + 800 - 100, "切り出し {ms} ms(前 600 ms が欠けている)");
    }

    /// 完全な無音(音が届いていない)のフレームも話し始めの前の音として残す。位置の計算が続き、
    /// 判定役が確かめるまでの間に 1 フレーム途切れても溜めた話し始めを失わない。
    #[test]
    fn dead_air_is_kept_as_silence_before_speech() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5);
        segmenter.push(&quiet_room(1000));
        segmenter.push(&silence(200));
        segmenter.push(&quiet_room(200));
        segmenter.push(&tone(500, 0.3));
        let done = segmenter.push(&quiet_room(1000));
        assert_eq!(done.len(), 1);
        let ms = done[0].len() * 1000 / SAMPLE_RATE;
        assert!(ms >= 500 + 600 + 800 - 100, "切り出し {ms} ms(無音の前の音が欠けている)");
        // 判定役ありで、確かめる前(1.1〜1.14 秒)に完全な無音が 2 フレーム挟まっても、途切れのない同じ音と
        // 同じ切り出しになる(話し始めの位置を失わない)。
        let mut audio = quiet_room(1000);
        audio.extend(speech(600));
        audio.extend(quiet_room(1000));
        let mut gapped = audio.clone();
        let gap = 1100 * SAMPLE_RATE / 1000..1140 * SAMPLE_RATE / 1000;
        gapped[gap].fill(0.0);
        let judge = || Some(Script::boxed(&[(60, VOICE_NOT_YET_KEPT), (usize::MAX, Verdict::UNJUDGED)]));
        let whole = Segmenter::new(SegmenterConfig::default(), 0.5).with_judge(judge()).push(&audio);
        let cut = Segmenter::new(SegmenterConfig::default(), 0.5).with_judge(judge()).push(&gapped);
        assert_eq!(whole.len(), 1);
        assert_eq!(cut.len(), 1);
        let (whole_ms, cut_ms) = (whole[0].len() * 1000 / SAMPLE_RATE, cut[0].len() * 1000 / SAMPLE_RATE);
        assert_eq!(whole_ms, cut_ms, "途切れで切り出しの長さが変わった");
        assert!(whole_ms >= 600 + 300 + 800 - 40, "切り出し {whole_ms} ms");
    }

    /// 離れた人の声が続く中で 1 度だけ基準を超えた物音は、1 秒後に始まった自分の声の話し始めにならない。
    #[test]
    fn a_blip_long_before_the_voice_does_not_pull_the_onset_back() {
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5)
            .with_judge(Some(Script::boxed(&[(0, Verdict::UNJUDGED)])));
        segmenter.push(&quiet_room(1000));
        segmenter.push(&tone(20, 0.3));
        segmenter.push(&quiet_room(1000));
        assert!(!segmenter.is_speaking());
        segmenter.push(&speech(500));
        assert!(segmenter.is_speaking());
        let done = segmenter.push(&quiet_room(1000));
        assert_eq!(done.len(), 1);
        let ms = done[0].len() * 1000 / SAMPLE_RATE;
        // 前 600 + 声 500 + 後 800。物音まで戻っていれば 1 秒以上長い。
        assert!(ms <= 600 + 500 + 800 + 100, "切り出し {ms} ms(話し始めが物音まで戻っている)");
        assert!(ms >= 600 + 500 + 800 - 100, "切り出し {ms} ms");
    }

    /// 判定役が今の発話より前の区間の終わりを知らせても、無音の数え直しには使わない。
    #[test]
    fn an_end_before_the_utterance_is_ignored() {
        let early_end = Verdict {
            speaking: true,
            kept: true,
            ended: Some(0.1),
        };
        let mut segmenter = Segmenter::new(SegmenterConfig::default(), 0.5).with_judge(Some(
            Script::boxed(&[(60, Verdict::UNJUDGED), (61, early_end), (usize::MAX, Verdict::UNJUDGED)]),
        ));
        segmenter.push(&quiet_room(500));
        assert!(segmenter.push(&speech(2000)).is_empty(), "古い終わりで切れた");
        assert!(segmenter.is_speaking());
    }
}
