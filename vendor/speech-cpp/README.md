# speech.cpp 0.10.1(macOS arm64 共有ライブラリ)

Haas-ja v1.0 を動かすための推論ライブラリ(ggml、Metal)。
出所: https://github.com/nyosegawa/speech.cpp/releases/tag/v0.10.1 の
`speech-0.10.1-macos-arm64-metal.zip`(SHA-256 `ff24f6f58c9d2dedb01caa88ba043ae845c76eff228e4cbfae5de749174404c1`、
リリースの SHA256SUMS と一致)から写した(2026-11-06)。
ライセンスは MIT。アーカイブには入っていないので、同じタグのリポジトリから `LICENSE` を写した。
ライブラリは ggml(MIT、https://github.com/nyosegawa/ggml.git の `speech.cpp` 枝)を静的に含む。その表示は `LICENSE-ggml`。

- `lib/libspeech.4.dylib` … C API(API 4.1、SOVERSION 4)。依存はシステムのものだけで、1 ファイルで足りる。
- `include/speech.h` … `src/core/speechcpp.rs` の関数の形はこのヘッダーを写している。
  公開の型は不透明で、構造体の並びを写す必要は無い。ライブラリを更新するときは関数の形と API の版を見比べる
  (`SPEECH_API_VERSION_MAJOR` が変わると読み込みを拒む)。

アプリは `src/core/speechcpp.rs` が実行時に dlopen で読む(ビルド時のリンクはしない)。
探す順: アプリ束の `Contents/Frameworks/`、次にこの `vendor/speech-cpp/lib/`(開発時)。
`--install-app` が `Contents/Frameworks/` に写して署名する。

モデルはここに置かない。アプリが設定の「認識」の「モデルを取得」で、データのフォルダの `models/haas-ja/`
(`Haas-616M-ja-v1.0-Q8_0.gguf`、0.66 GB)と `models/silero-vad/` に取る(取得元と SHA-256 は `assets/models.json`)。
Haas-ja v1.0(Third Intelligence、Apache-2.0)の sakasegawa 氏による非公式 GGUF 変換を使っている。
