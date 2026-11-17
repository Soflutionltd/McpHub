use std::process::Command;

/// Lance le vrai binaire : il démarre, répond à `version` et annonce la version du crate.
#[test]
fn binary_reports_crate_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_McpHub"))
        .arg("version")
        .output()
        .expect("le binaire McpHub doit se lancer");
    assert!(output.status.success(), "code de sortie : {:?}", output.status);
    let stdout = String::from_utf8(output.stdout).expect("sortie en UTF-8");
    assert_eq!(stdout.trim(), format!("McpHub v{}", env!("CARGO_PKG_VERSION")));
}
