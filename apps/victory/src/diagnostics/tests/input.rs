use super::super::input::decode;

#[test]
fn utf8_and_utf16_boms() {
    assert_eq!(decode(b"hello\r\n").unwrap(), "hello\r\n");
    assert_eq!(decode(&[0xef, 0xbb, 0xbf, b'x']).unwrap(), "x");
    assert_eq!(decode(&[0xff, 0xfe, b'x', 0]).unwrap(), "x");
    assert_eq!(decode(&[0xfe, 0xff, 0, b'x']).unwrap(), "x");
}

#[test]
fn bad_encoding_is_not_silently_replaced() {
    assert!(decode(&[0xff]).is_err());
    assert!(decode(&[0xff, 0xfe, 0x01]).is_err());
    assert!(decode(&[0xff, 0xfe, 0, 0xd8]).is_err());
}
