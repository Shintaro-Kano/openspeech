//! 単独のアプリとしての配置と、Mac のマイク許可への対応。
//!
//! Mac はマイクの利用を「起動元のアプリ」の権限で判断する。起動元にマイクの宣言が無いと、
//! 確認画面も出ずに無音だけが届く。そこで、マイクの宣言を持つ単独のアプリの形を作り、
//! Finder や Launchpad からも開けるようにする。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use super::speechcpp;

/// アプリの名前(表示用)。
pub const APP_NAME: &str = "OpenSpeech";
/// 音声認識の補助プログラム(あれば束の中に入れる)。
pub const HELPER_NAME: &str = "openspeech-speech";
const BUNDLE_NAME: &str = "OpenSpeech.app";
const EXECUTABLE_NAME: &str = "openspeech";
/// アプリのアイコン(icns)。束の Resources に入れる。
const APP_ICON: &[u8] = include_bytes!("../../assets/icon/OpenSpeech.icns");
/// ウィンドウ用のアイコン(64×64 の PNG)。
const WINDOW_ICON_PNG: &[u8] = include_bytes!("../../assets/icon/window_64.png");
/// 束の Resources に入れる第三者のライセンスの表示(名前, 中身)。配る束だけを受け取った人にも届くように。
/// このアプリ自身の `LICENSE` は著作者の名前を含むので実行ファイルには埋め込まず、束を作るときに写す
/// (`copy_own_license`)。
const LICENSE_FILES: [(&str, &[u8]); 4] = [
    ("THIRD-PARTY-NOTICES.md", include_bytes!("../../THIRD-PARTY-NOTICES.md")),
    ("THIRD-PARTY-LICENSES.txt", include_bytes!("../../THIRD-PARTY-LICENSES.txt")),
    ("speech-cpp-LICENSE.txt", include_bytes!("../../vendor/speech-cpp/LICENSE")),
    ("ggml-LICENSE.txt", include_bytes!("../../vendor/speech-cpp/LICENSE-ggml")),
];
const PRIVACY_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone";
const SOUND_SETTINGS_URL: &str =
    "x-apple.systempreferences:com.apple.Sound-Settings.extension?input";
/// Dock の並びを記録している設定。`persistent-apps` が左側(アプリ)の一覧。
const DOCK_DOMAIN: &str = "com.apple.dock";

/// マイクを使う理由(macOS が許可を尋ねる画面に出る)。外へ出るものを正直に書く。
const MIC_USAGE: &str = "OpenSpeech が、話した内容を文字にするためにマイクを使います。音声はこの Mac の中で文字にします。AI 訂正かテンプレート補完を使うときは文字を、外部の音声認識 API を選んだときは声と判定した発話の音声も、外のサービスへ送ります。";
/// 対応する macOS の下限(同梱の libspeech が 15.0 から。計画書 D9)。
const MINIMUM_SYSTEM: &str = "15.0";

/// 束の Info.plist。版は Cargo.toml の版(`CFBundleVersion` も同じ値にし、版を上げるたびに増える)。
fn info_plist() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleIdentifier</key><string>dev.openspeech.app</string>
	<key>CFBundleName</key><string>OpenSpeech</string>
	<key>CFBundleDisplayName</key><string>OpenSpeech</string>
	<key>CFBundleExecutable</key><string>openspeech</string>
	<key>CFBundleIconFile</key><string>OpenSpeech</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>{version}</string>
	<key>CFBundleVersion</key><string>{version}</string>
	<key>LSMinimumSystemVersion</key><string>{MINIMUM_SYSTEM}</string>
	<key>NSHighResolutionCapable</key><true/>
	<key>NSMicrophoneUsageDescription</key><string>{MIC_USAGE}</string>
	<key>NSAppleEventsUsageDescription</key><string>認識した文章を、入力先のアプリへ貼り付けるために使います。</string>
</dict>
</plist>
"#
    )
}

/// 開発中の実行ファイル(`target/debug/…`・`target/release/…`、試験の `target/debug/deps/…`)から見た repo の根。
/// 束の中や配布版では None。実行ファイルの上 4 段までで、`Cargo.toml` と `vendor/` のあるところ。
/// (開発機の絶対パスを実行ファイルに焼き込まないため、コンパイル時の場所は使わない。)
pub fn development_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if is_inside_app(&exe) {
        return None;
    }
    exe.ancestors()
        .skip(1)
        .take(4)
        .find(|dir| dir.join("Cargo.toml").is_file() && dir.join("vendor").is_dir())
        .map(Path::to_path_buf)
}

/// 束に入れる推論ライブラリの元。束の中で動いているなら自分の Frameworks、開発中なら vendor。
fn library_source() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if is_inside_app(&exe) {
        return exe.parent().map(|dir| dir.join("../Frameworks"));
    }
    speechcpp::vendored_library_dir()
}

