#[test]
fn native_cli_rejects_the_removed_custom_connection_label() {
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_ha-utilities"))
        .args([
            "--url",
            "http://127.0.0.1:1",
            "connections",
            "start",
            "--utility",
            "speculoos_power",
            "--accept-provider-settings",
            "--name",
            "Obsolete custom label",
        ])
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2));
    assert!(
        String::from_utf8(result.stderr)
            .unwrap()
            .contains("unexpected argument '--name'")
    );
}
