; Installer hooks for Tauri's NSIS template (bundle.windows.nsis.installerHooks).
;
; Guard against a duplicated install when migrating from the legacy WiX/MSI
; package. The template's reinstall page runs the MSI's interactive
; uninstaller, but if the user cancels it (or it fails), the page's Abort is
; swallowed in passive/update mode — the mode the in-app updater uses — and
; installation proceeds anyway, leaving both an MSI and an NSIS copy
; registered. This hook runs at the top of the install section, before any
; file is written: re-check for a surviving MSI registration, retry the
; uninstall without the confirmation dialog, and if the product still cannot
; be removed, quit without installing anything.
!macro NSIS_HOOK_PREINSTALL
  StrCpy $0 0
  hook_wix_loop:
    EnumRegKey $1 HKLM "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall" $0
    StrCmp $1 "" hook_wix_done
    IntOp $0 $0 + 1
    ReadRegStr $2 HKLM "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\$1" "DisplayName"
    ReadRegStr $3 HKLM "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\$1" "Publisher"
    StrCmp "$2$3" "${PRODUCTNAME}${MANUFACTURER}" 0 hook_wix_loop
    ReadRegDWORD $2 HKLM "SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\$1" "WindowsInstaller"
    IntCmp $2 1 0 hook_wix_loop hook_wix_loop

    ; $1 is the product code GUID (the key name for MSI registrations).
    ; /passive shows only a progress bar: no confirmation dialog to cancel.
    DetailPrint "Removing previously installed ${PRODUCTNAME} (MSI)"
    ClearErrors
    ExecWait 'msiexec.exe /X$1 /passive /norestart' $2
    ${If} ${Errors}
      StrCpy $2 2 ; ExecWait itself failed; fake a nonzero exit code
    ${EndIf}
    ${If} $2 = 0     ; removed
    ${OrIf} $2 = 3010 ; removed, reboot required
    ${OrIf} $2 = 1605 ; already gone
      Goto hook_wix_done
    ${EndIf}

    ; Still installed: do NOT proceed, or we would create a duplicate.
    IfSilent +2
    MessageBox MB_ICONEXCLAMATION "$(unableToUninstall)"
    SetErrorLevel $2
    Quit
  hook_wix_done:
!macroend
