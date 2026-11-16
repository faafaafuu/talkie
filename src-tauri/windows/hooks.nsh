; Хуки установщика NSIS.
;
; Говор живёт в трее и работает часами. Когда поверх него запускают установщик
; новой версии, Windows не даёт перезаписать запущенный govor.exe, и установка
; обрывается сообщением «Error opening file for writing». Поэтому программу
; останавливаем сами, до копирования файлов: несохранённых данных у неё нет.

!macro GOVOR_STOP
  ; Сперва по-хорошему: работающая копия по ключу --quit закрывается сама и
  ; успевает убрать значок из трея. Убитая силой оставляет его призраком.
  nsExec::Exec '"$INSTDIR\govor.exe" --quit'
  Pop $0
  Sleep 1500

  ; На случай зависшей программы. «Процесса нет» — норма, ошибку не проверяем.
  nsExec::Exec 'taskkill /F /T /IM "govor.exe"'
  Pop $0
  ; Дескрипторы файлов освобождаются не мгновенно даже после смерти процесса.
  Sleep 800
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro GOVOR_STOP
!macroend

!macro NSIS_HOOK_POSTINSTALL
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro GOVOR_STOP
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
!macroend
