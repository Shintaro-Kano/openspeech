#!/bin/bash
# OpenSpeech uninstaller — https://openspeech.unitedradiology.jp
# Removes OpenSpeech.app from /Applications and ~/Applications and resets its permissions.
# User data (settings, models, history) is NOT deleted.
set -euo pipefail
# Only the system's own commands (a PATH set up by the user or another tool is not used).
export PATH=/usr/bin:/bin:/usr/sbin:/sbin

BUNDLE_ID="dev.openspeech.app"

# A running copy of the app (any install location), for this user only.
app_running() {
  pgrep -U "$(id -u)" -f '/OpenSpeech\.app/Contents/MacOS/openspeech( |$)' >/dev/null 2>&1
}

main() {
  HOME="${HOME%/}"
  DATA="$HOME/Library/Application Support/OpenSpeech"

  # The running app is not quit by force: text in its editor would be lost.
  if app_running; then
    echo "OpenSpeech が起動しています。エディターの文章を転送するか写してから OpenSpeech を終了し、もう一度実行してください。" >&2
    exit 1
  fi

  echo "==> アプリを削除します"
  for app in "/Applications/OpenSpeech.app" "$HOME/Applications/OpenSpeech.app"; do
    if [ -e "$app" ]; then
      rm -rf "$app"
      echo "    削除しました: $app"
    fi
  done
  # Leftovers of an interrupted install (hidden staging folders next to the app).
  for dir in "/Applications" "$HOME/Applications"; do
    for stage in "$dir"/.openspeech-new.* "$dir"/.openspeech-old.*; do
      if [ -d "$stage" ]; then
        rm -rf "$stage"
        echo "    削除しました: $stage"
      fi
    done
  done

  echo "==> 権限をリセットします"
  tccutil reset Accessibility "$BUNDLE_ID" >/dev/null 2>&1 || true
  tccutil reset Microphone "$BUNDLE_ID" >/dev/null 2>&1 || true

  echo
  echo "アンインストール完了。"
  echo
  if [ -d "$DATA" ]; then
    size="$(du -sh "$DATA" 2>/dev/null | cut -f1 || true)"
    echo "設定・モデル・履歴などのデータは残してあります:"
    echo "  $DATA  (${size:-サイズ不明})"
    echo "これも消したい場合は、次のコマンドを実行してください:"
    echo "  rm -rf \"\$HOME/Library/Application Support/OpenSpeech\""
  else
    echo "データフォルダ($DATA)は見つかりませんでした。"
  fi
}

main "$@"
