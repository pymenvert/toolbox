# Harnais d'endurance de Lanterne (Windows).
#
# Personne n'avait jamais fait tourner le node plusieurs heures d'affilee en
# regardant ce qu'il devenait. Une fuite memoire, un rendu qui se degrade, un
# compteur d'images perdues qui grimpe : ces pannes-la ne se voient QUE dans
# la duree, et elles sont exactement celles qui gachent un spectacle.
#
# Ce script echantillonne /api/system a cadence reguliere dans un CSV, en
# maintenant une charge reelle sur le node, puis depouille le fichier.
#
# Usage :
#   .\endurance.ps1 -Minutes 240                 # 4 h, charge par defaut
#   .\endurance.ps1 -Minutes 60 -SansCharge      # observation passive
#   .\endurance.ps1 -Analyser endurance.csv      # depouiller un CSV existant
#                                                # (y compris celui d'un Pi)
#
# Le CSV utilise le point-virgule : Excel francais l'ouvre directement.

param(
    [string]$Url = "http://127.0.0.1:8080",
    [int]$Minutes = 60,
    [int]$IntervalleS = 15,
    [string]$Csv = "endurance.csv",
    [int]$CadenceCharge = 10,
    [switch]$SansCharge,
    [string]$Analyser
)

$ErrorActionPreference = "Stop"

# ---------------------------------------------------------------- collecte --

function Get-Champ($objet, $chemin) {
    # Lecture tolerante : un champ absent (plateforme sans la mesure) donne
    # une cellule vide plutot qu'une erreur qui tuerait le run a la 3e heure.
    $courant = $objet
    foreach ($cle in $chemin.Split(".")) {
        if ($null -eq $courant) { return "" }
        $courant = $courant.$cle
    }
    if ($null -eq $courant) { return "" }
    # SEPARATEUR DECIMAL : le point, toujours. Sur un Windows francais,
    # "$valeur" rend « 3,10 » -- le collecteur shell, lui, ecrit « 3.10 ».
    # Les deux collecteurs doivent produire des fichiers identiques, sinon
    # la promesse « depouiller un CSV de Pi avec le script Windows » est
    # fausse des qu'une colonne devient flottante.
    if ($courant -is [double] -or $courant -is [single] -or $courant -is [decimal]) {
        return $courant.ToString([cultureinfo]::InvariantCulture)
    }
    return $courant
}

# Lecture d'un nombre venant du CSV, quelle que soit sa provenance :
# les fichiers produits AVANT ce correctif portent des virgules decimales.
function ConvertTo-Nombre($texte) {
    if ($null -eq $texte -or "$texte" -eq "") { return $null }
    return [double]::Parse(("$texte" -replace ",", "."), [cultureinfo]::InvariantCulture)
}

