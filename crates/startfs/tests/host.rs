//! Picking the name this node advertises from `tailscale status --json`.

use startfs::node::dns_name;

#[test]
fn the_magicdns_name_loses_its_trailing_dot() {
    let status = r#"{"Self": {"DNSName": "laptop.tail1234.ts.net.", "HostName": "laptop"}}"#;
    assert_eq!(dns_name(status).as_deref(), Some("laptop.tail1234.ts.net"));
}

#[test]
fn no_name_when_magicdns_is_off_or_the_output_is_unusable() {
    assert_eq!(dns_name(r#"{"Self": {"DNSName": ""}}"#), None);
    assert_eq!(dns_name(r#"{"Self": {}}"#), None);
    assert_eq!(dns_name("not json"), None);
}
