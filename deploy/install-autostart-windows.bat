@echo off
rem Demarrage automatique Windows : lance Toolbox a chaque ouverture de
rem session (equivalent du service systemd cote Pi/Linux).
rem
rem Usage : placez ce script A COTE de toolbox-node.exe, double-cliquez.
rem Pour retirer : install-autostart-windows.bat --remove
rem Combine a [startup] preset/autoplay dans node.toml, le show (mapping
rem compris) reprend tout seul au demarrage de l'ordinateur.
setlocal
set "STARTUP=%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup"
set "LANCEUR=%STARTUP%\toolbox-node-autostart.bat"

if /i "%~1"=="--remove" (
  set "TROUVE="
  if exist "%LANCEUR%" ( del "%LANCEUR%" & set "TROUVE=1" )
  rem Retire aussi le lanceur d'anciennes versions de l'installeur PowerShell.
  if exist "%STARTUP%\toolbox-node.cmd" ( del "%STARTUP%\toolbox-node.cmd" & set "TROUVE=1" )
  if defined TROUVE ( echo Demarrage automatique retire. ) else ( echo Rien a retirer : le demarrage automatique n'etait pas installe. )
  goto :fin
)

if not exist "%~dp0toolbox-node.exe" (
  echo ERREUR : toolbox-node.exe introuvable a cote de ce script.
  echo Copiez ce script dans le dossier qui contient toolbox-node.exe.
  goto :fin
)

rem Genere un lanceur discret (fenetre reduite, pas de pause) dans le
rem dossier Demarrage de la session. Les chemins sont absolus, resolus ici.
> "%LANCEUR%" echo @echo off
>> "%LANCEUR%" echo cd /d "%~dp0"
>> "%LANCEUR%" echo if not exist media mkdir media
>> "%LANCEUR%" echo if not exist presets mkdir presets
>> "%LANCEUR%" echo if not exist logs mkdir logs
>> "%LANCEUR%" echo start "toolbox-node" /min "%~dp0toolbox-node.exe"

echo Installe : Toolbox demarrera a chaque ouverture de session Windows.
echo Lanceur : %LANCEUR%

:fin
pause