/// アプリとして開き直す・アプリケーションフォルダに置く手段があるか。Mac で、まだ束の外から動いているときだけ。
/// 束の中から作ると、同じ識別子の 2 個目の束ができてしまう。
pub fn can_relaunch_as_app() -> bool {
    cfg!(target_os = "macos") && !running_as_app()
}

/// すでにアプリの形で動いているか。
pub fn running_as_app() -> bool {
    std::env::current_exe().is_ok_and(|exe| is_inside_app(&exe))
}

/// アプリとして動いているなら、その束(`….app`)。
pub fn current_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if !is_inside_app(&exe) {
        return None;
    }
    // …/X.app/Contents/MacOS/実行ファイル
    exe.parent()?.parent()?.parent().map(Path::to_path_buf)
}

fn is_inside_app(exe: &Path) -> bool {
    exe.to_string_lossy().contains(".app/Contents/MacOS/")
}

/// ウィンドウ用のアイコン(幅, 高さ, RGBA)。読めなければ透明な 1×1 を返す。
pub fn window_icon() -> (u32, u32, Vec<u8>) {
    match image::load_from_memory(WINDOW_ICON_PNG) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            (rgba.width(), rgba.height(), rgba.into_raw())
        }
        Err(_) => (1, 1, vec![0, 0, 0, 0]),
    }
}

/// 補助プログラムの置き場所の候補。実行ファイルの隣、次に開発時の tools の下。
fn helper_source() -> Option<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(HELPER_NAME)));
    let vendored = development_root().map(|root| root.join("tools/openspeech-speech").join(HELPER_NAME));
    [beside, vendored]
        .into_iter()
        .flatten()
        .find(|path| path.is_file())
}

/// 補助プログラムを束の MacOS に写す(任意。無ければ何もしない)。
/// 署名で中身が変わるので、元ファイルの指紋を stamp に残して比べる。
fn copy_helper(macos: &Path, resources: &Path) -> Result<bool, String> {
    let Some(source) = helper_source() else {
        return Ok(false);
    };
    let target = macos.join(HELPER_NAME);
    if is_same_file(&source, &target) {
        return Ok(false);
    }
    let bytes = std::fs::read(&source).map_err(|e| format!("アプリを作れません: {e}"))?;
    let stamp = fingerprint(&bytes).to_string();
    let stamp_changed = write_if_changed(&resources.join("helper.stamp"), stamp.as_bytes())?;
    if !(stamp_changed || !target.is_file()) {
        return Ok(false);
    }
    std::fs::write(&target, bytes).map_err(|e| format!("アプリを作れません: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("アプリを作れません: {e}"))?;
    }
    Ok(true)
}

/// このアプリの `LICENSE` の名前(束の Resources の中)。
const OWN_LICENSE: &str = "LICENSE.txt";

/// このアプリの `LICENSE` の元。開発中なら repo の `LICENSE`、束の中で動いているなら自分の Resources。
fn own_license_source() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    if is_inside_app(&exe) {
        exe.parent().map(|dir| dir.join("../Resources").join(OWN_LICENSE))
    } else {
        development_root().map(|root| root.join("LICENSE"))
    }
    .filter(|path| path.is_file())
}

/// このアプリの `LICENSE` を束の Resources に写す(同じ束を作り直すなら写さない)。
fn copy_own_license(resources: &Path, source: &Path) -> Result<bool, String> {
    let target = resources.join(OWN_LICENSE);
    if is_same_file(source, &target) {
        return Ok(false);
    }
    let bytes = std::fs::read(source).map_err(|e| format!("アプリを作れません: {e}"))?;
    write_if_changed(&target, &bytes)
}

/// 中身が違うときだけ書く。書いたら true。
fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<bool, String> {
    if std::fs::read(path).is_ok_and(|current| current == bytes) {
        return Ok(false);
    }
    std::fs::write(path, bytes).map_err(|e| format!("アプリを作れません: {e}"))?;
    Ok(true)
}

