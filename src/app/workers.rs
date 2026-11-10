//! 処理役のスレッド(転送・訂正・外部認識・補完)と、そこへの依頼と結果の型。

use super::*;

/// 処理役のスレッドを `count` 本立て、依頼の口を返す。依頼は 1 つの列に並び、手の空いたスレッドが順に取る。
/// `work` が返した結果を `done` へ送り、画面を描き直させる。
///
/// `work` が panic しても、そのスレッドは死なずに次の依頼へ進み、その依頼には `failed` が作った失敗の結果を送る
/// (待っている側は普通の失敗として扱える)。`failed` は依頼を `work` に渡す前に呼ぶ。
pub(super) fn spawn_pool<J, D>(
    label: &'static str,
    count: usize,
    done: Sender<D>,
    ctx: egui::Context,
    failed: impl Fn(&J) -> D + Send + Sync + 'static,
    work: impl Fn(J) -> D + Send + Sync + 'static,
) -> Sender<J>
where
    J: Send + 'static,
    D: Send + 'static,
{
    let (jobs, queue) = mpsc::channel::<J>();
    let queue = Arc::new(Mutex::new(queue));
    let work = Arc::new(work);
    let failed = Arc::new(failed);
    for _ in 0..count {
        let queue = queue.clone();
        let done = done.clone();
        let ctx = ctx.clone();
        let work = work.clone();
        let failed = failed.clone();
        std::thread::spawn(move || {
            // 次の依頼を受け取る間だけ鍵を持つ。
            while let Some(job) = queue.lock().ok().and_then(|queue| queue.recv().ok()) {
                let fallback = failed(&job);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(job)))
                    .unwrap_or_else(|_| {
                        diag::log(format!("{label}の処理役で想定外の失敗(失敗として返す)"));
                        fallback
                    });
                let _ = done.send(result);
                ctx.request_repaint();
            }
        });
    }
    jobs
}

/// 処理の途中で想定外の失敗(panic)が起きたときの知らせ。
const UNEXPECTED: &str = "処理の途中で想定外の失敗が起きました";

/// 転送の依頼。クリップボードは共有なので、1 本のスレッドで順に処理する。
pub(super) struct TransferJob {
    pub(super) text: String,
    pub(super) target: Option<Target>,
    pub(super) method: TransferMethod,
    pub(super) restore_clipboard: bool,
    pub(super) enter_after: bool,
    /// 転送時点のエディターの中身。成功したらこの分だけ消す。
    pub(super) editor_snapshot: Option<String>,
    /// 完了の知らせで伝えること。
    pub(super) warning: Option<SendWarning>,
}

/// 転送の結果を画面へ戻す。
pub(super) struct TransferDone {
    pub(super) result: Result<Delivery, TransferError>,
    pub(super) editor_snapshot: Option<String>,
    pub(super) warning: Option<SendWarning>,
}

/// 転送は 1 本のスレッドで順に処理する(クリップボードは共有)。
pub(super) fn spawn_transfer_worker(
    done: Sender<TransferDone>,
    ctx: egui::Context,
    send: TransferFn,
) -> Sender<TransferJob> {
    let failed = |job: &TransferJob| TransferDone {
        result: Err(TransferError::Deliver(UNEXPECTED.to_string())),
        editor_snapshot: job.editor_snapshot.clone(),
        warning: job.warning,
    };
    spawn_pool("転送", 1, done, ctx, failed, move |job: TransferJob| {
        let began = Instant::now();
        let result = send(TransferRequest {
            text: &job.text,
            target: job.target.as_ref(),
            method: job.method,
            restore_clipboard: job.restore_clipboard,
            enter_after: job.enter_after,
        });
        match &result {
            Ok(delivery) => diag::log(format!(
                "転送 {} 字 → {}({:.2} 秒、{})",
                job.text.chars().count(),
                job.target.as_ref().map_or("?", |t| t.name.as_str()),
                began.elapsed().as_secs_f32(),
                delivery.label()
            )),
            Err(error) => diag::log(format!("転送できず: {error}")),
        }
        TransferDone {
            result,
            editor_snapshot: job.editor_snapshot,
            warning: job.warning,
        }
    })
}

