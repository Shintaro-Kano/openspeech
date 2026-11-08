# OpenSpeech icon

Regenerate PNGs and the .iconset (run from this folder):

    swiftc -O -parse-as-library make_icon.swift -o /tmp/make_icon && /tmp/make_icon .

Build the .icns and remove the temporary iconset:

    iconutil -c icns OpenSpeech.iconset -o OpenSpeech.icns && rm -rf OpenSpeech.iconset