/// アプリの形を `root` の下に作り、中の実行ファイルを `exe` と同じものにする。
/// 署名は作り直すと許可が取り消されるので、中身が変わったときだけ更新する。
fn prepare_bundle(root: &Path, exe: &Path) -> Result<(PathBuf, bool), String> {
    // 推論ライブラリが無ければ、何も作らずに断る(ライブラリの無い束を残さない)。
    let library_dir = library_source()
        .filter(|dir| dir.join(speechcpp::LIBRARY_FILE).is_file())
        .ok_or("推論ライブラリ(libspeech)が見つからないので、アプリを作れません")?;
    // LICENSE も先に確かめる(表示の無い束・作りかけの束を残さない)。
    let license = own_license_source().ok_or("LICENSE が見つからないので、アプリを作れません")?;
    let bundle = root.join(BUNDLE_NAME);
    let macos = bundle.join("Contents/MacOS");
    let resources = bundle.join("Contents/Resources");
    let frameworks = bundle.join("Contents/Frameworks");
    for folder in [&macos, &resources, &frameworks] {
        std::fs::create_dir_all(folder).map_err(|e| format!("アプリを作れません: {e}"))?;
    }
    let target = macos.join(EXECUTABLE_NAME);
    let mut changed = write_if_changed(&bundle.join("Contents/Info.plist"), info_plist().as_bytes())?;
    changed |= write_if_changed(&resources.join("OpenSpeech.icns"), APP_ICON)?;
    for (name, bytes) in LICENSE_FILES {
        changed |= write_if_changed(&resources.join(name), bytes)?;
    }
    changed |= copy_own_license(&resources, &license)?;
    // アプリの中から実行しているなら、自分自身に上書きしない(中身が消えてしまう)。
    if !is_same_file(exe, &target) {
        // 署名すると実行ファイルの中身が変わるので、元のプログラムの指紋で比べる。
        let source = std::fs::read(exe).map_err(|e| format!("アプリを作れません: {e}"))?;
        let stamp = fingerprint(&source).to_string();
        let stamp_changed = write_if_changed(&resources.join("program.stamp"), stamp.as_bytes())?;
        if stamp_changed || !target.is_file() {
            std::fs::copy(exe, &target).map_err(|e| format!("アプリを作れません: {e}"))?;
            changed = true;
        }
    }
    changed |= copy_helper(&macos, &resources)?;
    changed |= remove_retired_files(&frameworks, &resources)?;
    changed |= copy_libraries(
        &frameworks,
        &resources,
        &library_dir,
        &[speechcpp::LIBRARY_FILE],
        "speech-libraries.stamp",
    )?;
    Ok((bundle, changed))
}

/// 1.0 より前の版の束にあった、外した認識エンジンの推論ライブラリとその stamp。束を作り直すときに消す。
const RETIRED_FRAMEWORKS: [&str; 2] = ["libsherpa-onnx-c-api.dylib", "libonnxruntime.dylib"];
const RETIRED_RESOURCES: [&str; 1] = ["libraries.stamp"];

/// 外した認識エンジンの残り物を束から消す。消したら true(署名し直す)。
fn remove_retired_files(frameworks: &Path, resources: &Path) -> Result<bool, String> {
    let mut removed = false;
    let retired = RETIRED_FRAMEWORKS
        .iter()
        .map(|name| frameworks.join(name))
        .chain(RETIRED_RESOURCES.iter().map(|name| resources.join(name)));
    for path in retired {
        if path.is_file() {
            std::fs::remove_file(&path).map_err(|e| format!("アプリを作れません: {e}"))?;
            removed = true;
        }
    }
    Ok(removed)
}

/// 推論ライブラリ(開発時の vendor にあるもの)をアプリ束の Frameworks に写す。
/// 署名で中身が変わるので、元ファイルの指紋を stamp に残して比べる。元が無ければ失敗にする
/// (ライブラリの無い束を作ると、同じ識別子の壊れたアプリが増える)。
fn copy_libraries(
    frameworks: &Path,
    resources: &Path,
    source_dir: &Path,
    names: &[&str],
    stamp_file: &str,
) -> Result<bool, String> {
    let sources: Vec<PathBuf> = names
        .iter()
        .map(|name| source_dir.join(name))
        .collect();
    if let Some(missing) = sources.iter().find(|path| !path.is_file()) {
        return Err(format!(
            "推論ライブラリが見つからないので、アプリを作れません: {}",
            missing.display()
        ));
    }
    let mut stamp = String::new();
    let mut contents = Vec::new();
    for source in &sources {
        let bytes = std::fs::read(source).map_err(|e| format!("アプリを作れません: {e}"))?;
        stamp.push_str(&format!("{}\n", fingerprint(&bytes)));
        contents.push(bytes);
    }
    // 束の中から同じ束を作り直すときは、元が自分の Frameworks(署名済み)なので写さない。
    // 写すと stamp が変わって署名し直すことになり、許可が切れる。
    if names
        .iter()
        .zip(&sources)
        .all(|(name, source)| is_same_file(source, &frameworks.join(name)))
    {
        return Ok(false);
    }
    let stamp_changed = write_if_changed(&resources.join(stamp_file), stamp.as_bytes())?;
    let missing = names
        .iter()
        .any(|name| !frameworks.join(name).is_file());
    if !(stamp_changed || missing) {
        return Ok(false);
    }
    for (name, bytes) in names.iter().zip(contents) {
        std::fs::write(frameworks.join(name), bytes)
            .map_err(|e| format!("アプリを作れません: {e}"))?;
    }
    Ok(true)
}

fn is_same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// 内容の指紋(FNV-1a)。
fn fingerprint(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// 利用者のアプリケーションフォルダ。Spotlight と Launchpad が見つけられる場所。
pub fn applications_folder() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join("Applications"))
        .ok_or_else(|| "ホームフォルダが見つかりません".to_string())
}

