//! `imageMime.test.ts`, against the port in `zc_orchestration::attachments` that the upload
//! store and the attachment normalizer share.

use zc_orchestration::attachments::{infer_image_extension, parse_base64_data_url, DataUrl};

fn parsed(mime_type: &str, base64: &str) -> Option<DataUrl> {
    Some(DataUrl {
        mime_type: mime_type.into(),
        base64: base64.into(),
    })
}

#[test]
fn parses_base64_data_urls_with_a_mime_type() {
    assert_eq!(parse_base64_data_url("data:image/png;base64,SGVsbG8="), parsed("image/png", "SGVsbG8="));
}

#[test]
fn parses_base64_data_urls_with_mime_parameters() {
    assert_eq!(
        parse_base64_data_url("data:image/png;charset=utf-8;base64,SGVsbG8="),
        parsed("image/png", "SGVsbG8=")
    );
}

#[test]
fn rejects_non_base64_data_urls_and_missing_mime_types() {
    assert_eq!(parse_base64_data_url("data:image/png;charset=utf-8,hello"), None);
    assert_eq!(parse_base64_data_url("data:;base64,SGVsbG8="), None);
}

#[test]
fn parses_payloads_with_spaces() {
    assert_eq!(parse_base64_data_url("data:image/png;base64,SGVs bG8=\n"), parsed("image/png", "SGVsbG8="));
}

#[test]
fn rejects_characters_outside_the_alphabet() {
    assert_eq!(parse_base64_data_url("data:image/png;base64,SGVs!bG8="), None);
    assert_eq!(parse_base64_data_url("data:image/png;base64,SGVs,bG8="), None);
}

#[test]
fn rejects_structurally_malformed_base64() {
    assert_eq!(parse_base64_data_url("data:image/png;base64,AB=CD==="), None);
    assert_eq!(parse_base64_data_url("data:image/png;base64,SGV=bG8="), None);
    assert_eq!(parse_base64_data_url("data:image/png;base64,SGVsbG8=====AAA"), None);
    assert_eq!(parse_base64_data_url("data:image/png;base64,SGVsbG8"), None);
}

#[test]
fn accepts_one_or_two_trailing_pads() {
    assert_eq!(parse_base64_data_url("data:image/png;base64,SGVsbA=="), parsed("image/png", "SGVsbA=="));
    assert_eq!(parse_base64_data_url("data:image/png;base64,SGVsbG8h"), parsed("image/png", "SGVsbG8h"));
}

#[test]
fn rejects_empty_and_whitespace_only_payloads() {
    assert_eq!(parse_base64_data_url("data:image/png;base64,"), None);
    assert_eq!(parse_base64_data_url("data:image/png;base64, \r\n"), None);
}

#[test]
fn parses_a_case_insensitive_scheme_and_mime_type() {
    assert_eq!(parse_base64_data_url("DATA:IMAGE/PNG;BASE64,SGVsbG8="), parsed("image/png", "SGVsbG8="));
}

#[test]
fn parses_a_multi_megabyte_payload() {
    let data_url = format!("data:image/png;base64,{}", "A".repeat(14_000_000));
    let result = parse_base64_data_url(&data_url).unwrap();
    assert_eq!(result.mime_type, "image/png");
    assert_eq!(result.base64.len(), 14_000_000);
}

#[test]
fn does_not_read_inherited_keys_from_the_extension_map() {
    assert_eq!(infer_image_extension("constructor", None), ".bin");
    assert_eq!(infer_image_extension("image/png", Some("x.gif")), ".png");
    assert_eq!(infer_image_extension("application/octet-stream", Some("shot.JPEG")), ".jpeg");
}
