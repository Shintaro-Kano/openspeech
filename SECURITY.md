# セキュリティとデータの扱い(`SECURITY.md`)

OpenSpeech は読影レポートを口述するためのアプリです。話した内容や転送した文には、患者さんの情報が含まれることがあります。この文書は、アプリがこの Mac の外へ出すもの、この Mac に残すもの、インストーラがすることを、すべて書き出したものです。食い違いを見つけたら、下の「脆弱性の報告」から知らせてください。

## 1. 外へ出るもの(通信先の全部)

| 通信先 | いつ | 送るもの |
|---|---|---|
| `api.anthropic.com` | 設定の「AI 訂正」で API キーの接続を選び、キーを入れたときだけ | AI 訂正: 認識した文、直前の文(最大 200 字)、登録した単語(読み付き)、命令の言葉とテンプレートを呼ぶ語(そのまま残す語として。合わせて最大 400 語)、`guidance.md`(書いた場合。無ければこのアプリの既定の指示)、覚えた誤認識の組(最大 40 組)。「ヘイクロード」: 依頼の文、テンプレートの一覧(本文・所見の部品・使い方の注記を含む)、登録した単語(読み付き)、直前に補完した文。**音声は送りません。** |
| この Mac の Claude Code(`claude -p`)経由で Anthropic | 設定で「この Mac の Claude Code」を選んだときだけ(開発者向け) | 上と同じ文字。アプリは Claude Code を起動して標準入力で文を渡すだけで、サインインの情報には触れません。システムプロンプト(登録した単語・覚えた組を含む)は、所有者だけが読める一時ファイル(0600)で渡し、終わったら消します(コマンドの引数には出しません)。 |
| `api.openai.com`・`api.deepgram.com`・`generativelanguage.googleapis.com` のうち選んだ 1 つ | 設定の「認識」で「外部の音声認識 API」を選び、その API の鍵を入れたときだけ | **Haas が声と判定した発話(4 字以上)の音声**(16 kHz・mono の WAV)と、辞書の単語(用語のヒント。読みと句は送りません。設定で切れます)。OpenAI には場の説明 1 文、Gemini には書き起こしの指示も送ります。Deepgram には、モデルの改善に使わない指定(`mip_opt_out=true`)を毎回付けます。各社の扱いは README の「外部の音声認識 API」。 |
| `openspeech.unitedradiology.jp` → `huggingface.co`(とその CDN) | 「モデルを取得」を押したときだけ | ファイルの取得(GET)だけ。送るものはありません。`/usr/bin/curl -q` で取り、大きさと SHA-256 が実行ファイルに埋め込んだ表と合ったときだけ使います。https の取得先からは https にだけ転送を許します。 |
| `openspeech.unitedradiology.jp` | インストーラを実行したときだけ | ファイルの取得(GET)だけ。環境変数 `OPENSPEECH_BASE_URL` で取得先を変えた場合(GitHub Releases など)はそちら。 |

- **音声**はこの Mac の中で文字にします(Haas-ja、speech.cpp)。外へ出るのは、外部の音声認識 API を選んだときの上の分だけです。macOS 26 で「Mac 内蔵の音声認識」を選んだ場合も、端末内の SpeechAnalyzer を使います。日本語の資産が無ければ、補助プログラムが macOS に取得を頼みます(取得は macOS が Apple のサーバーから行います)。
- 利用状況の送信、クラッシュ報告の送信、更新の確認の通信はありません。
- API キーはコマンドの引数に出さず、所有者だけが読める一時ファイル(curl の設定)で渡し、終わったら消します。改行や空白・引用符を含むキーは受け付けません。
- 外部のコマンドは macOS の決まった場所のもの(`/usr/bin/curl` など)を起動します。例外は「この Mac の Claude Code」を選んだときの `claude` で、よく使われる置き場所(`~/.local/bin` など)、`/opt/homebrew/bin`・`/usr/local/bin`、PATH の順に探します。curl は `-q` で利用者の `~/.curlrc` を読みません。ただし環境変数の proxy の設定(`HTTPS_PROXY` など)には従います。

## 2. この Mac に残るもの

置き場所は `~/Library/Application Support/OpenSpeech/` です。

