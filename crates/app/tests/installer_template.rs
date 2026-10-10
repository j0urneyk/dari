//! `assets/installer.nsi` is cargo-packager's own NSIS template plus Dari's two `DariService`
//! steps. This test fails when the copy drifts from upstream, loses a step, or no longer matches
//! the cargo-packager version the workflows install.

use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

/// The cargo-packager version `assets/installer.nsi` was copied from.
const PACKAGER_VERSION: &str = "0.11.8";

/// SHA-256 of `crates/packager/src/package/nsis/installer.nsi` at tag `cargo-packager-v0.11.8`.
const UPSTREAM_SHA256: &str = "3ff5fde09ae24dfc031ab91f939b290f394e2c6d3549f2c43c37414716a47b7a";

/// Ends `Section Install`.
const INSTALL_ADDITION: &str = r#"
  ; Dari: register and start DariService. The command converges, so a repair or an upgrade runs it too.
  ClearErrors
  ExecWait '"$INSTDIR\dari-service.exe" install' $0
  ${If} ${Errors}
    Abort "Dari could not run dari-service.exe to set up its Windows service."
  ${ElseIf} $0 <> 0
    Abort "Dari could not set up its Windows service: dari-service.exe install exited with code $0."
  ${EndIf}
"#;

/// Starts `Section Uninstall`.
const UNINSTALL_ADDITION: &str = r#"  ; Dari: remove DariService before its executable is deleted. DariWaitForServiceExit comes from
  ; preinstall-section in crates/app/Cargo.toml.
  ${If} ${FileExists} "$INSTDIR\dari-service.exe"
    ClearErrors
    ExecWait '"$INSTDIR\dari-service.exe" uninstall' $0
    ${If} ${Errors}
      Abort "Dari could not run dari-service.exe to remove its Windows service."
    ${ElseIf} $0 <> 0
      Abort "Dari could not remove its Windows service: dari-service.exe uninstall exited with code $0."
    ${EndIf}
    !insertmacro DariWaitForServiceExit
  ${EndIf}

"#;

const REGROUND: &str = "To fix: copy crates/packager/src/package/nsis/installer.nsi from the \
    cargo-packager tag the workflows pin over crates/app/assets/installer.nsi, re-apply \
    INSTALL_ADDITION at the end of `Section Install` and UNINSTALL_ADDITION at the start of \
    `Section Uninstall`, and update PACKAGER_VERSION and UPSTREAM_SHA256 here";

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    // Windows checkouts may convert line endings; upstream's file has LF.
    text.replace("\r\n", "\n")
}

#[test]
fn template_is_upstream_plus_the_service_steps() {
    let template = read("assets/installer.nsi");
    for (name, addition) in [
        ("INSTALL_ADDITION", INSTALL_ADDITION),
        ("UNINSTALL_ADDITION", UNINSTALL_ADDITION),
    ] {
        assert_eq!(
            template.matches(addition).count(),
            1,
            "{name} must appear exactly once in assets/installer.nsi. {REGROUND}"
        );
    }

    let install_start = template
        .find("\nSection Install\n")
        .unwrap_or_else(|| panic!("`Section Install` is missing. {REGROUND}"));
    let install_end = install_start
        + template[install_start..]
            .find("\nSectionEnd\n")
            .unwrap_or_else(|| panic!("`Section Install` has no SectionEnd. {REGROUND}"));
    let install_body = &template[..=install_end];
    assert!(
        install_body.ends_with(INSTALL_ADDITION),
        "INSTALL_ADDITION must end `Section Install`. {REGROUND}"
    );
    let uninstall_anchor =
        format!("\nSection Uninstall\n{UNINSTALL_ADDITION}  !insertmacro CheckIfAppIsRunning\n");
    assert!(
        template.contains(&uninstall_anchor),
        "UNINSTALL_ADDITION must start `Section Uninstall`. {REGROUND}"
    );

    let upstream = template
        .replacen(INSTALL_ADDITION, "", 1)
        .replacen(UNINSTALL_ADDITION, "", 1);
    let digest = format!("{:x}", Sha256::digest(upstream.as_bytes()));
    assert_eq!(
        digest, UPSTREAM_SHA256,
        "Without Dari's additions, assets/installer.nsi differs from cargo-packager \
         {PACKAGER_VERSION}'s template. {REGROUND}"
    );
}

#[test]
fn workflows_install_the_packager_the_template_came_from() {
    let pin = format!("CARGO_PACKAGER_VERSION: {PACKAGER_VERSION}\n");
    for workflow in ["release.yml"] {
        let text = read(&format!("../../.github/workflows/{workflow}"));
        assert!(
            text.contains(&pin),
            "{workflow} must pin `{}`, the version assets/installer.nsi was copied from. \
             After changing the pin: {REGROUND}",
            pin.trim_end()
        );
    }
}
