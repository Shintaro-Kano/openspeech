#!/bin/bash
# openspeech-speech(Apple 音声認識の補助プロセス)をビルドする。macOS 26 が必要。
# PATH の swiftc を使う(CommandLineTools のフルパス指定ではビルドできない)。
set -euo pipefail
cd "$(dirname "$0")"
swiftc -O -parse-as-library -suppress-warnings main.swift -o openspeech-speech
echo "built $(pwd)/openspeech-speech"
