; Хуки установщика NSIS.
;
; Talkie живёт в трее и работает часами. Когда поверх него запускают установщик
; новой версии, Windows не даёт перезаписать запущенный talkie.exe, и установка
; обрывается сообщением «Error opening file for writing». Поэтому программу
; останавливаем сами, до копирования файлов: несохранённых данных у неё нет.

!macro TALKIE_STOP
  ; Сперва по-хорошему: работающая копия по ключу --quit закрывается сама и
  ; успевает убрать значок из трея. Убитая силой оставляет его призраком.
  nsExec::Exec '"$INSTDIR\talkie.exe" --quit'
  Pop $0
  Sleep 1500

  ; На случай зависшей программы. «Процесса нет» — норма, ошибку не проверяем.
  nsExec::Exec 'taskkill /F /T /IM "talkie.exe"'
  Pop $0
  ; До 0.2.0 программа называлась «Говор» — её копия держит те же данные и
  ; клавишу, поэтому тоже закрывается.
  nsExec::Exec 'taskkill /F /T /IM "govor.exe"'
  Pop $0
  ; Дескрипторы файлов освобождаются не мгновенно даже после смерти процесса.
  Sleep 800
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro TALKIE_STOP
!macroend

!macro NSIS_HOOK_POSTINSTALL
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro TALKIE_STOP
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
!macroend
