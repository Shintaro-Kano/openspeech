import Foundation
import AVFoundation
import Speech

// openspeech-speech: Apple のオンデバイス音声認識(SpeechAnalyzer + SpeechTranscriber, macOS 26)を
// 標準入出力で使う小さな補助プロセス。
//
// 標準入力(バイナリ): 4 バイト(リトルエンディアン)の中身の長さ、1 バイトの種類、中身。
//   1 = 音声(Int16 リトルエンディアン、モノラル 16 kHz)
//   2 = 発話の終わり(中身なし): 確定結果を出し、次の発話を待つ
//   3 = 終了
// 標準出力(1 行 1 JSON): ready / status / partial / final / error。
// 標準入力が閉じたら終わる。

enum Frame: Sendable {
    case audio(Data)
    case end
    case quit
}

let localeID = "ja-JP"
let sampleRate = 16_000.0

func emit(_ type: String, _ text: String? = nil) {
    var object: [String: String] = ["type": type]
    if let text { object["text"] = text }
    guard var line = try? JSONSerialization.data(withJSONObject: object, options: []) else { return }
    line.append(0x0A)
    FileHandle.standardOutput.write(line)
}

/// read(2) で必ず n バイト読む。途中で閉じたら nil。
func readExactly(_ n: Int) -> Data? {
    if n == 0 { return Data() }
    var data = Data(count: n)
    var got = 0
    while got < n {
        let r = data.withUnsafeMutableBytes { raw -> Int in
            read(0, raw.baseAddress! + got, n - got)
        }
        if r < 0 && errno == EINTR { continue }
        if r <= 0 { return nil }
        got += r
    }
    return data
}

/// 標準入力を別スレッドで読み、フレームを流す。
func startReader() -> AsyncStream<Frame> {
    let (stream, cont) = AsyncStream<Frame>.makeStream()
    Thread.detachNewThread {
        while true {
            guard let header = readExactly(5) else { break }
            let length = Int(header[0]) | Int(header[1]) << 8 | Int(header[2]) << 16 | Int(header[3]) << 24
            let kind = header[4]
            guard let payload = readExactly(length) else { break }
            switch kind {
            case 1: cont.yield(.audio(payload))
            case 2: cont.yield(.end)
            case 3: cont.yield(.quit); cont.finish(); return
            default: emit("error", "unknown frame type \(kind)")
            }
        }
        cont.yield(.quit)
        cont.finish()
    }
    return stream
}

@available(macOS 26.0, *)
func makeTranscriber(_ locale: Locale) -> SpeechTranscriber {
    SpeechTranscriber(locale: locale, transcriptionOptions: [], reportingOptions: [.volatileResults, .fastResults], attributeOptions: [])
}

@available(macOS 26.0, *)
func ensureAsset(_ locale: Locale) async throws {
    let transcriber = makeTranscriber(locale)
    let status = await AssetInventory.status(forModules: [transcriber])
    if status == .installed { return }
    emit("status", "音声認識モデル(\(localeID))を準備しています(\(status))")
    if let request = try await AssetInventory.assetInstallationRequest(supporting: [transcriber]) {
        let progress = request.progress
        let watcher = Task {
            var last = -1
            while !Task.isCancelled {
                let percent = Int(progress.fractionCompleted * 100)
                if percent != last { emit("status", "ダウンロード中 \(percent)%"); last = percent }
                try? await Task.sleep(nanoseconds: 500_000_000)
            }
        }
        defer { watcher.cancel() }
        try await request.downloadAndInstall()
    }
    let after = await AssetInventory.status(forModules: [transcriber])
    if after != .installed { throw NSError(domain: "openspeech-speech", code: 1, userInfo: [NSLocalizedDescriptionKey: "asset not installed (\(after))"]) }
    emit("status", "音声認識モデルを入れました")
}

