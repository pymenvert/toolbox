@echo off
rem Lancement portable Windows (P1.10) : tout vit dans le dossier du script.
rem Placez toolbox-node.exe a cote de ce script puis double-cliquez.
cd /d "%~dp0"
if not exist media mkdir media
if not exist presets mkdir presets
if not exist logs mkdir logs
if not exist shaders mkdir shaders
toolbox-node.exe %*
pause
