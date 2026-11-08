//! 文字コードの判定。CSV や履歴は UTF-8 のほか、古い環境の Shift_JIS も読めるようにする。

/// BOM 付き UTF-8、UTF-16LE、UTF-8、Shift_JIS の順に試す。
pub fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let (text, _, _) = encoding_rs::UTF_16LE.decode(bytes);
        return text.into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => decode_cp932(bytes),
    }
}

pub fn decode_cp932(bytes: &[u8]) -> String {
    let (text, _, _) = encoding_rs::SHIFT_JIS.decode(bytes);
    text.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_and_shift_jis_are_both_read() {
        assert_eq!(decode_text("右肺".as_bytes()), "右肺");
        assert_eq!(decode_text(&[0xEF, 0xBB, 0xBF, b'a']), "a");
        let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode("右肺");
        assert_eq!(decode_text(&sjis), "右肺");
    }
}