/// 指定の場所にアプリを置く。すでに同じものがあれば何もしない。
pub fn install_app(folder: &Path) -> Result<PathBuf, String> {
    // 普通の `cargo build` の実行ファイルには、依存 crate の場所として手元の絶対パスが残る。
    // 束にするのは `scripts/build-release.sh` で組んだもの(印 `OPENSPEECH_RELEASE_BUILD`)だけ。
    if option_env!("OPENSPEECH_RELEASE_BUILD").is_none() {
        return Err("scripts/build-release.sh で組んだ実行ファイルから入れてください(この実行ファイルには手元のパスが残ります)".to_string());
    }
    // フォルダは prepare_bundle が、ライブラリを確かめてから作る(断るときに空のフォルダを残さない)。
    let exe = std::env::current_exe().map_err(|e| format!("アプリを作れません: {e}"))?;
    let (bundle, changed) = prepare_bundle(folder, &exe)?;
    if changed {
        sign(&bundle)?;
    }
    Ok(bundle)
}

/// Dock に登録する束。アプリとして動いているならその束、そうでなければアプリケーションフォルダへ置いた束
/// (動いている束の写しは作らない)。
pub fn bundle_for_dock() -> Result<PathBuf, String> {
    match current_bundle() {
        Some(bundle) => Ok(bundle),
        None => applications_folder().and_then(|folder| install_app(&folder)),
    }
}

/// Finder でアプリの場所を見せる。
pub fn reveal_in_finder(path: &Path) -> Result<(), String> {
    Command::new("/usr/bin/open")
        .arg("-R")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Finder を開けません: {e}"))
}

/// 既定のテキストエディタでファイルを開く。
pub fn open_in_text_editor(path: &Path) -> Result<(), String> {
    let status = Command::new("/usr/bin/open")
        .arg("-t")
        .arg(path)
        .status()
        .map_err(|e| format!("テキストエディットを開けません: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("テキストエディットを開けません".to_string())
    }
}

/// 署名に使う身元。キーチェーンに "OpenSpeech" を含むコード署名用の証明書があればその名前、無ければ "-"(ad-hoc)。
/// ad-hoc 署名はビルドのたびに変わるため、作り直すたびに macOS がアクセシビリティやマイクの許可を忘れてしまう。
/// 自作の "OpenSpeech" コード署名証明書(キーチェーンアクセス → 証明書アシスタント → 証明書を作成…、
/// 種類は「コード署名」)を使えば、作り直しても許可が保たれる。ここでは探すだけで、証明書の作成や信頼設定の変更はしない。
/// 自作の証明書は信頼の設定をしなくても署名に使えるが、`-v`(有効なものだけ)の一覧には出ないので全部の一覧から探す。
/// 返すのは署名に渡す値: 証明書の SHA-1(同名の証明書が複数あっても一つに決まる)、見つからなければ ad-hoc の "-"。
pub fn signing_identity() -> String {
    let listing = Command::new("/usr/bin/security")
        .args(["find-identity", "-p", "codesigning"])
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    identity_from_listing(&listing).unwrap_or_else(|| "-".to_string())
}

/// 署名の種類を知らせる一行(インストールのあとに表示する)。
pub fn signing_notice() -> String {
    match signing_identity().as_str() {
        "-" => "署名: ad-hoc。作り直すたびにアクセシビリティの許可を取り直す必要がある".to_string(),
        hash => format!("署名: 証明書 OpenSpeech({hash})。作り直しても許可は保たれる"),
    }
}

/// `security find-identity` の出力から、"OpenSpeech" を含む名前の証明書の SHA-1 を取り出す。
/// 行の形は `  1) <SHA-1> "名前" (<不備>)`。信頼されていないだけ(CSSMERR_TP_NOT_TRUSTED)の自作の証明書は使えるので通し、
/// 期限切れや失効など他の不備があるものは飛ばす。
fn identity_from_listing(listing: &str) -> Option<String> {
    listing.lines().find_map(|line| {
        let (head, rest) = line.split_once('"')?;
        let (name, status) = rest.split_once('"')?;
        if !name.contains("OpenSpeech") {
            return None;
        }
        let status = status.trim();
        if !(status.is_empty() || status == "(CSSMERR_TP_NOT_TRUSTED)") {
            return None;
        }
        let hash = head.trim().rsplit(' ').next()?;
        (!hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit())).then(|| hash.to_string())
    })
}

/// 実行中のアプリ束の署名が ad-hoc か。束の外(cargo run、試験)なら None。`codesign` を起動するので一度だけ調べる。
/// ad-hoc だと作り直すたびに署名が変わり、アクセシビリティの許可が別のアプリへの許可になって消える。
pub fn running_signature_is_adhoc() -> Option<bool> {
    static ADHOC: OnceLock<Option<bool>> = OnceLock::new();
    *ADHOC.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        // Contents/MacOS/openspeech → OpenSpeech.app
        let bundle = exe.parent()?.parent()?.parent()?;
        if bundle.extension()? != "app" {
            return None;
        }
        let output = Command::new("/usr/bin/codesign").arg("-dv").arg(bundle).output().ok()?;
        Some(signature_is_adhoc(&String::from_utf8_lossy(&output.stderr)))
    })
}

/// `codesign -dv` の出力(stderr)から ad-hoc 署名かを読む。
fn signature_is_adhoc(listing: &str) -> bool {
    listing.lines().any(|line| line.trim() == "Signature=adhoc")
}

