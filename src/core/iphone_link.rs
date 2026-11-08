//! iPhone の連係の見張り。音が届くのを待ち、途中で音が死んだら開き直し、待ちの上限で諦める。
//!
//! 連係は切れてもデバイスが一覧に残り、0 だけの音を流し続ける(2026-11-06 の診断ログ)。
//! 判断だけをここに分け、時刻は引数で受ける。録音の開閉は呼び元が行う。

use std::time::{Duration, Instant};

/// 音が届くのを待つ上限。開く処理の時間も含む。再接続に 13 秒以上かかることがある。
pub const LINK_WAIT_LIMIT: Duration = Duration::from_secs(30);
/// 音が届いたあとで 0 だけの音がこれだけ続いたら、連係が切れたとみなす。
pub const DEAD_AIR: Duration = Duration::from_secs(2);
/// これより速く開いたら、連係を張らずに古いつながりを返したとみなす。実測は速いとき 0.14 秒以下、
/// 連係を張るとき 3 秒以上。
pub const FAST_OPEN: Duration = Duration::from_secs(1);
/// 速く開いたのに音が来ないとき、開き直すまでの長さ。その形で 8 秒待って音が届いた例は実測で 1 つも無い(9 回)。
/// 遅く開いたときは連係を張っている途中で、0 だけの音のあとで届くことがあるので、切らずに上限まで待つ。
pub const LINK_RETRY: Duration = Duration::from_secs(10);
/// 音が戻ってからこれだけ切れなければ、安定したとみなす。
pub const STEADY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStep {
    /// そのまま続ける。
    Keep,
    /// 待っていた音が届いた。待った時間を添える。
    Arrived(Duration),
    /// デバイスを閉じて開き直す。
    Reopen,
    /// 待ちの上限を過ぎた。
    GiveUp,
}

#[derive(Debug, Clone)]
pub struct LinkWatch {
    /// 音を待ち始めた時刻。音が届いている間は None。
    waiting_since: Option<Instant>,
    /// 次に開き直す時刻。
    retry_at: Option<Instant>,
    /// 音が戻った直後の控え(待ち始めた時刻, 音が戻った時刻)。短い音で切れを繰り返しても、
    /// 待ちの上限を最初に切れた時刻から数え続ける(数え直すと Mac へ切り替わらない)。
    recovered: Option<(Instant, Instant)>,
}

impl LinkWatch {
    /// デバイスを開き始めたときに作る。
    pub fn new(now: Instant) -> Self {
        LinkWatch {
            waiting_since: Some(now),
            retry_at: None,
            recovered: None,
        }
    }

    /// デバイスが開いた。`took` は開く処理そのものにかかった時間。
    pub fn opened(&mut self, now: Instant, took: Duration) {
        self.retry_at = (took < FAST_OPEN).then_some(now + LINK_RETRY);
    }

    /// デバイスを開けなかった。
    pub fn open_failed(&mut self, now: Instant) {
        self.retry_at = Some(now + LINK_RETRY);
    }

    /// 音を待っている時間。音が届いているなら None。
    pub fn waiting_for(&self, now: Instant) -> Option<Duration> {
        self.waiting_since
            .map(|since| now.saturating_duration_since(since))
    }

    /// `heard` はいまのデバイスから音が届いたか、`silence` は 0 だけの音が続いている長さ。
    pub fn step(&mut self, heard: bool, silence: Duration, now: Instant) -> LinkStep {
        let Some(since) = self.waiting_since else {
            if silence >= DEAD_AIR {
                let since = self.recovered.take().map_or(now, |(since, _)| since);
                if now >= since + LINK_WAIT_LIMIT {
                    return LinkStep::GiveUp;
                }
                return self.reopen(since);
            }
            if silence.is_zero() && self.recovered.is_some_and(|(_, at)| now >= at + STEADY) {
                self.recovered = None;
            }
            return LinkStep::Keep;
        };
        if heard {
            self.waiting_since = None;
            self.retry_at = None;
            self.recovered = Some((since, now));
            return LinkStep::Arrived(now.saturating_duration_since(since));
        }
        if now >= since + LINK_WAIT_LIMIT {
            return LinkStep::GiveUp;
        }
        if self.retry_at.is_some_and(|at| now >= at) {
            return self.reopen(since);
        }
        LinkStep::Keep
    }