# La charge doit etre CONTINUE, pas un ping toutes les 15 s : un node au
# repos ne redessine pas (la fenetre ne repeint que sur changement d'etat),
# donc un echantillonnage passif mesurerait un logiciel qui ne fait rien.
# Deux sources de charge, chacune dans son processus pour ne pas bloquer
# l'echantillonnage :
#   - un flot de commandes au rythme demande, comme une console OSC qui
#     pilote le spectacle en continu -- chacune traverse le bus, republie
#     l'etat et declenche un redessin ;
#   - un client MJPEG permanent, qui fait tourner le compositeur partage.
function Start-Charge($base, $cadence) {
    $jobs = @()
    $jobs += Start-Job -ArgumentList $base, $cadence -ScriptBlock {
        param($base, $cadence)
        $commandes = @(
            '{"cmd":"corner_set","index":0,"x":0.01,"y":0.01}',
            '{"cmd":"corner_set","index":0,"x":0.0,"y":0.0}',
            '{"cmd":"color_set","param":"gamma","value":1.1}',
            '{"cmd":"color_set","param":"gamma","value":1.0}',
            '{"cmd":"set_test_pattern","pattern":"grid"}',
            '{"cmd":"set_test_pattern","pattern":"bars"}'
        )
        $pause = [math]::Max(1, [int](1000 / [math]::Max(1, $cadence)))
        $i = 0
        while ($true) {
            try {
                Invoke-RestMethod -Uri "$base/api/command" -Method Post `
                    -ContentType "application/json" `
                    -Body $commandes[$i % $commandes.Count] -TimeoutSec 10 | Out-Null
            } catch {
                # Un refus ponctuel ne doit pas arreter un run de plusieurs heures.
                Start-Sleep -Seconds 1
            }
            $i++
            Start-Sleep -Milliseconds $pause
        }
    }
    # Client MJPEG permanent : curl.exe est livre avec Windows 10+.
    if (Get-Command curl.exe -ErrorAction SilentlyContinue) {
        $nul = if ($IsLinux -or $IsMacOS) { "/dev/null" } else { "NUL" }
        $p = Start-Process -FilePath "curl.exe" -PassThru -WindowStyle Hidden `
            -ArgumentList "-s", "-o", $nul, "$base/flux.mjpg?fps=15"
        $jobs += $p
    } else {
        Write-Host "  (curl.exe absent : pas de client MJPEG dans la charge)"
    }
    return $jobs
}

function Stop-Charge($jobs) {
    foreach ($j in $jobs) {
        try {
            if ($j -is [System.Diagnostics.Process]) {
                if (-not $j.HasExited) { $j.Kill() }
            } else {
                Stop-Job $j -ErrorAction SilentlyContinue
                Remove-Job $j -Force -ErrorAction SilentlyContinue
            }
        } catch { }
    }
}

function Start-Collecte {
    param($base, $minutes, $intervalle, $fichier, $sansCharge, $cadence)

    # UTF-8 SANS BOM, comme le collecteur shell : Out-File -Encoding utf8
    # sous PowerShell 5.1 prefixe le fichier d'un BOM, qui colle a l'en-tete
    # de la premiere colonne pour tout lecteur autre qu'Import-Csv.
    [System.IO.File]::WriteAllText(
        $fichier,
        "secondes;rss_mb;p50_us;p95_us;max_us;sautees;fps;erreurs;uptime_s`r`n",
        (New-Object System.Text.UTF8Encoding($false)))

    $debut = Get-Date
    $fin = $debut.AddMinutes($minutes)
    $lignes = 0
    $echecs = 0
    Write-Host "Endurance : $minutes min sur $base, un point toutes les $intervalle s."
    Write-Host "CSV : $fichier"
    $charge = @()
    if (-not $sansCharge) {
        $charge = Start-Charge $base $cadence
        Write-Host "Charge : ~$cadence commandes/s + client MJPEG."
    }

    while ((Get-Date) -lt $fin) {
        $sys = $null
        try {
            $sys = Invoke-RestMethod -Uri "$base/api/system" -TimeoutSec 15
        } catch {
            $echecs++
            Write-Host "  node injoignable ($echecs) : $($_.Exception.Message)"
        }
        if ($null -ne $sys) {
            $secondes = [int]((Get-Date) - $debut).TotalSeconds
            $ligne = @(
                $secondes,
                (Get-Champ $sys "rss_mb"),
                (Get-Champ $sys "rendu.p50_us"),
                (Get-Champ $sys "rendu.p95_us"),
                (Get-Champ $sys "rendu.max_us"),
                (Get-Champ $sys "rendu.sautees"),
                (Get-Champ $sys "fps"),
                (Get-Champ $sys "erreurs_recentes"),
                (Get-Champ $sys "uptime_s")
            ) -join ";"
            Add-Content -Path $fichier -Value $ligne -Encoding utf8
            $lignes++
            if ($lignes % 20 -eq 0) {
                Write-Host "  $lignes points | RSS $(Get-Champ $sys 'rss_mb') Mo | p95 $(Get-Champ $sys 'rendu.p95_us') us"
            }
        }
        Start-Sleep -Seconds $intervalle
    }
    Stop-Charge $charge
    Write-Host "Collecte terminee : $lignes points, $echecs echec(s) de lecture."
}

# ------------------------------------------------------------ depouillement --

function Get-Pente {
    # Moindres carres : Mo par heure. Une seule mesure ne fait pas une pente.
    param($temps, $valeurs)
    $n = $temps.Count
    if ($n -lt 3) { return $null }
    $mx = ($temps | Measure-Object -Average).Average
    $my = ($valeurs | Measure-Object -Average).Average
    $num = 0.0
    $den = 0.0
    for ($i = 0; $i -lt $n; $i++) {
        $dx = $temps[$i] - $mx
        $num += $dx * ($valeurs[$i] - $my)
        $den += $dx * $dx
    }
    if ($den -eq 0) { return $null }
    return $num / $den
}

function Get-Mediane($valeurs) {
    if ($valeurs.Count -eq 0) { return $null }
    $tri = $valeurs | Sort-Object
    return $tri[[int]($tri.Count / 2)]
}

function Show-Analyse($fichier) {
    if (-not (Test-Path $fichier)) {
        Write-Host "Fichier introuvable : $fichier"
        return
    }
    $lignes = Import-Csv -Path $fichier -Delimiter ";"
    if ($lignes.Count -lt 3) {
        Write-Host "Trop peu de points pour conclure ($($lignes.Count))."
        return
    }
    $heures = @()
    $rss = @()
    $p95 = @()
    foreach ($l in $lignes) {
        $memoire = ConvertTo-Nombre $l.rss_mb
        $secondes = ConvertTo-Nombre $l.secondes
        if ($null -ne $memoire -and $null -ne $secondes) {
            $heures += $secondes / 3600.0
            $rss += $memoire
        }
        $val = ConvertTo-Nombre $l.p95_us
        if ($null -ne $val -and $val -gt 0) { $p95 += $val }
    }
    $derniere = ConvertTo-Nombre $lignes[-1].secondes
    if ($null -eq $derniere) { $derniere = 0 }
    $duree = $derniere / 3600.0

    Write-Host ""
    Write-Host "=== Endurance : $($lignes.Count) points sur $([math]::Round($duree,2)) h ==="

    # --- Memoire : c'est LA question que le run doit trancher.
    if ($rss.Count -ge 3) {
        $debut = $rss[0]
        $fin = $rss[-1]
        $pic = ($rss | Measure-Object -Maximum).Maximum
        $pente = Get-Pente $heures $rss
        Write-Host ""
        Write-Host "Memoire du process : $debut Mo au depart, $fin Mo a l'arrivee (pic $pic Mo)"
        # Un run court ne permet AUCUN verdict : les premieres minutes sont
        # de la montee en regime (caches qui se remplissent, allocateur qui
        # ne rend rien tout de suite). Extrapoler ces 6 Mo-la donnerait
        # "110 Mo/h" et un faux cri a la fuite. Il faut au moins 30 min.
        $DUREE_MINIMALE_H = 0.5
        if ($duree -lt $DUREE_MINIMALE_H) {
            Write-Host "  run trop court ($([math]::Round($duree * 60)) min) pour conclure :"
            Write-Host "  les premieres minutes sont de la montee en regime. Relancer sur 1 h au moins."
        } elseif ($null -ne $pente) {
            $p = [math]::Round($pente, 2)
            Write-Host "  tendance : $p Mo/h"
            # Seuils volontairement larges : le RSS respire (cache, allocateur
            # qui ne rend pas tout de suite). Ce qui compte est une croissance
            # SOUTENUE, pas une bosse.
            if ($p -gt 5 -and ($fin - $debut) -gt 20) {
                Write-Host "  VERDICT : fuite probable -- a 24 h cela ferait +$([math]::Round($p * 24)) Mo."
            } elseif ($p -gt 1) {
                Write-Host "  VERDICT : croissance lente a surveiller sur un run plus long."
            } else {
                Write-Host "  VERDICT : stable."
            }
        }
    } else {
        Write-Host ""
        Write-Host "Memoire du process : non mesuree sur cette plateforme."
    }

    # --- Rendu : degradation dans le temps ?
    if ($p95.Count -ge 8) {
        $quart = [int]($p95.Count / 4)
        $debutMed = Get-Mediane $p95[0..($quart - 1)]
        $finMed = Get-Mediane $p95[($p95.Count - $quart)..($p95.Count - 1)]
        $med = Get-Mediane $p95
        Write-Host ""
        Write-Host "Temps par image (p95) : $([math]::Round($med / 1000, 2)) ms en median sur le run"
        Write-Host "  premier quart $([math]::Round($debutMed / 1000, 2)) ms -> dernier quart $([math]::Round($finMed / 1000, 2)) ms"
        if ($debutMed -gt 0 -and $finMed -gt ($debutMed * 1.3)) {
            Write-Host "  VERDICT : le rendu se degrade avec le temps (+30 % ou plus)."
        } else {
            Write-Host "  VERDICT : pas de degradation."
        }
    }

    # --- Incidents cumules.
    $sautDebut = ConvertTo-Nombre $lignes[0].sautees
    $sautFin = ConvertTo-Nombre $lignes[-1].sautees
    if ($null -ne $sautDebut -and $null -ne $sautFin) {
        $delta = [int]($sautFin - $sautDebut)
        Write-Host ""
        Write-Host "Images perdues pendant le run : $delta"
        if ($delta -gt 100) { Write-Host "  VERDICT : anormal, a investiguer." }
    }
    $errFin = ConvertTo-Nombre $lignes[-1].erreurs
    if ($null -ne $errFin -and $errFin -gt 0) {
        Write-Host "Erreurs dans le journal a la fin : $errFin (voir l'onglet Logs)"
    }
    Write-Host ""
}

# ------------------------------------------------------------------- main ----

if ($Analyser -ne "") {
    Show-Analyse $Analyser
} else {
    Start-Collecte $Url $Minutes $IntervalleS $Csv $SansCharge.IsPresent $CadenceCharge
    Show-Analyse $Csv
}