/// アプリ束に署名する。中の共有ライブラリと補助プログラムも先に 1 つずつ署名する(束の署名は中身まで及ばない)。
fn sign(bundle: &Path) -> Result<(), String> {
    let identity = signing_identity();
    let helper = bundle.join("Contents/MacOS").join(HELPER_NAME);
    if helper.is_file() {
        codesign(&helper, &identity)?;
    }
    if let Ok(entries) = std::fs::read_dir(bundle.join("Contents/Frameworks")) {
        for path in entries.flatten().map(|entry| entry.path()) {
            if path.extension().is_some_and(|ext| ext == "dylib") {
                codesign(&path, &identity)?;
            }
        }
    }
    codesign(bundle, &identity)
}

fn codesign(path: &Path, identity: &str) -> Result<(), String> {
    let status = Command::new("/usr/bin/codesign")
        .args(["--force", "--sign", identity])
        .arg(path)
        .status()
        .map_err(|e| format!("アプリに署名できません: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("アプリに署名できませんでした".to_string())
    }
}

/// アプリケーションフォルダにアプリを置いて開く。呼び出し側はこのあと終了する。
/// 全体ショートカットを渡すため、少し待ってから開く。
pub fn relaunch_as_app() -> Result<(), String> {
    let bundle = install_app(&applications_folder()?)?;
    Command::new("/bin/sh")
        .args(["-c", "/bin/sleep 1; exec /usr/bin/open -n \"$1\"", "sh"])
        .arg(&bundle)
        .spawn()
        .map_err(|e| format!("アプリを開けません: {e}"))?;
    Ok(())
}

/// システム設定のマイクの画面を開く。
pub fn open_privacy_settings() -> Result<(), String> {
    Command::new("/usr/bin/open")
        .arg(PRIVACY_SETTINGS_URL)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("システム設定を開けません: {e}"))
}

/// システム設定のサウンド(入力)の画面を開く。iPhone のマイクが現れているかを確かめられる。
pub fn open_sound_settings() -> Result<(), String> {
    Command::new("/usr/bin/open")
        .arg(SOUND_SETTINGS_URL)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("システム設定を開けません: {e}"))
}

// ---- Dock ----

/// URL に使えない文字をパーセント符号化する。`/` は区切りとして残す。
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Dock がアプリを記録する形の URL(`file:///.../OpenSpeech.app/`)。末尾の `/` 付きで記録される。
fn dock_url(bundle: &Path) -> String {
    let path = bundle
        .canonicalize()
        .unwrap_or_else(|_| bundle.to_path_buf());
    format!("file://{}/", percent_encode(&path.to_string_lossy()))
}

/// Dock の一覧に足す 1 項目(plist の dict)。
fn dock_entry(bundle: &Path) -> String {
    format!(
        "<dict><key>tile-data</key><dict><key>file-data</key><dict>\
         <key>_CFURLString</key><string>{}</string>\
         <key>_CFURLStringType</key><integer>15</integer>\
         </dict></dict><key>tile-type</key><string>file-tile</string></dict>",
        dock_url(bundle)
    )
}

/// `%XX` を元の文字に戻す。
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let hex = (bytes[index] == b'%' && index + 2 < bytes.len())
            .then(|| std::str::from_utf8(&bytes[index + 1..index + 3]).ok())
            .flatten()
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match hex {
            Some(value) => {
                out.push(value);
                index += 3;
            }
            None => {
                out.push(bytes[index]);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `defaults read` の出力から、記録されているアプリの場所を取り出す。
fn dock_paths(output: &str) -> Vec<PathBuf> {
    output
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once("_CFURLString\" = \"")?;
            let (url, _) = rest.split_once('"')?;
            let path = percent_decode(url.trim_start_matches("file://").trim_end_matches('/'));
            Some(PathBuf::from(path))
        })
        .collect()
}

/// 同じ場所を指すか。Mac のファイルシステムは濁点の合成形と分解形などを同じ名前として扱い、
/// Dock は別の綴りで書き戻すことがあるので、実在する場所は canonicalize で揃えてから比べる。無ければ大文字小文字を問わず比べる。
fn same_location(a: &Path, b: &Path) -> bool {
    if is_same_file(a, b) {
        return true;
    }
    let text = |path: &Path| path.to_string_lossy().trim_end_matches('/').to_lowercase();
    text(a) == text(b)
}

/// `defaults read` の出力に、このアプリの場所があるか。
fn dock_lists(output: &str, bundle: &Path) -> bool {
    dock_paths(output)
        .iter()
        .any(|path| same_location(path, bundle))
}

/// すでに Dock に置いてあるか。
pub fn is_in_dock(bundle: &Path) -> bool {
    Command::new("/usr/bin/defaults")
        .args(["read", DOCK_DOMAIN, "persistent-apps"])
        .output()
        .is_ok_and(|output| dock_lists(&String::from_utf8_lossy(&output.stdout), bundle))
}