    fn reopen(&mut self, since: Instant) -> LinkStep {
        self.waiting_since = Some(since);
        self.retry_at = None;
        LinkStep::Reopen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Duration = Duration::ZERO;

    fn seconds(value: u64) -> Duration {
        Duration::from_secs(value)
    }

    #[test]
    fn a_slow_connection_is_waited_for() {
        let start = Instant::now();
        let mut watch = LinkWatch::new(start);
        // 開くのに 5 秒、その後 6 秒の 0 だけの音。8 秒で諦めていた場面。
        assert_eq!(watch.step(false, NONE, start + seconds(4)), LinkStep::Keep);
        watch.opened(start + seconds(5), seconds(5));
        assert_eq!(watch.step(false, NONE, start + seconds(11)), LinkStep::Keep);
        assert_eq!(
            watch.step(true, NONE, start + seconds(13)),
            LinkStep::Arrived(seconds(13))
        );
        assert_eq!(watch.waiting_for(start + seconds(13)), None);
    }

    #[test]
    fn a_slow_open_is_never_cut_before_the_limit() {
        let start = Instant::now();
        let mut watch = LinkWatch::new(start);
        watch.opened(start + seconds(5), seconds(5));
        for at in 6..30 {
            assert_eq!(
                watch.step(false, seconds(at - 5), start + seconds(at)),
                LinkStep::Keep,
                "連係を張っている途中で開き直さない({at} 秒)"
            );
        }
        assert_eq!(
            watch.step(false, seconds(25), start + seconds(30)),
            LinkStep::GiveUp
        );
    }

    #[test]
    fn dead_air_after_sound_reopens_once() {
        let start = Instant::now();
        let mut watch = LinkWatch::new(start);
        watch.opened(start, NONE);
        assert!(matches!(
            watch.step(true, NONE, start + seconds(1)),
            LinkStep::Arrived(_)
        ));
        assert_eq!(watch.step(true, NONE, start + seconds(10)), LinkStep::Keep);
        let cut = start + seconds(20);
        assert_eq!(watch.step(true, seconds(1), cut), LinkStep::Keep);
        assert_eq!(watch.step(true, DEAD_AIR, cut), LinkStep::Reopen);
        assert_eq!(watch.waiting_for(cut), Some(NONE));
        assert_eq!(
            watch.step(false, DEAD_AIR, cut + seconds(1)),
            LinkStep::Keep,
            "開き直している間に続けて開き直さない"
        );
    }

    #[test]
    fn a_fast_open_without_sound_is_reopened_while_waiting() {
        let start = Instant::now();
        let mut watch = LinkWatch::new(start);
        watch.opened(start, NONE);
        assert_eq!(watch.step(false, NONE, start + seconds(9)), LinkStep::Keep);
        assert_eq!(
            watch.step(false, NONE, start + seconds(10)),
            LinkStep::Reopen
        );
        assert_eq!(
            watch.step(false, NONE, start + seconds(19)),
            LinkStep::Keep,
            "開き終わるまでは次を数えない"
        );
        watch.opened(start + seconds(20), NONE);
        assert_eq!(
            watch.step(false, NONE, start + seconds(29)),
            LinkStep::Keep
        );
    }

    #[test]
    fn a_failed_open_is_retried() {
        let start = Instant::now();
        let mut watch = LinkWatch::new(start);
        watch.open_failed(start + seconds(5));
        assert_eq!(watch.step(false, NONE, start + seconds(14)), LinkStep::Keep);
        assert_eq!(
            watch.step(false, NONE, start + seconds(15)),
            LinkStep::Reopen
        );
    }

    #[test]
    fn short_blips_do_not_restart_the_limit() {
        let start = Instant::now();
        let mut watch = LinkWatch::new(start);
        watch.opened(start, NONE);
        let mut now = start;
        let mut reopened = 0;
        // 開く → 短い音 → 0 だけの音、を繰り返す。
        loop {
            now += seconds(1);
            assert!(matches!(watch.step(true, NONE, now), LinkStep::Arrived(_)));
            now += seconds(2);
            match watch.step(true, DEAD_AIR, now) {
                LinkStep::Reopen => reopened += 1,
                LinkStep::GiveUp => break,
                other => panic!("{other:?}"),
            }
            watch.opened(now, NONE);
        }
        assert!(reopened >= 2);
        assert!(now <= start + LINK_WAIT_LIMIT + seconds(3), "上限のすぐ後で諦める");
    }

    #[test]
    fn sound_that_keeps_stopping_short_of_steady_does_not_restart_the_limit() {
        let start = Instant::now();
        let mut watch = LinkWatch::new(start);
        watch.opened(start, NONE);
        watch.step(true, NONE, start);
        assert_eq!(
            watch.step(true, seconds(1), start + STEADY),
            LinkStep::Keep,
            "5 秒の時点で音が止まっていたら、まだ安定とは言わない"
        );
        let cut = start + STEADY + seconds(1);
        assert_eq!(watch.step(true, DEAD_AIR, cut), LinkStep::Reopen);
        assert_eq!(watch.waiting_for(cut), Some(STEADY + seconds(1)));
    }

    #[test]
    fn sound_that_stays_clears_the_memory_of_the_cut() {
        let start = Instant::now();
        let mut watch = LinkWatch::new(start);
        watch.opened(start, NONE);
        watch.step(true, NONE, start);
        assert_eq!(watch.step(true, NONE, start + STEADY), LinkStep::Keep);
        let cut = start + seconds(600);
        assert_eq!(watch.step(true, DEAD_AIR, cut), LinkStep::Reopen);
        assert_eq!(watch.waiting_for(cut), Some(NONE));
        assert_eq!(watch.step(false, NONE, cut + seconds(29)), LinkStep::Keep);
        assert_eq!(watch.step(false, NONE, cut + seconds(30)), LinkStep::GiveUp);
    }

    #[test]
    fn the_wait_ends_at_the_limit_even_while_opening() {
        let start = Instant::now();
        let mut watch = LinkWatch::new(start);
        assert_eq!(watch.step(false, NONE, start + seconds(29)), LinkStep::Keep);
        assert_eq!(
            watch.step(false, NONE, start + LINK_WAIT_LIMIT),
            LinkStep::GiveUp
        );
    }
}
