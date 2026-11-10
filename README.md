# OpenSpeech

Mac 専用の、主に放射線科の読影レポート作成業務補助を想定した日本語音声入力アプリです。話した内容をこの Mac の中で文字にし、確かめてから、文字情報を送ります。

- 無料・オープンソース(MIT)・アカウント不要・利用状況の送信なし
- 対応: Apple Silicon の Mac、macOS 15 以降
- **医療機器ではありません** 
- **API keyの使用があります（任意）**

## 特徴

- **端末内の音声認識**: Haas-ja v1.0 を speech.cpp(Metal)で動かします。声の判定に Silero VAD を使い、物音を声と取り違えにくくしています。macOS 26 では Mac 内蔵の音声認識も選べます。音声は Mac の外に出ません(下の外部の音声認識 API を選んだときを除く)。
- **エディターで確かめてから転送**: 話した文はアプリの中のエディターに入ります。直してから ⌘Enter(または登録した「送信」の言葉)で、前面にあったアプリへ入れます(アクセシビリティ API で差し込み、できなければ貼り付け)。
- **AI 訂正(任意)**: 自分の Claude API キーを入れると、認識の誤りを Claude が直します。送るのは文字だけです。
- **外部の音声認識 API(任意)**: 自分の鍵で OpenAI、Deepgram、Gemini Flash(精度優先)のどれかを使うと、Haas の文を API の文に差し替えてから AI 訂正に回します。このときは音声を送ります(下の「外部の音声認識 API」。各社のデータの扱いもそこに)。
- **辞書・学習・テンプレート**: 登録した単語(読み付き)を認識の手がかりにし、手直しから「誤認識 → 正しい語」を覚えます。定型文(テンプレート)を声で呼び出せます。「ヘイクロード」と呼びかけると、テンプレートを改変して Claude が文章を作ります。文章を選んで「意味が一貫するように整えて」のように頼むと、直した文で置き換えます(どちらも任意)。公開のテンプレートライブラリ(GitHub)から他の利用者のテンプレートを取り込み、自分のものを共有できます(1.1.0)。
- **録音の切り替え**: Caps Lock(変えられます)。iPhone を連係のマイクとして使えます。

## インストール(Apple Silicon の Mac)

ターミナルで次の 1 行を実行します。

```bash
curl -fsSL https://openspeech.unitedradiology.jp/install.sh | bash
```

`OpenSpeech.app` を、すでに入っていればその場所に、無ければ `/Applications`(書けなければ `~/Applications`)に置いて開きます。インストーラがすることは [SECURITY.md](SECURITY.md) に全部書いてあります。

### 初回セットアップ

初めて開くと「はじめに」の案内が出ます。上から順に進めてください。

1. **マイクの許可**: マイクを入れると macOS が許可を尋ねます。「許可」を押します。
2. **認識のモデルの取得**: 「モデルを取得」を押すと、認識のモデル(約 656 MB)と声の判定のモデル(約 1.2 MB)を Mac に取ります(1 回だけ)。あとから設定の「認識」でも取れます。止まっても次は続きから取り、大きさと SHA-256 を確かめてから使います。
3. **転送の許可**: 文をほかのアプリへ入れるには、システム設定 → プライバシーとセキュリティ → アクセシビリティで OpenSpeech をオンにします。
4. **AI 訂正(使うときだけ)**: 設定の「AI 訂正」に Anthropic の API キーを入れます。キーは `~/Library/Application Support/OpenSpeech/claude_api_key` に所有者だけが読める形(0600)で置かれ、設定ファイルには入りません。使わなくても音声入力はできます。

### 更新

同じ 1 行をもう一度実行します。古い `OpenSpeech.app` を置き換え、アクセシビリティの許可を消します(このアプリは Apple の開発者証明書で署名していないため、更新のたびに macOS が別のアプリとみなします)。更新のあと、アクセシビリティの許可をもう一度オンにしてください。

### アンインストール

```bash
curl -fsSL https://openspeech.unitedradiology.jp/uninstall.sh | bash
```

アプリだけを消します。設定・辞書・学習・転送履歴・モデル(約 0.7 GB)は `~/Library/Application Support/OpenSpeech` に残ります。要らなければ、そのフォルダを自分で消してください。

