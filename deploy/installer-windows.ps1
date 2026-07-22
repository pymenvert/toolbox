# Installateur Windows du node Toolbox — installation par profils.
#
# - choisit un PROFIL (complet, lecteur+mapping, synchro, lumières, minimal)
#   qui écrit node.toml + fonctions.json : les fonctions inutiles sont
#   réellement coupées (0 ressource consommée) ;
# - binaire : zip/exe déjà téléchargé à côté du script, ou téléchargement de
#   la dernière release GitHub (pack GStreamer autonome recommandé) ;
# - optionnel : lancement automatique à l'ouverture de session Windows.
#
# Usage (clic droit → Exécuter avec PowerShell, ou depuis un terminal) :
#   powershell -ExecutionPolicy Bypass -File installer-windows.ps1
#   ... -Dossier "D:\Toolbox" -Profil complet -SansQuestion
param(
    [string]$Dossier = "$env:LOCALAPPDATA\Toolbox",
    [ValidateSet("", "complet", "lecteur", "synchro", "lumieres", "minimal")]
    [string]$Profil = "",
    [switch]$SansQuestion,
    [switch]$DemarrageAuto
)

$ErrorActionPreference = "Stop"
$Depot = "pymenvert/toolbox"

function Dire($msg) { Write-Host ">> $msg" -ForegroundColor Cyan }

# --- profil -------------------------------------------------------------------
$profils = @{
    "1" = "complet"; "2" = "lecteur"; "3" = "synchro"; "4" = "lumieres"; "5" = "minimal"
}
if (-not $Profil) {
    Write-Host ""
    Write-Host "Profils d'installation :" -ForegroundColor Yellow
    Write-Host "  1. Complet          - tout : lecteur, mapping, OSC, MIDI, parc, lumieres"
    Write-Host "  2. Lecteur + Mapping - projection seule, pas de reseau ni lumieres"
    Write-Host "  3. Synchro multi-machines - lecteur + mapping + parc reseau + sync"
    Write-Host "  4. Lumieres         - console Art-Net + OSC/MIDI, pas de video"
    Write-Host "  5. Minimal          - lecteur seul (le plus leger)"
    $choix = Read-Host "Choix [1]"
    if (-not $choix) { $choix = "1" }
    if (-not $profils.ContainsKey($choix)) { throw "choix invalide : $choix" }
    $Profil = $profils[$choix]
}
Dire "Profil : $Profil"

# Les interrupteurs de fonctions par profil (fonctions.json — le format est
# celui de l'onglet Fonctions, champs absents = actifs).
$fonctions = @{
    complet  = '{"player":true,"output":true,"osc":true,"oscquery":true,"osc_feedback":true,"midi":true,"fleet":true,"fader":true,"preview":true,"artnet":true}'
    lecteur  = '{"player":true,"output":true,"osc":false,"oscquery":false,"osc_feedback":false,"midi":false,"fleet":false,"fader":true,"preview":true,"artnet":false}'
    synchro  = '{"player":true,"output":true,"osc":true,"oscquery":false,"osc_feedback":false,"midi":false,"fleet":true,"fader":true,"preview":true,"artnet":false}'
    lumieres = '{"player":false,"output":false,"osc":true,"oscquery":true,"osc_feedback":true,"midi":true,"fleet":false,"fader":false,"preview":false,"artnet":true}'
    minimal  = '{"player":true,"output":true,"osc":false,"oscquery":false,"osc_feedback":false,"midi":false,"fleet":false,"fader":false,"preview":false,"artnet":false}'
}[$Profil]

# --- binaire ------------------------------------------------------------------
# 1. un exe ou un pack dezippe a cote du script ; 2. sinon, telechargement.
$ici = Split-Path -Parent $MyInvocation.MyCommand.Path
$exeLocal = $null
foreach ($cand in @("$ici\toolbox-node.exe", "$ici\dist\toolbox-node.exe")) {
    if (Test-Path $cand) { $exeLocal = $cand; break }
}

New-Item -ItemType Directory -Force $Dossier | Out-Null
foreach ($sous in @("media", "presets", "logs", "shaders")) {
    New-Item -ItemType Directory -Force (Join-Path $Dossier $sous) | Out-Null
}