/// Dock にアプリを足す。足したら true、すでにあれば false。
/// Dock は設定を起動時に読むので、開き直して反映させる。
pub fn add_to_dock(bundle: &Path) -> Result<bool, String> {
    if !bundle.is_dir() {
        return Err("アプリがまだ作られていません".to_string());
    }
    if is_in_dock(bundle) {
        return Ok(false);
    }
    let status = Command::new("/usr/bin/defaults")
        .args([
            "write",
            DOCK_DOMAIN,
            "persistent-apps",
            "-array-add",
            &dock_entry(bundle),
        ])
        .status()
        .map_err(|e| format!("Dock に追加できません: {e}"))?;
    if !status.success() {
        return Err("Dock に追加できませんでした".to_string());
    }
    let restarted = Command::new("/usr/bin/killall")
        .arg("Dock")
        .status()
        .map_err(|e| format!("Dock を開き直せません: {e}"))?;
    if !restarted.success() {
        return Err("Dock を開き直せませんでした。ログインし直すと反映されます".to_string());
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn adhoc_is_read_from_the_codesign_listing() {
        assert!(super::signature_is_adhoc(
            "Executable=/x\nIdentifier=dev.openspeech.app\nSignature=adhoc\n"
        ));
        assert!(!super::signature_is_adhoc(
            "Identifier=dev.openspeech.app\nAuthority=OpenSpeech\nSignature size=4567\n"
        ));
        assert!(!super::signature_is_adhoc(""));
        assert_eq!(super::running_signature_is_adhoc(), None, "試験は束の外で走る");
    }

    #[test]
    fn the_bundle_id_used_for_the_permission_reset_matches_the_plist() {
        let tag = format!("<string>{}</string>", crate::core::transfer::BUNDLE_ID);
        assert!(super::info_plist().contains(&tag));
    }

    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mic-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn app_paths_are_recognized() {
        assert!(is_inside_app(Path::new(
            "/x/OpenSpeech.app/Contents/MacOS/openspeech"
        )));
        assert!(!is_inside_app(Path::new("/x/target/debug/openspeech")));
    }

    /// 実際のプログラムで作って署名を確かめる手動の試験。
    /// 使い方: BUNDLE_TEST_DIR=<置き場所> cargo test bundle_signs -- --ignored
    #[test]
    #[ignore = "本物の実行ファイルで束を作って署名する(BUNDLE_TEST_DIR が要る)"]
    fn bundle_signs_with_the_real_program() {
        let root = PathBuf::from(std::env::var("BUNDLE_TEST_DIR").unwrap());
        let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/openspeech");
        let (bundle, _) = prepare_bundle(&root, &exe).unwrap();
        sign(&bundle).unwrap();
        println!("{}", bundle.display());
    }

    #[test]
    fn signing_does_not_make_the_bundle_look_changed() {
        let root = temp("signed");
        let exe = root.join("program");
        std::fs::write(&exe, b"one").unwrap();
        let (bundle, _) = prepare_bundle(&root, &exe).unwrap();
        // 署名で中の実行ファイルが書き換わった状態を作る。
        let inside = bundle.join("Contents/MacOS").join(EXECUTABLE_NAME);
        std::fs::write(&inside, b"one+signature").unwrap();
        assert!(!prepare_bundle(&root, &exe).unwrap().1);
        assert_eq!(std::fs::read(&inside).unwrap(), b"one+signature");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn running_from_inside_the_bundle_keeps_the_program() {
        let root = temp("self");
        let exe = root.join("program");
        std::fs::write(&exe, b"one").unwrap();
        let (bundle, _) = prepare_bundle(&root, &exe).unwrap();
        let inside = bundle.join("Contents/MacOS").join(EXECUTABLE_NAME);
        assert!(!prepare_bundle(&root, &inside).unwrap().1);
        assert_eq!(std::fs::read(&inside).unwrap(), b"one");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn app_icon_is_an_icns_file() {
        assert_eq!(&APP_ICON[..4], b"icns");
        assert_eq!(
            u32::from_be_bytes(APP_ICON[4..8].try_into().unwrap()) as usize,
            APP_ICON.len()
        );
    }

    #[test]
    fn window_icon_is_64_square_with_visible_pixels() {
        let (width, height, rgba) = window_icon();
        assert_eq!((width, height), (64, 64));
        assert_eq!(rgba.len(), 64 * 64 * 4);
        assert!(rgba.chunks(4).any(|pixel| pixel[3] > 0));
    }

    #[test]
    fn signing_identity_is_adhoc_or_a_certificate_hash() {
        let identity = signing_identity();
        assert!(identity == "-" || identity.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn identity_is_the_hash_of_the_openspeech_certificate() {
        let listing = "  1) ABCDEF0123 \"Apple Development: x\"\n  2) 0123456789 \"OpenSpeech\"\n     2 valid identities found\n";
        assert_eq!(
            identity_from_listing(listing).as_deref(),
            Some("0123456789")
        );
        assert_eq!(
            identity_from_listing("     0 valid identities found\n"),
            None
        );
    }

    #[test]
    fn an_untrusted_self_signed_certificate_is_accepted_but_other_defects_are_not() {
        // キーチェーンアクセスで作った自作の証明書は、信頼の設定をしないとこう出る
        let untrusted = "  1) 5D45205CA1D400AACAFF7BABCD14A64A3528D903 \"OpenSpeech\" (CSSMERR_TP_NOT_TRUSTED)\n     1 identities found\n";
        assert_eq!(
            identity_from_listing(untrusted).as_deref(),
            Some("5D45205CA1D400AACAFF7BABCD14A64A3528D903")
        );
        let expired = "  1) 5D45205CA1D400AACAFF7BABCD14A64A3528D903 \"OpenSpeech\" (CSSMERR_TP_CERT_EXPIRED)\n     1 identities found\n";
        assert_eq!(identity_from_listing(expired), None);
        // 期限切れの古い証明書より、あとに作った使えるものを選ぶ
        let both = "  1) AAAA \"OpenSpeech\" (CSSMERR_TP_CERT_EXPIRED)\n  2) BBBB \"OpenSpeech\" (CSSMERR_TP_NOT_TRUSTED)\n     2 identities found\n";
        assert_eq!(identity_from_listing(both).as_deref(), Some("BBBB"));
    }

    #[test]
    fn dock_url_is_percent_encoded_with_a_trailing_slash() {
        let url = dock_url(Path::new("/Users/me/Applications/OpenSpeech.app"));
        assert_eq!(url, "file:///Users/me/Applications/OpenSpeech.app/");
        assert_eq!(percent_encode("Google Chrome.app"), "Google%20Chrome.app");
    }

    #[test]
    fn dock_entry_is_a_file_tile_for_the_bundle() {
        let entry = dock_entry(Path::new("/Applications/Foo.app"));
        assert!(entry.starts_with("<dict><key>tile-data</key>"));
        assert!(entry.contains("<string>file:///Applications/Foo.app/</string>"));
        assert!(entry.contains("<string>file-tile</string>"));
    }

    #[test]
    fn dock_listing_is_matched_regardless_of_case_and_slash() {
        let bundle = Path::new("/Users/me/Applications/OpenSpeech.app");
        let listed = "\"_CFURLString\" = \"file:///Users/me/Applications/openspeech.app/\";";
        assert!(dock_lists(listed, bundle));
        assert!(!dock_lists(
            "\"_CFURLString\" = \"file:///Applications/Safari.app/\";",
            bundle
        ));
        assert_eq!(percent_decode("Google%20Chrome.app"), "Google Chrome.app");
        assert_eq!(percent_decode("100%"), "100%");
    }

    /// Dock は大文字小文字の違う名前で書き戻すことがある。実在する場所なら同じと分かること。
    #[cfg(target_os = "macos")]
    #[test]
    fn dock_listing_matches_the_spelling_the_dock_writes_back() {
        let root = temp("dock");
        let bundle = root.join("OpenSpeech.app");
        std::fs::create_dir_all(&bundle).unwrap();
        let decomposed = root.join("openspeech.app");
        let listed = format!(
            "\"_CFURLString\" = \"file://{}/\";",
            percent_encode(&decomposed.to_string_lossy())
        );
        assert!(dock_lists(&listed, &bundle));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bundle_declares_the_microphone() {
        let plist = info_plist();
        assert!(plist.contains("NSMicrophoneUsageDescription"));
        assert!(plist.contains(&format!("<string>{EXECUTABLE_NAME}</string>")));
        // 版は Cargo.toml から、下限は 15.0。
        let version = format!("<string>{}</string>", env!("CARGO_PKG_VERSION"));
        assert_eq!(plist.matches(&version).count(), 2, "CFBundleShortVersionString と CFBundleVersion");
        assert!(plist.contains("<key>LSMinimumSystemVersion</key><string>15.0</string>"));
        // 音声が外へ出るのは、外部の音声認識 API を選んだときだけ、と書く。
        assert!(MIC_USAGE.contains("外部の音声認識 API を選んだとき"));
        assert!(!MIC_USAGE.contains("中だけ"), "文字は外へ出ることがある");
    }

    #[test]
    fn bundle_is_only_updated_when_the_program_changes() {
        let root = temp("bundle");
        let exe = root.join("program");
        std::fs::write(&exe, b"one").unwrap();
        let (bundle, changed) = prepare_bundle(&root, &exe).unwrap();
        assert!(changed);
        assert!(bundle.join("Contents/Info.plist").is_file());
        // ライセンスの表示も束に入る(配る束だけを受け取った人にも届く)。
        for (name, bytes) in LICENSE_FILES {
            assert_eq!(std::fs::read(bundle.join("Contents/Resources").join(name)).unwrap(), bytes, "{name}");
        }
        let own = std::fs::read_to_string(bundle.join("Contents/Resources").join(OWN_LICENSE)).unwrap();
        assert!(own.starts_with("MIT License"), "{own}");
        assert_eq!(
            std::fs::read(bundle.join("Contents/MacOS").join(EXECUTABLE_NAME)).unwrap(),
            b"one"
        );
        assert!(!prepare_bundle(&root, &exe).unwrap().1);
        std::fs::write(&exe, b"two").unwrap();
        assert!(prepare_bundle(&root, &exe).unwrap().1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_bundle_gets_the_inference_library_and_drops_retired_ones() {
        assert!(
            speechcpp::vendored_library_dir().is_some_and(|dir| dir.join(speechcpp::LIBRARY_FILE).is_file()),
            "vendor/speech-cpp の dylib は repo にある"
        );
        let root = temp("bundle-libraries");
        let exe = root.join("program");
        std::fs::write(&exe, b"one").unwrap();
        // 1.0 より前の版の束に残っている、外した認識エンジンのライブラリ。
        let contents = root.join(BUNDLE_NAME).join("Contents");
        let (frameworks, resources) = (contents.join("Frameworks"), contents.join("Resources"));
        let plant_retired = || {
            std::fs::create_dir_all(&frameworks).unwrap();
            std::fs::create_dir_all(&resources).unwrap();
            for name in RETIRED_FRAMEWORKS {
                std::fs::write(frameworks.join(name), b"old").unwrap();
            }
            for name in RETIRED_RESOURCES {
                std::fs::write(resources.join(name), b"old").unwrap();
            }
        };
        plant_retired();
        let (bundle, _) = prepare_bundle(&root, &exe).unwrap();
        assert_eq!(bundle.join("Contents"), contents);
        assert!(frameworks.join(speechcpp::LIBRARY_FILE).is_file());
        for name in RETIRED_FRAMEWORKS {
            assert!(!frameworks.join(name).exists(), "外したエンジンのものは消す: {name}");
        }
        for name in RETIRED_RESOURCES {
            assert!(!resources.join(name).exists(), "{name}");
        }
        let names: Vec<_> = std::fs::read_dir(&frameworks).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(names.len(), 1, "{names:?}");
        // 古いものだけが残っていた束も「変わった」として署名し直す。
        assert!(!prepare_bundle(&root, &exe).unwrap().1, "何も変わらなければ署名し直さない");
        plant_retired();
        assert!(prepare_bundle(&root, &exe).unwrap().1);
        // ライブラリが消えていても写し直す(署名でも中身が変わるので、指紋は元ファイルで比べている)。
        std::fs::remove_file(frameworks.join(speechcpp::LIBRARY_FILE)).unwrap();
        assert!(prepare_bundle(&root, &exe).unwrap().1);
        assert!(frameworks.join(speechcpp::LIBRARY_FILE).is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn libraries_are_copied_again_only_when_the_source_changes() {
        let root = temp("copy-libraries");
        let (source, frameworks, resources) =
            (root.join("source"), root.join("frameworks"), root.join("resources"));
        for folder in [&source, &frameworks, &resources] {
            std::fs::create_dir_all(folder).unwrap();
        }
        let copy = || copy_libraries(&frameworks, &resources, &source, &["a.dylib"], "a.stamp");
        assert!(copy().is_err(), "元が無ければ失敗にする(ライブラリの無い束を作らない)");
        std::fs::write(source.join("a.dylib"), b"one").unwrap();
        assert!(copy().unwrap());
        assert_eq!(std::fs::read(frameworks.join("a.dylib")).unwrap(), b"one");
        // 署名で束の中身が変わっても、元が同じなら写し直さない。
        std::fs::write(frameworks.join("a.dylib"), b"signed").unwrap();
        assert!(!copy().unwrap());
        std::fs::write(source.join("a.dylib"), b"two").unwrap();
        assert!(copy().unwrap());
        assert_eq!(std::fs::read(frameworks.join("a.dylib")).unwrap(), b"two");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rebuilding_a_bundle_from_its_own_libraries_does_not_touch_it() {
        // 束の中から同じ束を作り直すと、写す元が自分の Frameworks(署名済み)になる。stamp は未署名のファイルから
        // 書いたものなので食い違うが、写し直す(=署名し直して許可を切る)ことはしない。
        let root = temp("copy-libraries-self");
        let (frameworks, resources) = (root.join("frameworks"), root.join("resources"));
        for folder in [&frameworks, &resources] {
            std::fs::create_dir_all(folder).unwrap();
        }
        std::fs::write(frameworks.join("a.dylib"), b"signed").unwrap();
        std::fs::write(resources.join("a.stamp"), b"unsigned-fingerprint\n").unwrap();
        let changed =
            copy_libraries(&frameworks, &resources, &frameworks, &["a.dylib"], "a.stamp").unwrap();
        assert!(!changed);
        assert_eq!(std::fs::read(resources.join("a.stamp")).unwrap(), b"unsigned-fingerprint\n");
        assert_eq!(std::fs::read(frameworks.join("a.dylib")).unwrap(), b"signed");
        let _ = std::fs::remove_dir_all(&root);
    }
}