/// 1 発話ぶんの解析。
@available(macOS 26.0, *)
@MainActor
final class Session {
    let analyzer: SpeechAnalyzer
    let input: AsyncStream<AnalyzerInput>.Continuation
    let results: Task<String, Error>
    let format: AVAudioFormat
    let sourceFormat: AVAudioFormat
    let converter: AVAudioConverter?

    init(locale: Locale) async throws {
        let transcriber = makeTranscriber(locale)
        analyzer = SpeechAnalyzer(modules: [transcriber])
        sourceFormat = AVAudioFormat(commonFormat: .pcmFormatInt16, sampleRate: sampleRate, channels: 1, interleaved: true)!
        let best = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [transcriber])
        format = best ?? sourceFormat
        converter = format == sourceFormat ? nil : AVAudioConverter(from: sourceFormat, to: format)
        let (stream, cont) = AsyncStream<AnalyzerInput>.makeStream()
        input = cont
        results = Task {
            var finalized = ""
            for try await r in transcriber.results {
                let text = String(r.text.characters)
                if r.isFinal {
                    finalized += text
                } else {
                    emit("partial", finalized + text)
                }
            }
            return finalized
        }
        try await analyzer.start(inputSequence: stream)
    }

    func feed(_ data: Data) throws {
        let frames = AVAudioFrameCount(data.count / 2)
        if frames == 0 { return }
        guard let pcm = AVAudioPCMBuffer(pcmFormat: sourceFormat, frameCapacity: frames) else { return }
        pcm.frameLength = frames
        data.withUnsafeBytes { raw in
            memcpy(pcm.int16ChannelData![0], raw.baseAddress!, Int(frames) * 2)
        }
        if let converter {
            let capacity = AVAudioFrameCount(Double(frames) * format.sampleRate / sourceFormat.sampleRate) + 1024
            guard let out = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: capacity) else { return }
            var err: NSError?
            var done = false
            converter.convert(to: out, error: &err) { _, status in
                if done { status.pointee = .noDataNow; return nil }
                done = true; status.pointee = .haveData; return pcm
            }
            if let err { throw err }
            input.yield(AnalyzerInput(buffer: out))
        } else {
            input.yield(AnalyzerInput(buffer: pcm))
        }
    }

    func finish() async throws -> String {
        input.finish()
        try await analyzer.finalizeAndFinishThroughEndOfInput()
        return try await results.value
    }

    func cancel() async {
        input.finish()
        await analyzer.cancelAndFinishNow()
        results.cancel()
    }
}

@main
struct Main {
    static func main() async {
        setvbuf(stdout, nil, _IONBF, 0)
        signal(SIGPIPE, SIG_IGN)
        guard #available(macOS 26.0, *) else {
            emit("error", "macOS 26 以降が必要です")
            exit(1)
        }
        let frames = startReader()
        guard let locale = await SpeechTranscriber.supportedLocale(equivalentTo: Locale(identifier: localeID)) else {
            emit("error", "\(localeID) に対応していません")
            exit(1)
        }
        do {
            try await ensureAsset(locale)
        } catch {
            emit("error", "モデルを準備できません: \(error.localizedDescription)")
            exit(1)
        }
        emit("ready")

        var session: Session? = nil
        var failed = false
        for await frame in frames {
            switch frame {
            case .audio(let data):
                if failed { continue }
                do {
                    if session == nil { session = try await Session(locale: locale) }
                    try session!.feed(data)
                } catch {
                    emit("error", "解析できません: \(error.localizedDescription)")
                    failed = true
                    if let s = session { await s.cancel() }
                    session = nil
                }
            case .end:
                var text = ""
                if let s = session {
                    do { text = try await s.finish() } catch {
                        emit("error", "確定できません: \(error.localizedDescription)")
                    }
                }
                session = nil
                failed = false
                emit("final", text)
            case .quit:
                if let s = session { await s.cancel() }
                exit(0)
            }
        }
        exit(0)
    }
}
