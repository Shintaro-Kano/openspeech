#!/bin/bash
# OpenSpeech installer — https://openspeech.unitedradiology.jp
#
# What this script does (the complete list):
#   1. Checks the Mac: macOS, Apple Silicon (also under Rosetta), macOS 15 or later
#   2. Downloads VERSION, the app archive and its SHA-256 file, and checks the archive against it
#   3. Unpacks into a temporary folder and checks the signature and the bundled library
#   4. Stops if OpenSpeech is running (it asks you to quit it; text in its editor is not saved)
#   5. Installs OpenSpeech.app where it already is, otherwise into /Applications
#      (or ~/Applications if /Applications is not writable). The old copy is replaced only after the
#      new one is in place and checked, and is put back if anything fails.
#   6. On an update: resets the Accessibility permission (the ad-hoc signature changes with every version)
#   7. Opens the app
# It does not install LaunchAgents or anything else, and does not add a quarantine attribute.
#
# Update: run the same command again.
# Remove: curl -fsSL https://openspeech.unitedradiology.jp/uninstall.sh | bash
set -euo pipefail
# Only the system's own commands (a PATH set up by the user or another tool is not used).
export PATH=/usr/bin:/bin:/usr/sbin:/sbin

BUNDLE_ID="dev.openspeech.app"
APP_NAME="OpenSpeech.app"
ARCHIVE="openspeech-macos-arm64.tar.gz"