## AI 訂正と Claude への接続

AI 訂正と「ヘイクロード」は、次のどちらかで Claude につなぎます。どちらも送るのは文字だけで、音声は送りません。

- **API キー(既定・推奨)**: Anthropic の API キーで `api.anthropic.com` に直接送ります。API の利用規約で扱われます。
- **Mac の Claude Code(開発者向け・自己責任)**: 利用者自身がサインインした Claude Code(`claude -p`)を呼び出します。アプリはサインインの情報に触れません。Anthropic の文書は、第三者のアプリが Claude.ai のログインを提供することを認めておらず、Agent SDK などでは API キーで認証するよう求めています。一方で、利用者が改変していない Claude Code に自分のサブスクリプションでサインインすることは妨げないとしています。アプリから Claude Code を呼び出すこの形は、文書に直接は書かれていません。使う場合は、[Claude Code の法的事項](https://code.claude.com/docs/en/legal-and-compliance) と [Agent SDK の概要](https://code.claude.com/docs/en/agent-sdk/overview) を読み、自分の判断で使ってください。

送るもの(AI 訂正): 認識した文、直前の文(最大 200 字)、登録した単語(読み付き)、命令の言葉とテンプレートを呼ぶ語(合わせて最大 400 語)、`guidance.md`(書いた場合。無ければこのアプリの既定の指示)、覚えた誤認識の組(最大 40 組)。送るもの(「ヘイクロード」): 依頼の文、エディターの文章(選んだ範囲、選んでいなければ全文。先頭 3000 字まで。文章の手直しを頼めるように毎回添えます)、テンプレートの一覧(本文・所見の部品・使い方の注記を含む)、登録した単語(読み付き)、直前に補完した文、`request.md`(書いた場合。無ければこのアプリの既定の指示)。

## 外部の音声認識 API(任意・自分の鍵で)

設定の「認識」で「外部の音声認識 API(Haas と併用)」を選び、API と鍵を入れると使えます。鍵が無ければ Haas だけで動きます。

| API | 送り先 | 1 発話の待ちの目安(この Mac での計測、2026-11-08、90% の発話がこれ以内) |
|---|---|---|
| OpenAI(gpt-transcribe) | `api.openai.com` | 約 1.1 秒 |
| Deepgram(nova-3) | `api.deepgram.com` | 約 0.3 秒 |
| Google(Gemini FLASH 精度優先) | `generativelanguage.googleapis.com` | 約 3.7 秒 |

- Haas が先に動き、その文をすぐ表示し、命令語(「改行」「送信」など)もその文で判定します。API の文が届いたら表示を差し替え、AI 訂正にはその文を回します(AI 訂正を使っているときは、訂正した文の文末の命令語も働きます。訂正のない場合と同じ)。
- **送るもの**: Haas が声と判定した発話(4 字以上)の音声(WAV)と、辞書の単語(用語のヒント。読みと句は送りません。設定で切れます)。咳や物音のような短い発話は送りません。
- 失敗・時間切れ(既定 15 秒)のとき、Haas の文に比べて長すぎる答え(2 倍 + 20 字を超える)や短すぎる答え(Haas の文が 12 字以上で、その半分未満)のときは、Haas の文で進みます。内容の突き合わせ(左右・否定など)はしません。
- 鍵は `~/Library/Application Support/OpenSpeech/<API>_api_key` に所有者だけが読める形(0600)で置きます。音声と鍵はコマンドの引数に出さず、送っている間だけの一時ファイルで渡します。
- 送った音声の扱いは、それぞれの API の規約によります。2026-11-08 に各社の原文で確かめた内容(要約。使う前に原文を確かめてください):
  - **OpenAI**: 2023-03-01 以降、API に送ったデータは、明示的に許可しない限りモデルの学習に使われません。乱用の監視のための記録は既定で 30 日まで残ります(書き起こしの API は、データの扱いの表で監視の保持が「なし」とされています)([Your data](https://developers.openai.com/api/docs/guides/your-data))。
  - **Deepgram**: 既定では、送った要求をモデルの改善に使い、保持します。このアプリはすべての要求に `mip_opt_out=true` を付けてこれを断ります(断った要求は学習に使われず、データは処理に要る間だけ保持されます)([Your data at Deepgram](https://developers.deepgram.com/trust-security/your-data)、[Terms](https://deepgram.com/terms))。
  - **Gemini**: 無料枠では、送った内容が Google の製品の提供・改善・開発に使われ、人が読んで注釈を付けることがあり、機微な情報・秘密・個人情報を送らないよう求めています。課金を有効にした Cloud プロジェクトの鍵なら改善には使われず、違反の検出のために限られた期間記録されます([Gemini API 追加利用規約](https://aistudio.google.com/docs/terms))。
- 外部の音声認識 API を使うか、どの API を使うかは、各社の規約を読んだうえで利用者が判断してください。

## プライバシーと外部通信先

- **音声**: この Mac の中で文字にします。外へ送るのは、外部の音声認識 API を選んだときの、声と判定した発話だけです。
- **文字**: AI 訂正か「ヘイクロード」を使うときだけ、Anthropic(API キー)か、Mac の Claude Code へ送ります。
- **モデルの取得**: 「モデルを取得」を押したときだけ、`huggingface.co` から取ります。
- **テンプレートの共有ライブラリ**(1.1.0): 辞書の「共有ライブラリから取り込む」を押したときだけ、`shintaro-kano.github.io`(GitHub Pages)から公開テンプレートの目録を取ります。「共有…」はブラウザで GitHub の投稿フォームを開くだけで、アプリは送信しません。
- 利用状況の送信や、更新の確認のための通信はありません。

この Mac に残るもの(置き場所と消し方)は [SECURITY.md](SECURITY.md) にあります。転送した文は日ごとのファイルに残ります(90 日・合計 5 MB まで。設定の「履歴とログ」で保存を切る・すべて消すことができます)。

## 既知の制約

- Intel の Mac では動きません(同梱のライブラリが Apple Silicon 専用)。macOS 15 より前は対応していません。Mac 内蔵の音声認識は macOS 26 でだけ選べます。
- 更新のたびにアクセシビリティの許可をオンにし直す必要があります(上の「更新」)。
- 録音の切り替えは既定で Caps Lock です。Caps Lock を入力の切り替えに使っている場合は、設定の「入力と転送」で別のキーにしてください。
- 認識も AI 訂正も誤ります(似た検査名の取り違えなど)。転送の前に必ず確かめてください。

## モデルの出所

- 認識: Haas-ja v1.0(Third Intelligence、Apache-2.0)の sakasegawa 氏による非公式 GGUF 変換(Hugging Face の [`sakasegawa/Haas-ja-v1.0-GGUF`](https://huggingface.co/sakasegawa/Haas-ja-v1.0-GGUF)。このアプリのサーバーに置いた同じファイルを先に試します)
- 声の判定: Silero VAD v6.2(Silero Team、MIT)の sakasegawa 氏による GGUF 変換(Hugging Face の [`sakasegawa/silero-vad-GGUF`](https://huggingface.co/sakasegawa/silero-vad-GGUF))

モデルはアプリに同梱せず、利用者が「モデルを取得」を押したときに取ります。第三者のソフトウェアとライセンスは [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) にまとめてあります。

## ソースからのビルド(開発者向け)

```bash
# Rust は rust-toolchain.toml の版(1.99.1)が使われます
cargo test
cargo clippy --all-targets -- -D warnings

# Mac 内蔵の音声認識の補助プログラム(macOS 26 の SDK が要ります。無くても他は動きます)
tools/openspeech-speech/build.sh

# 配る形のビルド(実行ファイルに手元の絶対パスを残さない)と、アプリの組み立て
scripts/build-release.sh
target/release/openspeech --install-app "$HOME/Applications"
```

- モデルが要る試験は `scripts/test-with-models.sh` で走らせます(モデルを取ったあと)。
- 診断用の引数は `openspeech --help` で一覧できます。

## 名前について

このアプリは、同じ名前の音声認識の研究用ツールキット(`openspeech-team/openspeech`)とは関係ありません。

## ライセンス

MIT License。[LICENSE](LICENSE) を見てください。同梱・利用しているソフトウェアのライセンスは [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) と [THIRD-PARTY-LICENSES.txt](THIRD-PARTY-LICENSES.txt) にあり、配る `OpenSpeech.app` の `Contents/Resources/` にも入っています。