/// 外部の音声認識 API への依頼 1 件。`id` は発話の番号、`haas_text` は表示している Haas の文。
pub(super) struct RemoteJob {
    pub(super) id: u64,
    pub(super) haas_text: String,
    pub(super) audio: Arc<Vec<f32>>,
    pub(super) provider: Arc<dyn RemoteProvider>,
    pub(super) hints: remote_asr::Hints,
    /// これを過ぎたら送らない(列で待つ間に、アプリは Haas の文で先へ進んでいる)。待つ上限もここまで。
    pub(super) deadline: Instant,
}

/// 外部の音声認識 API の答え。
pub(super) struct RemoteDone {
    pub(super) id: u64,
    pub(super) haas_text: String,
    pub(super) result: Result<String, remote_asr::RemoteError>,
}

/// 外部の音声認識 API の処理役。訂正の 3 本とは別に 2 本(答えを待つ間、訂正を塞がない)。
pub(super) fn spawn_remote_worker(done: Sender<RemoteDone>, ctx: egui::Context) -> Sender<RemoteJob> {
    let failed = |job: &RemoteJob| RemoteDone {
        id: job.id,
        haas_text: job.haas_text.clone(),
        result: Err(remote_asr::RemoteError::Temporary(UNEXPECTED.to_string())),
    };
    spawn_pool("外部認識", 2, done, ctx, failed, |job: RemoteJob| {
        let began = Instant::now();
        // 列で待った分を引いた残り時間だけ待つ(1 秒も残っていなければ送らない)。
        let remaining = job.deadline.saturating_duration_since(began);
        if remaining < Duration::from_secs(1) {
            return RemoteDone {
                id: job.id,
                haas_text: job.haas_text,
                result: Err(remote_asr::RemoteError::Temporary(
                    "待つ上限を過ぎたので送りませんでした".to_string(),
                )),
            };
        }
        let result = crate::core::recognizer::wav_bytes(&job.audio)
            .map_err(remote_asr::RemoteError::Temporary)
            .and_then(|wav| job.provider.transcribe(&wav, &job.hints, remaining));
        match &result {
            Ok(text) => diag::log(format!(
                "外部認識 {} 字 → {} 字({:.2} 秒、{})",
                job.haas_text.chars().count(),
                text.chars().count(),
                began.elapsed().as_secs_f32(),
                job.provider.label()
            )),
            // 誤りの説明はサーバーの文を含むので、記録には英数字の部分だけを残す(話した文を書かない)。
            Err(error) => diag::log(format!(
                "外部認識エラー({}): {}",
                job.provider.label(),
                ascii_only(&error.to_string())
            )),
        }
        RemoteDone {
            id: job.id,
            haas_text: job.haas_text,
            result,
        }
    })
}

/// 記録に残してよい部分(英数字と記号の 1 行、80 字まで)。
fn ascii_only(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_graphic() || *c == ' ')
        .take(80)
        .collect::<String>()
        .trim()
        .to_string()
}

/// AI の段への依頼の種類。いまは訂正だけ(テンプレート補完は別の処理役 `spawn_assistant_worker`。
/// 1 件に数秒かかるので、訂正の 3 本を塞がない)。
pub(super) enum AiRequest {
    Correction {
        /// 学習で当てた置き換え(AI が元に戻したら、その組の重みを下げる)。
        applied: Vec<(String, String)>,
        request: CorrectionRequest,
        corrector: Arc<dyn Corrector>,
    },
}

/// AI の段への依頼 1 件。`id` は発話の番号。
pub(super) struct AiJob {
    pub(super) id: u64,
    pub(super) request: AiRequest,
}

pub(super) struct CorrectionDone {
    pub(super) id: u64,
    pub(super) applied: Vec<(String, String)>,
    /// AI に送った文(学習の置き換えを当てた後)。学習の「元」。
    pub(super) original: String,
    pub(super) result: Result<String, String>,
}

/// 訂正は通信待ちが長いので、認識とは別のスレッドで処理する。Claude Code は 1 件に数秒かかるので、
/// 話す速さに追いつくよう同時に進める。終わる順は前後してよい(確定は `watch_corrections` が話した順に行う)。
pub(super) fn spawn_ai_worker(done: Sender<CorrectionDone>, ctx: egui::Context) -> Sender<AiJob> {
    let failed = |job: &AiJob| {
        let AiRequest::Correction {
            applied, request, ..
        } = &job.request;
        CorrectionDone {
            id: job.id,
            applied: applied.clone(),
            original: request.text.clone(),
            result: Err(UNEXPECTED.to_string()),
        }
    };
    spawn_pool("訂正", CORRECTION_WORKERS, done, ctx, failed, |job: AiJob| {
        let AiRequest::Correction {
            applied,
            request,
            corrector,
        } = job.request;
        let began = Instant::now();
        let result = corrector.correct(&request);
        match &result {
            Ok(text) => diag::log(format!(
                "訂正 {} 字 → {} 字({:.2} 秒、{})",
                request.text.chars().count(),
                text.chars().count(),
                began.elapsed().as_secs_f32(),
                corrector.name()
            )),
            // 誤りの文はサーバーや Claude Code の出力を含むので、英数字の部分だけを残す。
            Err(message) => diag::log(format!("訂正エラー: {}", ascii_only(message))),
        }
        CorrectionDone {
            id: job.id,
            applied,
            original: request.text,
            result,
        }
    })
}