if ($exeLocal) {
    Dire "Binaire local : $exeLocal"
    Copy-Item $exeLocal (Join-Path $Dossier "toolbox-node.exe") -Force
    # Pack GStreamer autonome : les DLL vivent a cote de l'exe.
    $libLocal = Join-Path (Split-Path -Parent $exeLocal) "lib"
    if (Test-Path $libLocal) {
        Dire "Pack video detecte : copie des DLL GStreamer"
        Copy-Item (Join-Path (Split-Path -Parent $exeLocal) "*.dll") $Dossier -Force
        Copy-Item $libLocal (Join-Path $Dossier "lib") -Recurse -Force
    }
} else {
    $reponse = "o"
    if (-not $SansQuestion) {
        $reponse = Read-Host "Aucun binaire local. Telecharger la derniere release GitHub ? [O/n]"
        if (-not $reponse) { $reponse = "o" }
    }
    if ($reponse -notmatch "^[oOyY]") { throw "pas de binaire : deposez toolbox-node.exe a cote du script et relancez" }
    $zip = Join-Path $env:TEMP "toolbox-node-windows.zip"
    $url = "https://github.com/$Depot/releases/latest/download/toolbox-node-windows-x64-gstreamer.zip"
    Dire "Telechargement du pack video autonome..."
    try {
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    } catch {
        Dire "Pack video indisponible, repli sur le binaire leger (mire/backend simule)"
        $url = "https://github.com/$Depot/releases/latest/download/toolbox-node-windows-x64.zip"
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
    }
    Dire "Extraction dans $Dossier"
    Expand-Archive -Path $zip -DestinationPath $Dossier -Force
    Remove-Item $zip -Force
}

# --- configuration ------------------------------------------------------------
$nodeToml = Join-Path $Dossier "node.toml"
if (Test-Path $nodeToml) {
    Dire "node.toml existant conserve (nouveau modele : node.toml.new)"
    $nodeToml = Join-Path $Dossier "node.toml.new"
}
$nom = $env:COMPUTERNAME.ToLower()
@"
# Configuration du node Toolbox — generee par installer-windows.ps1
# (profil : $Profil). Reference complete : node.toml.example du depot.
name = "$nom"

[ports]
bind = "0.0.0.0"
http = 8080
osc = 9000
oscquery = 8081
"@ | ForEach-Object { [System.IO.File]::WriteAllText($nodeToml, $_) }
# WriteAllText : UTF-8 SANS BOM — serde_json refuse un BOM en tete de
# fonctions.json (le fichier serait ignore silencieusement).

$fonctionsJson = Join-Path $Dossier "fonctions.json"
if (Test-Path $fonctionsJson) {
    Dire "fonctions.json existant conserve (les bascules de l'UI priment)"
} else {
    [System.IO.File]::WriteAllText($fonctionsJson, $fonctions)
}

# --- demarrage automatique ------------------------------------------------------
$auto = "n"
if ($DemarrageAuto) { $auto = "o" }
elseif (-not $SansQuestion) { $auto = Read-Host "Lancer Toolbox a chaque ouverture de session ? [o/N]" }
if ($auto -match "^[oOyY]") {
    $demarrage = [Environment]::GetFolderPath("Startup")
    # MEME nom de fichier que install-autostart-windows.bat : sa
    # desinstallation (--remove) retire donc aussi celui-ci (coherence).
    $lanceur = Join-Path $demarrage "toolbox-node-autostart.bat"
    $exe = Join-Path $Dossier "toolbox-node.exe"
    $contenu = "@echo off`r`ncd /d `"$Dossier`"`r`nif not exist media mkdir media`r`n" +
        "if not exist presets mkdir presets`r`nif not exist logs mkdir logs`r`n" +
        "start `"toolbox-node`" /min `"$exe`"`r`n"
    # cmd.exe lit les .bat dans la CODEPAGE OEM du systeme (ni ASCII ni
    # UTF-8) : ecrire en ASCII mutilait les chemins accentues (profils
    # Windows francais type C:\Users\Frederic\...). On ecrit en OEM.
    $oemCp = [System.Globalization.CultureInfo]::CurrentCulture.TextInfo.OEMCodePage
    $oem = [System.Text.Encoding]::GetEncoding($oemCp)
    [System.IO.File]::WriteAllText($lanceur, $contenu, $oem)
    Dire "Demarrage automatique installe : $lanceur"
}

Dire "Installation terminee dans $Dossier"
Dire "Lancement : double-clic sur toolbox-node.exe (UI web : http://localhost:8080/)"
if ($Profil -eq "synchro") {
    Dire "Synchro : ajouter [sync] role = `"maitre`" (ou `"suiveur`" + maitre = `"ip:9010`") dans node.toml"
}