msg()  { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m注意:\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31mエラー:\033[0m %s\n' "$*" >&2; exit 1; }

# A running copy of the app (any install location), for this user only.
app_running() {
  pgrep -U "$(id -u)" -f '/OpenSpeech\.app/Contents/MacOS/openspeech( |$)' >/dev/null 2>&1
}

# The bundle is complete and its signature holds.
bundle_ok() {
  [ -f "$1/Contents/Frameworks/libspeech.4.dylib" ] \
    && [ -x "$1/Contents/MacOS/openspeech" ] \
    && codesign --verify --deep --strict "$1" 2>/dev/null
}

# Removes what this run left behind. If the old copy was moved aside and the new one is not in place
# (an error or Ctrl-C between the two moves), the old copy is put back.
cleanup() {
  [ -n "$TMP" ] && rm -rf "$TMP"
  [ -n "$STAGE_NEW" ] && rm -rf "$STAGE_NEW"
  if [ -n "$STAGE_OLD" ] && [ -d "$STAGE_OLD" ]; then
    if [ -e "$STAGE_OLD/$APP_NAME" ] && [ -n "$DEST" ] && [ ! -e "$DEST" ]; then
      if mv "$STAGE_OLD/$APP_NAME" "$DEST"; then
        warn "途中で止まったため、以前の OpenSpeech.app を戻しました: $DEST"
      else
        warn "以前の OpenSpeech.app は $STAGE_OLD/$APP_NAME にあります。$DEST へ戻してください。"
        return
      fi
    fi
    rmdir "$STAGE_OLD" 2>/dev/null || true
  fi
}

main() {
  # Names used by cleanup start empty (never taken from the caller's environment).
  TMP=""; STAGE_NEW=""; STAGE_OLD=""; DEST=""
  trap cleanup EXIT
  trap 'exit 130' INT HUP TERM
  HOME="${HOME%/}"
  BASE_URL="${OPENSPEECH_BASE_URL:-https://openspeech.unitedradiology.jp/dl}"
  BASE_URL="${BASE_URL%/}"

  # ---- environment checks ----
  if [ "$(uname)" != "Darwin" ]; then
    die "このインストーラは Mac 専用です。"
  fi
  # uname -m says x86_64 in a terminal running under Rosetta; ask the hardware instead.
  if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || echo 0)" != "1" ]; then
    die "OpenSpeech は Apple Silicon(M1 以降)の Mac のみ対応です。この Mac(Intel 版)にはインストールできません。"
  fi
  osver="$(sw_vers -productVersion)"
  osmajor="${osver%%.*}"
  if [ "$osmajor" -lt 15 ]; then
    die "OpenSpeech は macOS 15 以降が必要です(この Mac は macOS $osver)。"
  fi

  # ---- download and verify ----
  TMP="$(mktemp -d)"
  msg "OpenSpeech をダウンロードしています…"
  for f in VERSION "$ARCHIVE" "$ARCHIVE.sha256"; do
    curl -q -fsSL "$BASE_URL/$f" -o "$TMP/$f" \
      || die "ダウンロードに失敗しました: $BASE_URL/$f"
  done
  msg "ファイルの SHA-256 を確認しています…"
  expected="$(awk 'NR == 1 { print $1 }' "$TMP/$ARCHIVE.sha256")"
  actual="$(shasum -a 256 "$TMP/$ARCHIVE" | awk '{ print $1 }')"
  if [ "${#expected}" -ne 64 ] || [ "$actual" != "$expected" ]; then
    die "SHA-256 が一致しません。ダウンロードが壊れています。もう一度実行してください。"
  fi
  VERSION="$(tr -d '[:space:]' < "$TMP/VERSION")"

  # ---- unpack to a scratch folder and verify before touching any installed copy ----
  mkdir "$TMP/x"
  tar -xzf "$TMP/$ARCHIVE" -C "$TMP/x"
  [ -d "$TMP/x/$APP_NAME" ] || die "アーカイブに $APP_NAME が見つかりません。"
  bundle_ok "$TMP/x/$APP_NAME" || die "アーカイブの $APP_NAME が不完全か、署名の検証に失敗しました。"

  # ---- the running app is not quit by force: text in its editor would be lost ----
  if app_running; then
    die "OpenSpeech が起動しています。エディターの文章を転送するか写してから OpenSpeech を終了し、もう一度実行してください。"
  fi

  # ---- target: where it already is; a second copy with the same bundle id confuses macOS permissions ----
  SYSTEM_COPY="/Applications/$APP_NAME"
  USER_COPY="$HOME/Applications/$APP_NAME"
  IS_UPDATE=""
  if [ -e "$SYSTEM_COPY" ] && [ -e "$USER_COPY" ]; then
    die "OpenSpeech が 2 か所($SYSTEM_COPY と $USER_COPY)に入っています。使わない方を削除してから、もう一度実行してください。"
  elif [ -e "$SYSTEM_COPY" ]; then
    DEST="$SYSTEM_COPY"; IS_UPDATE=1
  elif [ -e "$USER_COPY" ]; then
    DEST="$USER_COPY"; IS_UPDATE=1
  elif [ -w "/Applications" ]; then
    DEST="$SYSTEM_COPY"
  else
    mkdir -p "$HOME/Applications"
    DEST="$USER_COPY"
  fi
  APP_DIR="$(dirname "$DEST")"
  if [ ! -w "$APP_DIR" ]; then
    die "$APP_DIR に書き込めないため、$DEST を更新できません。管理者に更新を頼むか、$DEST を削除してから、もう一度実行してください。"
  fi

  # ---- install: put the new copy next to the old one, check it, then swap (put the old one back on failure) ----
  # ditto keeps the signature intact; no quarantine attribute is added.
  msg "$APP_DIR に OpenSpeech.app を設置しています…"
  STAGE_NEW="$(mktemp -d "$APP_DIR/.openspeech-new.XXXXXX")"
  /usr/bin/ditto "$TMP/x/$APP_NAME" "$STAGE_NEW/$APP_NAME" \
    || die "$APP_DIR に書き込めませんでした(何も変えていません)。"
  bundle_ok "$STAGE_NEW/$APP_NAME" \
    || die "設置した OpenSpeech.app の検証に失敗しました(何も変えていません)。"
  if [ -e "$DEST" ]; then
    STAGE_OLD="$(mktemp -d "$APP_DIR/.openspeech-old.XXXXXX")"
    mv "$DEST" "$STAGE_OLD/$APP_NAME" || die "以前の OpenSpeech.app を動かせませんでした(何も変えていません)。"
  fi
  # On failure, cleanup puts the old copy back.
  mv "$STAGE_NEW/$APP_NAME" "$DEST" || die "新しい OpenSpeech.app を置けませんでした。"
  if [ -n "$STAGE_OLD" ]; then
    if rm -rf "$STAGE_OLD"; then
      STAGE_OLD=""
    else
      warn "以前の OpenSpeech.app の残り($STAGE_OLD)を消せませんでした。あとで削除してください。"
      STAGE_OLD=""
    fi
    msg "以前の OpenSpeech.app を置き換えました(設定とモデルは残ります)"
  fi

  if [ -n "$IS_UPDATE" ]; then
    tccutil reset Accessibility "$BUNDLE_ID" >/dev/null 2>&1 || true
  fi

  # ---- done ----
  msg "OpenSpeech を起動します"
  open "$DEST" || warn "自動で開けませんでした。$DEST を開いてください。"
  echo
  echo "インストール完了: OpenSpeech v$VERSION"
  echo "場所: $DEST"
  echo
  echo "つぎのステップ:"
  echo "  - 初回の起動時に案内が表示されます"
  echo "  - 音声認識のモデル(約 0.66 GB)は、アプリの中から取得します"
  echo "  - 設定とデータは ~/Library/Application Support/OpenSpeech に保存されます"
  if [ -n "$IS_UPDATE" ]; then
    echo
    echo "更新しました。アクセシビリティの許可をもう一度してください:"
    echo "  システム設定 → プライバシーとセキュリティ → アクセシビリティ で OpenSpeech をオンにします"
    echo "  (署名が版ごとに変わるため、更新のたびに必要です)"
  fi
  echo
  echo "更新するときは、同じコマンドをもう一度実行するだけです。"
}

main "$@"
