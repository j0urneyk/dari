use std::fs;
use std::path::Path;

use sha2::{Digest, Sha256};

const PACKAGER_VERSION: &str = "0.11.8";

const UPSTREAM_SHA256: &str = "3ff5fde09ae24dfc031ab91f939b290f394e2c6d3549f2c43c37414716a47b7a";

const INSTALL_ADDITION: &str = r#"
  ClearErrors
  ExecWait '"$INSTDIR\dari-service.exe" install' $0
  ${If} ${Errors}
    Abort "Dari could not run dari-service.exe to set up its Windows service."
  ${ElseIf} $0 <> 0
    Abort "Dari could not set up its Windows service: dari-service.exe install exited with code $0."
  ${EndIf}
"#;

const UNINSTALL_ADDITION: &str = r#"  ${If} ${FileExists} "$INSTDIR\dari-service.exe"
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

const PAGE_ADDITION: &str = r#"
Var DariSecureDesktopControl
Var DariSecureDesktopCheckbox
Page custom DariSecureDesktopPage DariSecureDesktopPageLeave
Function DariSecureDesktopPage
  Call SkipIfPassive
  Call DariReadSecureDesktopControl
  !insertmacro MUI_HEADER_TEXT "$(dariSecureDesktopTitle)" "$(dariSecureDesktopSubtitle)"

  nsDialogs::Create 1018
  Pop $0
  ${IfThen} $(^RTL) == 1 ${|} nsDialogs::SetRTL $(^RTL) ${|}

  ${NSD_CreateCheckbox} 0 0 100% 12u "$(dariSecureDesktopCheckbox)"
  Pop $DariSecureDesktopCheckbox
  ${If} $DariSecureDesktopControl == "on"
    SendMessage $DariSecureDesktopCheckbox ${BM_SETCHECK} ${BST_CHECKED} 0
  ${EndIf}

  ${NSD_CreateLabel} 12u 20u -12u 48u "$(dariSecureDesktopExplanation)"
  Pop $0

  ${NSD_SetFocus} $DariSecureDesktopCheckbox
  nsDialogs::Show
FunctionEnd
Function DariSecureDesktopPageLeave
  ${NSD_GetState} $DariSecureDesktopCheckbox $0
  ${If} $0 == ${BST_CHECKED}
    StrCpy $DariSecureDesktopControl "on"
  ${Else}
    StrCpy $DariSecureDesktopControl "off"
  ${EndIf}
FunctionEnd
Function DariReadSecureDesktopControl
  ${IfThen} $DariSecureDesktopControl != "" ${|} Return ${|}
  ClearErrors
  ReadRegDWORD $0 HKLM "SOFTWARE\Policies\Dari" "SecureDesktopControl"
  ${IfNot} ${Errors}
    ${If} $0 = 1
      StrCpy $DariSecureDesktopControl "on"
    ${Else}
      StrCpy $DariSecureDesktopControl "off"
    ${EndIf}
    Return
  ${EndIf}
  ; ReadRegDWORD can't tell a missing value from one of a type it can't read, so look for the name.
  ; EnumRegValue sets the error flag past the last value; the unnamed default value has an empty name.
  StrCpy $DariSecureDesktopControl "on"
  StrCpy $1 0
  ${Do}
    ClearErrors
    EnumRegValue $2 HKLM "SOFTWARE\Policies\Dari" $1
    ${IfThen} ${Errors} ${|} ${ExitDo} ${|}
    ${If} $2 == "SecureDesktopControl"
      StrCpy $DariSecureDesktopControl "off"
      ${ExitDo}
    ${EndIf}
    IntOp $1 $1 + 1
  ${Loop}
FunctionEnd
"#;

const STRINGS_ADDITION: &str = r#"
LangString dariSecureDesktopTitle ${LANG_ENGLISH} "UAC prompts and the lock screen"
LangString dariSecureDesktopSubtitle ${LANG_ENGLISH} "Choose whether viewers can answer them."
LangString dariSecureDesktopCheckbox ${LANG_ENGLISH} "Let viewers answer UAC prompts and the lock screen"
LangString dariSecureDesktopExplanation ${LANG_ENGLISH} "A viewer you let control this PC can then click and type on UAC prompts and the lock screen. Programs running as you could use this too.$\n$\nYou can change this later in Dari's settings."
"#;

const POLICY_INSTALL_ADDITION: &str = r#"
  Call DariReadSecureDesktopControl
  ClearErrors
  ExecWait '"$INSTDIR\dari-service.exe" policy $DariSecureDesktopControl' $0
  ${If} ${Errors}
    Abort "Dari could not run dari-service.exe to set the SecureDesktopControl policy."
  ${ElseIf} $0 <> 0
    Abort "Dari could not set the SecureDesktopControl policy: dari-service.exe policy $DariSecureDesktopControl exited with code $0."
  ${EndIf}
