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
# Etat du client MJPEG, renseigne par Start-Charge et lu par Start-Collecte
# pour annoncer la charge REELLE.
$script:MjpegEtat = "sans client MJPEG"

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
            '{"cmd":"set_test_pattern","pattern":"checker"}'
        )
        $pause = [math]::Max(1, [int](1000 / [math]::Max(1, $cadence)))
        $i = 0
        $refus = 0
        while ($true) {
            try {
                Invoke-RestMethod -Uri "$base/api/command" -Method Post `
                    -ContentType "application/json" `
                    -Body $commandes[$i % $commandes.Count] -TimeoutSec 10 | Out-Null
            } catch {
                # Un refus ponctuel ne doit pas arreter un run de plusieurs
                # heures -- mais il ne doit pas non plus passer inapercu :
                # « bars » n'existait pas et un sixieme de la charge partait
                # en 422 sans que rien ne le dise.
                $refus++
                if ($refus -le 3 -or $refus % 100 -eq 0) {
                    Write-Warning "commande refusee ($refus) : $($_.Exception.Message)"
                }
                Start-Sleep -Seconds 1
            }
            $i++
            Start-Sleep -Milliseconds $pause
        }
    }
    # Client MJPEG permanent : curl.exe est livre avec Windows 10+.
    if (Get-Command curl.exe -ErrorAction SilentlyContinue) {
        $nul = if ($IsLinux -or $IsMacOS) { "/dev/null" } else { "NUL" }
        # Sonder AVANT d'annoncer : /flux.mjpg repond 404 si la fonction
        # « Apercu » est coupee (le profil Pi 3 conseille de la couper),
        # 503 au-dela de 4 clients, 401 si un mot de passe est pose. Sans
        # cette verification, curl mourait aussitot, le compositeur partage
        # n'etait pas sollicite de tout le run, et le script annoncait
        # quand meme un client MJPEG.
        # Un flux MJPEG ne se termine JAMAIS : la sonde sort forcement en
        # timeout (curl code 28) apres avoir ecrit « 200 ». Or ce script
        # tourne sous $ErrorActionPreference = "Stop", et depuis PowerShell
        # 7.4 un code de retour non nul d'une commande native est une erreur
        # TERMINANTE : la sonde tuerait le run qu'elle est censee preparer.
        # On neutralise ce comportement le temps de l'appel (la variable
        # n'existe pas en 5.1, l'affectation y est sans effet).
        $ancienNatif = $PSNativeCommandUseErrorActionPreference
        $PSNativeCommandUseErrorActionPreference = $false
        $code = & curl.exe -s -m 3 -o $nul -w "%{http_code}" "$base/flux.mjpg?fps=15" 2>$null
        $PSNativeCommandUseErrorActionPreference = $ancienNatif
        if ("$code" -match "^2") {
            $p = Start-Process -FilePath "curl.exe" -PassThru -WindowStyle Hidden `
                -ArgumentList "-s", "-o", $nul, "$base/flux.mjpg?fps=15"
            $jobs += $p
            $script:MjpegEtat = "+ client MJPEG"
        } else {
            # Drapeau de portee script : l'annonce de la charge est imprimee
            # par Start-Collecte, ailleurs. Sans cela, la sonde etait bien
            # faite mais le message « + client MJPEG » restait inconditionnel
            # -- le collecteur shell disait la verite, celui-ci non.
            $script:MjpegEtat = "SANS client MJPEG (HTTP $code) - charge reduite"
        }
    } else {
        $script:MjpegEtat = "SANS client MJPEG (curl.exe absent)"
    }
    return $jobs
}

# Le node est laisse dans l'etat ou on l'a trouve. Sans ca, la charge
# laissait la MIRE DE TEST allumee -- et la mire est prioritaire sur la
# video (engine/raster.rs) : le spectacle suivant projetait un damier. Le
# coin 0 du mapping, lui, restait a la valeur par defaut, pas a celle de
# l'operateur : on le sauve avant, on le recharge apres.
function Save-Etat($base) {
    try {
        Invoke-RestMethod -Uri "$base/api/command" -Method Post -TimeoutSec 10 `
            -ContentType "application/json" `
            -Body '{"cmd":"mapping_save","name":"__endurance_avant"}' | Out-Null
        return $true
    } catch {
        Write-Warning "mapping non sauvegarde avant la charge : $($_.Exception.Message)"
        return $false
    }
}

function Restore-Etat($base, $sauvegarde) {
    $commandes = @('{"cmd":"set_test_pattern","pattern":null}')
    if ($sauvegarde) {
        $commandes += '{"cmd":"mapping_load","name":"__endurance_avant"}'
    }
    foreach ($c in $commandes) {
        try {
            Invoke-RestMethod -Uri "$base/api/command" -Method Post -TimeoutSec 10 `
                -ContentType "application/json" -Body $c | Out-Null
        } catch {
            Write-Warning "RESTAURATION INCOMPLETE : $($_.Exception.Message)"
            Write-Warning "verifier la mire et le mapping dans l'UI avant le prochain spectacle."
        }
    }
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

    # Chemin ABSOLU d'abord : [System.IO.File] resout les chemins relatifs
    # contre le repertoire du processus .NET, que Set-Location ne met PAS a
    # jour. Avec un chemin relatif, l'en-tete partait dans un fichier et les
    # lignes dans un autre -- reproduit sur ce poste.
    $fichier = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($fichier)
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
    $sauvegarde = $false
    # try/finally : sans lui, un Ctrl+C ou une erreur terminante (CSV
    # verrouille par Excel, par exemple) laissait tourner le job de commandes
    # ET un curl.exe orphelin qui martelaient le node indefiniment, mire de
    # test allumee.
    try {
        if (-not $sansCharge) {
            $sauvegarde = Save-Etat $base
            $charge = Start-Charge $base $cadence
            Write-Host "Charge : ~$cadence commandes/s $script:MjpegEtat."
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
                # Ecriture protegee : un CSV ouvert dans Excel rend
                # Add-Content terminant ($ErrorActionPreference = Stop) et
                # tuait un run de plusieurs heures a sa derniere minute.
                try {
                    Add-Content -Path $fichier -Value $ligne -Encoding utf8
                    $lignes++
                } catch {
                    $echecs++
                    Write-Warning "ligne non ecrite ($echecs) : $($_.Exception.Message)"
                }
                if ($lignes % 20 -eq 0 -and $lignes -gt 0) {
                    Write-Host "  $lignes points | RSS $(Get-Champ $sys 'rss_mb') Mo | p95 $(Get-Champ $sys 'rendu.p95_us') us"
                }
            }
            Start-Sleep -Seconds $intervalle
        }
    } finally {
        Stop-Charge $charge
        if (-not $sansCharge) { Restore-Etat $base $sauvegarde }
    }
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
    # PIEGE : [int] applique l'arrondi BANCAIRE. [int](3/2) = 2, donc
    # l'ancienne version renvoyait le MAXIMUM d'une serie de 3, pas sa
    # mediane -- et le decoupage en quarts produit justement des paquets de
    # 3 elements des 11 points. Division entiere explicite.
    if ($null -eq $valeurs -or $valeurs.Count -eq 0) { return $null }
    $tri = @($valeurs | Sort-Object)
    $n = $tri.Count
    $m = [int][math]::Floor($n / 2)
    if ($n % 2 -eq 1) { return $tri[$m] }
    return ($tri[$m - 1] + $tri[$m]) / 2
}