/// テンプレート補完の依頼 1 件。`prepared` は下ごしらえ済みの依頼(文脈・候補を添えた物)、`plan` は補完役の
/// 指示文を作ったときの辞書と構造(描画に使う)。
pub(super) struct AssistantJob {
    pub(super) prepared: completion::Prepared,
    pub(super) plan: Arc<Plan>,
    pub(super) assistant: Arc<dyn Assistant>,
}

pub(super) struct AssistantDone {
    pub(super) result: Result<Outcome, String>,
    pub(super) took: Duration,
}

/// 補完は 1 本のスレッドで順に処理する(依頼は一度に 1 つしか受けない)。
pub(super) fn spawn_assistant_worker(done: Sender<AssistantDone>, ctx: egui::Context) -> Sender<AssistantJob> {
    let failed = |_: &AssistantJob| AssistantDone {
        result: Err(format!("補完役: {UNEXPECTED}")),
        took: Duration::ZERO,
    };
    spawn_pool("補完", 1, done, ctx, failed, |job: AssistantJob| {
        let began = Instant::now();
        let result = completion::run(job.assistant.as_ref(), &job.prepared, &job.plan);
        match &result {
            // 依頼と本文の中身と文脈は残さない(患者の情報を含みうる)。経路・選択の数・退けた理由(テンプレートの
            // モジュール名と行、守りの種類)だけ。
            Ok(outcome) => {
                diag::log(format!(
                    "補完 {} 字 → {} 字({:.2} 秒、{}、{}、選択 {}/{}、呼んだ回数 {})",
                    job.prepared.request.request.chars().count(),
                    outcome.reply.text.as_ref().map_or(0, |t| t.chars().count()),
                    began.elapsed().as_secs_f32(),
                    job.assistant.name(),
                    outcome.route.label(),
                    outcome.kept.len(),
                    outcome.claude_selections.len(),
                    outcome.calls
                ));
                for reason in &outcome.dropped {
                    diag::log(format!("補完: 選択を退けた({reason})"));
                }
            }
            Err(message) => diag::log(format!("補完エラー: {}", ascii_only(message))),
        }
        AssistantDone {
            result,
            took: began.elapsed(),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 依頼は `count` 本で同時に処理され、結果はすべて届く(3 本そろわないと進まない柵で確かめる)。
    #[test]
    fn the_pool_runs_jobs_side_by_side_and_returns_every_result() {
        let (done, results) = mpsc::channel::<u32>();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let jobs = spawn_pool("試験", 3, done, egui::Context::default(), |_| 0, move |n: u32| {
            barrier.wait();
            n * 2
        });
        for n in 1..=3 {
            jobs.send(n).unwrap();
        }
        let mut got: Vec<u32> = (0..3)
            .map(|_| results.recv_timeout(Duration::from_secs(5)).expect("3 本が同時に動く"))
            .collect();
        got.sort_unstable();
        assert_eq!(got, vec![2, 4, 6]);
    }

    /// 処理が panic しても、そのスレッドは次の依頼を受ける(並列数が減らない)。panic した依頼には失敗の結果が届く。
    #[test]
    fn a_panicking_job_does_not_kill_the_worker() {
        let (done, results) = mpsc::channel::<i64>();
        let failed = |n: &u32| -i64::from(*n);
        let jobs = spawn_pool("試験", 1, done, egui::Context::default(), failed, |n: u32| {
            assert!(n != 3, "わざと失敗させる");
            i64::from(n)
        });
        jobs.send(3).unwrap();
        jobs.send(7).unwrap();
        let recv = || results.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(recv(), -3, "失敗の結果");
        assert_eq!(recv(), 7, "同じスレッドが次も受ける");
    }
}
