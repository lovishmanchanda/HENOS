; Extra steps for the Windows installer.
;
; HENOS accepts connections from your other devices on the local network.
; Without a firewall rule Windows blocks those until the person answers a
; prompt, and answering it wrongly leaves HENOS unreachable with no hint why.
; The installer already runs with administrator rights, so it adds the rule
; itself: this program only, incoming only, private and domain networks only
; (never public ones such as cafe Wi-Fi). The uninstaller removes it again.

!macro NSIS_HOOK_POSTINSTALL
  nsExec::Exec 'netsh advfirewall firewall delete rule name="HENOS"'
  nsExec::Exec 'netsh advfirewall firewall add rule name="HENOS" dir=in action=allow program="$INSTDIR\henos-desktop.exe" profile=private,domain enable=yes description="Lets your other HENOS devices on this network reach this PC."'
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  nsExec::Exec 'netsh advfirewall firewall delete rule name="HENOS"'
!macroend