| ファイル | 中身 | 期限・消し方 |
|---|---|---|
| `global.json` | この Mac の設定(マイク、認識エンジン、AI 訂正の接続先など) | アプリを消しても残ります。 |
| `user.json` | 使い方の設定、覚えた誤認識の組 | 覚えた組は「辞書」の「学習」(設定の「学習」の「覚えた語を見る」)で 1 組ずつ、またはすべて消せます。 |
| `dictionary.json` | 登録した単語・テンプレート | 利用者が書くファイル。 |
| `guidance.md` | AI への補足(書いた場合だけ) | 利用者が書くファイル。 |
| `claude_api_key` | Anthropic の API キー(0600) | 設定の「AI 訂正」でキーを空にすると消えます。 |
| `openai_api_key`・`deepgram_api_key`・`gemini-flash_api_key` | 外部の音声認識 API の鍵(0600) | 設定の「認識」で鍵を空にすると消えます。 |
| `history/` | **転送した文**(日ごとのファイル `YYYY-MM-DD.log`、平文) | 90 日より前の分と、合計 5 MB を超える分は古い日から消えます。設定の「履歴とログ」で保存を切る・すべて消すことができます。 |
| `VoiceLog/` | 発話の録音 | **既定では保存しません。** オンにした場合は上限(既定 20 MB、設定で 5〜500 MB)を超えた分を古いものから消します。置き場所は設定で変えられます。保存をオフにしても、それまでの録音は残ります(フォルダを自分で消してください)。 |
| `logs/voice.log` | 動作の記録 | 話した文・覚えた語・Claude の返事は書かず、字数だけを書きます。1 MB を超えると前の分を `voice.old` に回します。 |
| `backup/auto/` | `user.json` と `dictionary.json` の写し | 最新 5 世代。 |
| `backup/guidance-*.md` | 書き方を改めたときの `guidance.md` の控え | 自分で消してください。 |
| `*.unreadable-*.json`・`dictionary.conflict-*.json` | 読めなかった設定や、外で書き換えられて食い違った辞書の控え | 自分で消してください。 |
| `app.ron` | 窓の位置と大きさなど(文章は入りません) | アプリを消しても残ります。 |
| `models/` | 認識と声の判定のモデル(約 0.66 GB)。取得の途中の `.part` も | アプリを消しても残ります。 |

- 一時フォルダに `openspeech-prompt-*`・`openspeech-key-*`・`openspeech-body-*`(外部の音声認識 API に送る音声と本文)が置かれるのは、送っている間だけです。アプリが途中で終わって残った分は、次に起動したときに消します。
- 転送で貼り付けを使うときは、クリップボードに「履歴に残さない」印(`org.nspasteboard.ConcealedType`)を付けます。元の中身(文字か画像)は、設定で戻すことにしている場合(既定)だけ、同じ印を付けて戻します。Apple の機器どうしで共有するクリップボード(Handoff)には、この印は効きません。
- このほか、macOS が `~/Library/Preferences/dev.openspeech.app.plist`(最後に選んだフォルダなど)を置きます。Dock に追加した場合は Dock の登録も残ります。
- すべて消すには、アプリを消したあとで `~/Library/Application Support/OpenSpeech` と上の設定ファイルを消し、Dock から外してください。

## 3. インストーラ(`install.sh`)

### `install.sh` がすること(全部)

1. Apple Silicon と macOS 15 以降であることを確かめます(違えば何もせずに終わります)。
2. `VERSION`、`openspeech-macos-arm64.tar.gz` とその `.sha256` を取り、アーカイブの SHA-256 を確かめます。
3. 一時フォルダに展開し、署名(`codesign --verify --deep --strict`)と同梱のライブラリを確かめます。
4. OpenSpeech が動いていれば、終了するよう案内して止まります(エディターの文章が消えないよう、自分では終わらせません)。
5. `/Applications` と `~/Applications` の両方に OpenSpeech があれば、片方を消すよう案内して止まります。
6. すでに入っている場所(無ければ `/Applications`、書けなければ `~/Applications`)に、新しい `OpenSpeech.app` を隣に置いて確かめてから入れ替えます。入れ替えに失敗したら古い版に戻します。
7. 更新のときは、アクセシビリティの許可を消します(`tccutil reset Accessibility dev.openspeech.app`)。署名が変わり、古い許可は使えないためです。
8. アプリを開きます。

### `install.sh` がしないこと

- 管理者のパスワードを求めること、ログイン項目・常駐プロセス・拡張機能を入れること。
- モデルを取ること(アプリの中で、利用者が押したときだけ取ります)。
- 隔離属性(quarantine)を付けること。`curl | bash` で取ったファイルには付かないため、Gatekeeper の確認は出ません。

インストーラが起動するのは macOS に入っているコマンド(`/usr/bin` などの下)だけです(PATH を決まった値にしてから動きます)。

### `uninstall.sh` がすること(全部)

1. OpenSpeech が動いていれば、終了するよう案内して止まります。
2. `/Applications` と `~/Applications` の `OpenSpeech.app` を消します。
3. アクセシビリティとマイクの許可を消します(`tccutil reset`)。
4. `~/Library/Application Support/OpenSpeech` は消さずに、場所と大きさ、消すためのコマンドを表示します。

## 4. コード署名

Apple の開発者証明書による署名と公証はしていません(ad-hoc 署名)。そのため:

- インストールは上の 1 行で行い、ブラウザから `.app` を落とす形は用意していません。
- 更新のたびに macOS が別のアプリとみなすので、アクセシビリティの許可をオンにし直す必要があります。マイクの許可は、次に使うときに macOS がもう一度尋ねます。
- 配布する `OpenSpeech.app` は GitHub Actions の公開されたワークフロー(`.github/workflows/release.yml`)で作ります。手元の絶対パスは実行ファイルに残しません。

## 5. 脆弱性の報告

GitHub の [Security Advisories](https://github.com/Shintaro-Kano/openspeech/security/advisories/new)(非公開)から知らせてください。患者さんの情報を含む画面や録音は送らないでください。