function Show-Analyse($fichier) {
    $fichier = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($fichier)
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
    $maxv = @()
    $inactifs = 0
    $sansImage = 0
    foreach ($l in $lignes) {
        $memoire = ConvertTo-Nombre $l.rss_mb
        $secondes = ConvertTo-Nombre $l.secondes
        if ($null -ne $memoire -and $null -ne $secondes) {
            $heures += $secondes / 3600.0
            $rss += $memoire
        }
        $val = ConvertTo-Nombre $l.p95_us
        if ($null -ne $val -and $val -gt 0) { $p95 += $val }
        elseif ($null -ne $val) { $inactifs++ }
        $f = ConvertTo-Nombre $l.fps
        if ($null -ne $f -and $f -le 0.01) { $sansImage++ }
        $vmax = ConvertTo-Nombre $l.max_us
        if ($null -ne $vmax -and $vmax -gt 0) { $maxv += $vmax }
    }
    # REDEMARRAGES. uptime_s ne peut que croitre : une baisse signifie que
    # le node est reparti (panique + systemd Restart=always, par exemple).
    # Sans cette detection, la pente memoire etait calculee a travers le
    # redemarrage -- ce qui aplatit voire inverse une vraie fuite -- et le
    # delta d'images perdues devenait negatif. Un node qui a plante trois
    # fois ressortait « stable ».
    $redemarrages = 0
    $precedent = $null
    foreach ($l in $lignes) {
        $u = ConvertTo-Nombre $l.uptime_s
        if ($null -ne $u -and $null -ne $precedent -and $u -lt $precedent) {
            $redemarrages++
        }
        if ($null -ne $u) { $precedent = $u }
    }

    $derniere = ConvertTo-Nombre $lignes[-1].secondes
    if ($null -eq $derniere) { $derniere = 0 }
    $duree = $derniere / 3600.0

    Write-Host ""
    Write-Host "=== Endurance : $($lignes.Count) points sur $([math]::Round($duree,2)) h ==="
    if ($redemarrages -gt 0) {
        Write-Host ""
        Write-Host "!!! LE NODE A REDEMARRE $redemarrages fois pendant le run !!!"
        Write-Host "    C'est en soi le resultat le plus important du test : chercher la"
        Write-Host "    cause dans le journal avant de lire quoi que ce soit d'autre."
        Write-Host "    Les chiffres ci-dessous couvrent plusieurs vies du process et"
        Write-Host "    n'ont donc PAS de sens comme tendance."
    }

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
        if ($redemarrages -gt 0) {
            # Un redemarrage remet la RSS a zero : la regression traverse
            # alors DEUX vies du process et sort negative, ce qui imprimait
            # « VERDICT : stable. » sur un run pourtant traverse par un
            # plantage. Le bandeau du haut annoncait deja que les tendances
            # sont sans valeur -- seules les images perdues en tenaient
            # compte, quarante lignes plus bas la memoire l'ignorait.
            Write-Host "  VERDICT : non calculable ($redemarrages redemarrage(s) pendant le run)."
            Write-Host "  La memoire repart de zero a chaque redemarrage : la pente melangerait"
            Write-Host "  deux vies du process. Relancer un run sans plantage pour conclure."
        } elseif ($duree -lt $DUREE_MINIMALE_H) {
            Write-Host "  run trop court ($([math]::Round($duree * 60)) min) pour conclure :"
            Write-Host "  les premieres minutes sont de la montee en regime. Relancer sur 1 h au moins."
        } elseif ($null -ne $pente) {
            $p = [math]::Round($pente, 2)
            Write-Host "  tendance globale : $p Mo/h"

            # LA PENTE GLOBALE NE SUFFIT PAS. Un allocateur ne rend pas la
            # memoire au fil de l'eau : il agrandit ses reserves par MARCHES,
            # puis se stabilise. Une regression lineaire sur un escalier
            # rapporte une pente qui n'existe pas -- observe en reel :
            # 373 Mo, palier a 377 pendant cinquante minutes, marche a 383,
            # palier de nouveau, et la regression annoncait « 6,1 Mo/h,
            # croissance a surveiller ». Ce qui distingue une fuite d'une
            # montee en regime, c'est que la fuite ne se stabilise JAMAIS.
            # On regarde donc le DERNIER TIERS du run, et depuis combien de
            # temps la valeur ne bouge plus.
            $tiers = [int]($rss.Count / 3)
            if ($tiers -lt 3) { $tiers = $rss.Count }
            $depart = $rss.Count - $tiers
            $penteFin = Get-Pente $heures[$depart..($rss.Count - 1)] $rss[$depart..($rss.Count - 1)]

            $derniereHausse = 0
            for ($i = 1; $i -lt $rss.Count; $i++) {
                if ($rss[$i] -gt $rss[$i - 1]) { $derniereHausse = $i }
            }
            $stableDepuis = [math]::Round(($heures[$rss.Count - 1] - $heures[$derniereHausse]) * 60)
            Write-Host "  derniere hausse il y a $stableDepuis min ; sur le dernier tiers : $([math]::Round($penteFin, 2)) Mo/h"

            if ($null -ne $penteFin -and $penteFin -le 1 -and $stableDepuis -ge 20) {
                Write-Host "  VERDICT : PALIER -- la memoire a fini de monter."
                # L'avertissement n'a de sens que si les deux chiffres
                # divergent : sur une courbe reellement plate, il n'y a
                # aucune marche a lisser.
                if ([math]::Abs($p) -gt 1) {
                    Write-Host "  (la tendance globale ci-dessus est un artefact : elle lisse des marches)"
                }
            } elseif ($p -gt 5 -and ($fin - $debut) -gt 20) {
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
        $identiques = 0
        for ($i = 0; $i -lt $p95.Count -and $i -lt $maxv.Count; $i++) {
            if ($p95[$i] -eq $maxv[$i]) { $identiques++ }
        }
        if ($p95.Count -gt 0 -and ($identiques / $p95.Count) -gt 0.8) {
            Write-Host ""
            Write-Host "NOTE : le p95 est egal au maximum sur $([math]::Round(100 * $identiques / $p95.Count)) % des points."
            Write-Host "  Trop peu d'images mesurees par seconde pour que le p95 ait un sens"
            Write-Host "  statistique -- augmenter -CadenceCharge, ou lire la colonne max_us."
        }
        Write-Host "Temps par image (p95) : $([math]::Round($med / 1000, 2)) ms en median sur le run"
        Write-Host "  premier quart $([math]::Round($debutMed / 1000, 2)) ms -> dernier quart $([math]::Round($finMed / 1000, 2)) ms"
        if ($debutMed -gt 0 -and $finMed -gt ($debutMed * 1.3)) {
            Write-Host "  VERDICT : le rendu se degrade avec le temps (+30 % ou plus)."
        } else {
            Write-Host "  VERDICT : pas de degradation."
        }
        # Le seuil que le README et l'UI annoncent tous les deux, et que le
        # depouillement n'appliquait nulle part.
        if ($med -ge 16000) {
            Write-Host "  ATTENTION : au-dela de 16 ms, une sortie 60 Hz saute des images."
        }
    }

    # SORTIE INACTIVE. Les points a zero etaient purement et simplement
    # JETES du calcul : une sortie morte -- ecran noir, plus une seule image
    # presentee -- disparaissait de l'analyse, qui concluait alors
    # « pas de degradation » sur les rares points survivants. C'est le
    # contraire de ce qu'un test d'endurance doit dire.
    $total = $lignes.Count
    if ($total -gt 0 -and $sansImage -gt 0) {
        $part = [math]::Round(100 * $sansImage / $total)
        Write-Host ""
        Write-Host "Sortie sans aucune image : $part % du run ($sansImage points sur $total)"
        if ($part -ge 50) {
            Write-Host "  VERDICT : la sortie ne peignait RIEN sur $part % du run."
            if ($part -lt 100) {
                Write-Host "  Les chiffres de rendu ci-dessous ne portent que sur le reste."
            }
        } elseif ($part -ge 10) {
            Write-Host "  (normal si la charge est faible : le node ne repeint que sur changement d'etat)"
        }
    }

    # LA PIRE IMAGE. Le p95 ne bouge que si l'incident touche au moins 5 %
    # des images : un blocage isole -- celui que le spectateur VOIT -- lui
    # est invisible. Cette colonne etait collectee et jamais regardee.
    if ($maxv.Count -ge 3) {
        $pire = ($maxv | Measure-Object -Maximum).Maximum
        $medMax = Get-Mediane $maxv
        Write-Host ""
        Write-Host "Pire image du run : $([math]::Round($pire / 1000, 1)) ms (median des pires : $([math]::Round($medMax / 1000, 1)) ms)"
        if ($pire -ge 100000) {
            Write-Host "  VERDICT : blocage franc (>= 100 ms) -- visible a l'oeil, a investiguer."
        } elseif ($pire -ge 33000) {
            Write-Host "  VERDICT : a-coups perceptibles (>= 33 ms, soit 2 images a 60 Hz)."
        } else {
            Write-Host "  VERDICT : aucun a-coup notable."
        }
    }

    # --- Incidents cumules.
    $sautDebut = ConvertTo-Nombre $lignes[0].sautees
    $sautFin = ConvertTo-Nombre $lignes[-1].sautees
    if ($null -ne $sautDebut -and $null -ne $sautFin) {
        Write-Host ""
        if ($redemarrages -gt 0) {
            # Le compteur repart de zero a chaque demarrage : la difference
            # entre le premier et le dernier point n'a plus aucun sens, et
            # elle sortait NEGATIVE.
            Write-Host "Images perdues : non calculable ($redemarrages redemarrage(s) ont remis le compteur a zero)"
            Write-Host "  dernier compteur connu : $([int]$sautFin) depuis le dernier demarrage"
        } else {
            $delta = [int]($sautFin - $sautDebut)
            Write-Host "Images perdues pendant le run : $delta"
            if ($delta -gt 100) { Write-Host "  VERDICT : anormal, a investiguer." }
        }
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