"#;

const POLICY_UNINSTALL_ADDITION: &str = r#"
  ; An upgrade runs the old uninstaller with /P, and an administrator's policy must survive it.
  ${GetOptions} $CMDLINE "/P" $R0
  ${If} ${Errors}
    DeleteRegValue HKLM "SOFTWARE\Policies\Dari" "SecureDesktopControl"
    DeleteRegKey /ifempty HKLM "SOFTWARE\Policies\Dari"
  ${EndIf}
"#;

const ADDITIONS: [(&str, &str); 6] = [
    ("PAGE_ADDITION", PAGE_ADDITION),
    ("STRINGS_ADDITION", STRINGS_ADDITION),
    ("INSTALL_ADDITION", INSTALL_ADDITION),
    ("POLICY_INSTALL_ADDITION", POLICY_INSTALL_ADDITION),
    ("UNINSTALL_ADDITION", UNINSTALL_ADDITION),
    ("POLICY_UNINSTALL_ADDITION", POLICY_UNINSTALL_ADDITION),
];

const REGROUND: &str = "To fix: copy crates/packager/src/package/nsis/installer.nsi from the \
    cargo-packager tag the workflows pin over crates/app/assets/installer.nsi, re-apply \
    PAGE_ADDITION after the directory page, STRINGS_ADDITION after the language files, \
    INSTALL_ADDITION then POLICY_INSTALL_ADDITION at the end of `Section Install`, \
    UNINSTALL_ADDITION at the start of `Section Uninstall` and POLICY_UNINSTALL_ADDITION before \
    its closing /P check, and update PACKAGER_VERSION and UPSTREAM_SHA256 here";

fn read(relative: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    // Windows checkouts may convert line endings; upstream's file has LF.
    text.replace("\r\n", "\n")
}

#[test]
fn template_is_upstream_plus_dari_additions() {
    let template = read("assets/installer.nsi");
    for (name, addition) in ADDITIONS {
        assert_eq!(
            template.matches(addition).count(),
            1,
            "{name} must appear exactly once in assets/installer.nsi. {REGROUND}"
        );
    }

    let page_anchor = format!(
        "\n!insertmacro MUI_PAGE_DIRECTORY\n{PAGE_ADDITION}\n; 6. Start menu shortcut page\n"
    );
    assert!(
        template.contains(&page_anchor),
        "PAGE_ADDITION must follow the directory page. {REGROUND}"
    );
    let strings_anchor = format!(
        "  !include \"{{{{this}}}}\"\n{{{{/each}}}}\n{STRINGS_ADDITION}\n!macro SetContext\n"
    );
    assert!(
        template.contains(&strings_anchor),
        "STRINGS_ADDITION must follow the language files. {REGROUND}"
    );
    let install_start = template
        .find("\nSection Install\n")
        .unwrap_or_else(|| panic!("`Section Install` is missing. {REGROUND}"));
    let install_end = install_start
        + template[install_start..]
            .find("\nSectionEnd\n")
            .unwrap_or_else(|| panic!("`Section Install` has no SectionEnd. {REGROUND}"));
    let install_body = &template[..=install_end];
    assert!(
        install_body.ends_with(&format!("{INSTALL_ADDITION}{POLICY_INSTALL_ADDITION}")),
        "INSTALL_ADDITION then POLICY_INSTALL_ADDITION must end `Section Install`. {REGROUND}"
    );
    let uninstall_anchor =
        format!("\nSection Uninstall\n{UNINSTALL_ADDITION}  !insertmacro CheckIfAppIsRunning\n");
    assert!(
        template.contains(&uninstall_anchor),
        "UNINSTALL_ADDITION must start `Section Uninstall`. {REGROUND}"
    );
    let policy_uninstall_anchor = format!(
        "  {{{{/if}}}}\n{POLICY_UNINSTALL_ADDITION}\n  ${{GetOptions}} $CMDLINE \"/P\" $R0\n  \
         IfErrors +2 0\n    SetAutoClose true\nSectionEnd\n"
    );
    assert!(
        template.contains(&policy_uninstall_anchor),
        "POLICY_UNINSTALL_ADDITION must come just before the closing /P check of \
         `Section Uninstall`. {REGROUND}"
    );

    let upstream = ADDITIONS.iter().fold(template, |text, (_, addition)| {
        text.replacen(addition, "", 1)
    });
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
    for workflow in ["release.yml", "platform.yml"] {
        let text = read(&format!("../../.github/workflows/{workflow}"));
        assert!(
            text.contains(&pin),
            "{workflow} must pin `{}`, the version assets/installer.nsi was copied from. \
             After changing the pin: {REGROUND}",
            pin.trim_end()
        );
    }
}
