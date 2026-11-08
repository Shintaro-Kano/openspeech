# Third-Party Notices

The source code of OpenSpeech is provided under the **MIT License** ([LICENSE](LICENSE)).

This file lists the third-party software and assets that the distributed app contains or links against, and the models that the app downloads at run time. Models are not part of the release archive and not part of this repository.

The distributed `OpenSpeech.app` carries this file, [LICENSE](LICENSE), [THIRD-PARTY-LICENSES.txt](THIRD-PARTY-LICENSES.txt) (the full license texts of the Rust crates and the egui fonts) and the two license files of speech.cpp and ggml in `Contents/Resources/`.

---

## 1. Bundled native library: speech.cpp and ggml

The app bundle contains `Contents/Frameworks/libspeech.4.dylib` (arm64), built from speech.cpp, which includes ggml. The headers and the library are in `vendor/speech-cpp/`.

- **speech.cpp** (<https://github.com/nyosegawa/speech.cpp>): MIT License, Copyright (c) 2026 Sakasegawa
- **ggml**: MIT License, Copyright (c) 2023-2026 The ggml authors

The license texts, as kept in `vendor/speech-cpp/LICENSE` and `vendor/speech-cpp/LICENSE-ggml`:

**vendor/speech-cpp/LICENSE**

    MIT License
    
    Copyright (c) 2026 Sakasegawa
    
    Permission is hereby granted, free of charge, to any person obtaining a copy
    of this software and associated documentation files (the "Software"), to deal
    in the Software without restriction, including without limitation the rights
    to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
    copies of the Software, and to permit persons to whom the Software is
    furnished to do so, subject to the following conditions:
    
    The above copyright notice and this permission notice shall be included in all
    copies or substantial portions of the Software.
    
    THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
    IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
    FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
    AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
    LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
    OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
    SOFTWARE.

**vendor/speech-cpp/LICENSE-ggml**

    MIT License
    
    Copyright (c) 2023-2026 The ggml authors
    
    Permission is hereby granted, free of charge, to any person obtaining a copy
    of this software and associated documentation files (the "Software"), to deal
    in the Software without restriction, including without limitation the rights
    to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
    copies of the Software, and to permit persons to whom the Software is
    furnished to do so, subject to the following conditions:
    
    The above copyright notice and this permission notice shall be included in all
    copies or substantial portions of the Software.
    
    THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
    IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
    FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
    AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
    LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
    OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
    SOFTWARE.

---

## 2. Models (downloaded by the app at run time, not bundled)

The app downloads the two model files only when the user presses "モデルを取得" (get the models). They are stored in the user's data folder (`~/Library/Application Support/OpenSpeech/models/`). OpenSpeech does not include them in the app, the release archive or this repository.

### 2.1 Haas-ja v1.0 (speech recognition)

- **Model**: Haas-ja v1.0, by Third Intelligence. License: **Apache-2.0** (<https://www.apache.org/licenses/LICENSE-2.0>).
- **File used by the app**: `Haas-616M-ja-v1.0-Q8_0.gguf` (about 656 MB), the unofficial GGUF conversion by sakasegawa (Hugging Face: `sakasegawa/Haas-ja-v1.0-GGUF`). The conversion is not made or endorsed by Third Intelligence.
- The app is not affiliated with Third Intelligence or sakasegawa. Please check the model cards on Hugging Face for the licenses and notices that apply to the files.

### 2.2 Silero VAD v6.2 (voice detection)

- **Model**: Silero VAD v6.2, by the Silero Team. License: **MIT** (<https://github.com/snakers4/silero-vad>).
- **File used by the app**: `silero-vad-309K-v6.2-F32.gguf` (about 1.2 MB), the GGUF conversion by sakasegawa (Hugging Face: `sakasegawa/silero-vad-GGUF`).

---

## 3. Rust crates (linked at build time)

The binary links the Rust crates in `Cargo.lock`. For the target `aarch64-apple-darwin` there are **186** third-party packages in the dependency graph (from `cargo metadata --format-version 1 --locked --filter-platform aarch64-apple-darwin`). None of them is licensed under GPL, AGPL, LGPL or SSPL; this is checked by `scripts/check-licenses.py`. One crate (`r-efi`) offers an LGPL option next to MIT and Apache-2.0 and is used under MIT or Apache-2.0 (it is not in the graph for macOS in any case).

Summary by license expression (as declared by the crates):

| License expression | Packages |
|---|---|
| MIT OR Apache-2.0 (including the forms `Apache-2.0 OR MIT`, `MIT/Apache-2.0`, `Apache-2.0/MIT`, `MIT / Apache-2.0`) | 101 |
| MIT | 28 |
| Unicode-3.0 | 18 |
| Apache-2.0 | 8 |
| Three-way choices (MIT and Apache-2.0 plus Zlib, BSD-2-Clause or 0BSD) | 18 |
| Unlicense OR MIT | 3 |
| BSD-3-Clause OR Apache-2.0 | 2 |
| Other single or combined expressions (MPL-2.0, ISC, BSD-3-Clause, Zlib, `Apache-2.0 AND MIT`, and the three combined expressions below) | 8 |

Notable licenses and the crates that carry them:

| License | Crates | Note |
|---|---|---|
| MPL-2.0 | `option-ext` 0.2.0 | Linked unmodified. Source: <https://github.com/soc/option-ext>. |
| SIL Open Font License 1.1 and Ubuntu Font Licence 1.0 | `epaint_default_fonts` 0.31.1 (`(MIT OR Apache-2.0) AND OFL-1.1 AND Ubuntu-font-1.0`) | Default fonts of egui, see section 4. |
| Unicode-3.0 | `icu_collections`, `icu_locale_core`, `icu_normalizer`, `icu_normalizer_data`, `icu_properties`, `icu_properties_data`, `icu_provider`, `litemap`, `potential_utf`, `tinystr`, `writeable`, `yoke`, `yoke-derive`, `zerofrom`, `zerofrom-derive`, `zerotrie`, `zerovec`, `zerovec-derive` (all Unicode-3.0), `unicode-ident` 1.0.27 (`(MIT OR Apache-2.0) AND Unicode-3.0`) | Copyright Unicode, Inc. License: <https://www.unicode.org/license.txt>. |
| `(Apache-2.0 OR MIT) AND BSD-3-Clause` | one crate | Both licenses apply (different parts of the crate). |
| ISC, BSD-3-Clause, Zlib | one crate each | Permissive. |

The full list with versions and the license text of every crate (and of the fonts below) is in [THIRD-PARTY-LICENSES.txt](THIRD-PARTY-LICENSES.txt), generated from `Cargo.lock` by `scripts/check-licenses.py --write THIRD-PARTY-LICENSES.txt`; CI fails when it is out of date. A crate published without a license file is listed with the standard MIT text and its authors (each such crate offers MIT).

Main direct dependencies (versions from `Cargo.lock`): `eframe` 0.31.1 and `egui` 0.31.1 (MIT OR Apache-2.0), `image` 0.25.10 (MIT OR Apache-2.0), `cpal` 0.15.3 (Apache-2.0).

---

## 4. Bundled default egui fonts

The crate `epaint_default_fonts` (part of egui) embeds these font files in the binary:

- **Hack** (Hack-Regular): MIT License, Copyright 2018 Source Foundry Authors. It contains work from the DejaVu project (public domain) and Bitstream Vera Sans Mono (Copyright 2003 Bitstream Inc., Bitstream Vera License).
- **Ubuntu-Light**: Ubuntu Font Licence 1.0.
- **Noto Emoji** (NotoEmoji-Regular): SIL Open Font License 1.1.
- **emoji-icon-font**: MIT License, Copyright (c) 2014 John Slegers.

The Japanese text in the app is drawn with a Japanese font that is already installed in macOS (Hiragino Sans, or one of two fallback fonts). It is read from the system when the app starts and is not part of the app.

---

## 5. App icon

The app icon was drawn by this project (generated by `assets/icon/make_icon.swift`) and is provided under the same MIT License as the source code.
