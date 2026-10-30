# mini-oled-dock — correzioni e aggiunte del 29/10/2026

Questo documento riassume tutte le modifiche contenute nel ramo `fix/tray-menu-placement`.
Ogni voce corrisponde a **un commit**, compilato, testato (`cargo test`) e provato a mano sul dock vero prima di essere salvato.

**Principio guida:** nessuna funzione nuova cambia il comportamento predefinito. Chi aggiorna senza toccare le impostazioni ritrova il dock esattamente com'era. Tutte le novità sono opzioni, spente di default (o, per la lingua, su «Sistema» come prima).

**Memoria:** obiettivo restare intorno ai 10 MB. Ogni funzione carica i propri dati solo quando serve e li libera subito dopo; nessun timer o thread permanente in più.

---

## Indice

| # | Commit | Tipo | In breve |
|---|--------|------|----------|
| 1 | `4812c51` | Correzione | Pannello di G HUB (e app simili) aperto in alto a sinistra |
| 2 | `d9ba5f0` | Miglioria | Menu del tasto destro del tray centrati sopra l'icona |
| 3 | `9051999` | Nuova opzione | «Win key opens Start»: Win apre Start + dock con un'unica animazione |
| 4 | `4adb178` | Nuova opzione | Lingua globale: Sistema / English / Italiano |
| 5 | `80c683b` | Novità | Audio spaziale e mixer per app nel pannello del volume |
| 6 | `aff38bb` | Correzione | App avviate dal dock che si aprivano dietro le altre finestre |
| 7 | `7af0432` | Novità | Pannello del volume personalizzabile |
| 8 | `143a69a` | Nuova opzione | Start orizzontale (stile Windows 11) con importazione dallo Start di Windows |
| 9 | `645471f` | Correzione | Tasto Win nei giochi a schermo intero |

---

## 1. Pannello di G HUB aperto in alto a sinistra — `4812c51`

**Problema.** Con il clic destro sull'icona di Logitech G HUB nel tray, il pannello compariva nell'angolo in alto a sinistra dello schermo.

**Causa.** `lghub_system_tray.exe` (app WinUI 3) non usa il punto del clic: cerca la finestra `Shell_TrayWnd` e ne legge il rettangolo per capire dov'è la barra. Trovava la finestra nascosta creata dal dock per ricevere le icone, che era un rettangolo piccolo e staccato dal bordo dello schermo, e ripiegava sull'angolo in alto a sinistra.

**Soluzione.** La finestra `Shell_TrayWnd` nascosta ora ha un rettangolo simile a quello di una barra vera: appoggiata al bordo dello schermo, dal bordo sinistro fino alla fine delle icone del tray del dock. Resta invisibile.

**File:** `src/tray_host.rs` (+ test).

## 2. Menu del tasto destro del tray centrati sopra l'icona — `d9ba5f0`

**Problema.** I menu delle app pinnate/aperte compaiono sempre centrati sopra l'icona; quelli del tray invece si aprivano esattamente dove si cliccava, coprendo l'icona stessa e quelle vicine.

**Soluzione.**
- Al clic destro su un'icona del tray, il dock passa all'app come punto di riferimento il centro dell'icona, appena sopra la barra, invece della posizione del cursore.
- Per 3 secondi il dock osserva le finestre che quell'app apre: la prima che sembra un menu (menu classici `#32768` o pannelli fatti in proprio, come quello di G HUB) viene spostata centrata sopra l'icona, a 6 px dal bordo del dock, con la stessa regola dei menu delle app pinnate.
- Limiti di sicurezza: solo per il clic destro; nessuna finestra con barra del titolo o troppo grande; i sottomenu non vengono toccati; se un'app riporta indietro il suo menu, dopo 6 tentativi il dock smette.
- Il dock riconosce il menu spostato come «menu aperto» e non lo copre.

**Limiti noti.** Con alcune app il menu può comparire per un istante nel punto del clic e poi spostarsi. Per le icone nascoste nel menu a freccia, il menu si centra sulla freccia.

**File:** `src/tray_host.rs` (+ test).

## 3. Opzione «Win key opens Start» — `9051999`

**Prima.** Win da solo mostra/nasconde il dock; doppio tocco apre lo Start. Comportamento voluto, quindi **resta il predefinito**.

**Con l'opzione accesa** (Impostazioni → Comportamento, chiave `win_opens_start`):
- un tocco di Win apre **insieme** dock e Start;
- il tocco successivo li chiude insieme;
- un tocco durante la chiusura li fa tornare indietro dal punto in cui erano;
- con auto-hide spento il dock resta visibile e Win apre/chiude solo lo Start;
- le combinazioni (Win+E, Win+D…) funzionano come prima.

**Animazione unica.** Durante apertura/chiusura con Win, lo Start non ha più una dissolvenza propria: a ogni fotogramma il dock calcola una sola volta trasparenza e scorrimento e li applica a sé e allo Start nello stesso istante.

**File:** `src/config.rs`, `src/main.rs`, `src/ui.rs`, `src/popup.rs`, `src/popup_settings.rs`.

## 4. Lingua globale — `4adb178`

**Novità.** Impostazioni → Aspetto → **Lingua**: *Sistema* (predefinito, segue la lingua di Windows come prima), *English*, *Italiano*. Chiave `language`: `"auto"`, `"en"`, `"it"`.

- Il cambio è immediato, senza riavvio.
- Tradotti: Impostazioni, menu del tasto destro sulle app e sul logo Windows, menu Start, pannelli di accensione, volume, rete, notifiche, tray e calendario, nomi dei colori.
- Orologio, data e calendario usano giorni e mesi nella lingua scelta (con *Sistema*: le impostazioni regionali di Windows, come prima).
- Restano in inglese di proposito: messaggi in console di `--install`, titoli interni delle finestre, «Wi-Fi», «Ethernet», «Internet», «Volume».

**Come aggiungere una lingua o un testo:** `src/i18n.rs`. Le traduzioni sono coppie di costanti compilate nell'eseguibile; lo stato è un solo byte. Nessuna allocazione.

**Costo:** eseguibile +7,5 KB, memoria privata invariata.

## 5. Audio spaziale e mixer nel pannello del volume — `80c683b`

**Novità** nel pannello che si apre con il clic sinistro sul volume:
- **Audio spaziale:** «Disattivato» più i formati realmente disponibili sull'uscita selezionata (es. Windows Sonic for Headphones, Dolby Atmos for Headphones, Dolby Atmos for Home Theater), con il segno di spunta su quello attivo. Se l'uscita non supporta l'audio spaziale la sezione non compare.
- **Mixer volume:** una riga per ogni app che sta riproducendo audio (fino a 8), con muto e cursore; «Suoni di sistema» in fondo.
- «Altre impostazioni volume» apre la pagina di Windows.

**Come funziona.**
- API ufficiale WinRT `Windows.Media.Audio.SpatialAudioDeviceConfiguration` (`IsSpatialAudioFormatSupported`, `SetDefaultSpatialAudioFormatAsync`), nessun trucco nel registro.
- Un formato compare solo se Windows lo accetta **e** se è installata l'app che lo fornisce (Dolby Access, DTS Sound Unbound). Così spariscono i formati non utilizzabili.
- «Dolby Atmos per altoparlanti» è escluso: Windows lo offre solo sugli altoparlanti integrati dei portatili compatibili, e le schede USB (es. Sound Blaster X7) si dichiarano «altoparlanti» anche con le cuffie collegate.
- Il cambio di formato gira su un thread a parte che termina subito: il dock non si blocca.
- Il mixer legge le app solo a pannello aperto; nulla resta in memoria dopo.

**Limite noto.** DTS non è stato verificato su una macchina che lo ha installato.

**File:** `src/audio.rs`, `src/popup_audio.rs`, `src/glyphs.rs`, `src/i18n.rs`, `Cargo.toml` (feature del crate `windows`).

## 6. App avviate dal dock aperte dietro le altre finestre — `aff38bb`

**Problema.** Le app avviate dal dock senza privilegi di amministratore si aprivano in secondo piano. Quelle da amministratore e quelle già aperte funzionavano.

**Causa.** Il dock gira come amministratore e, per non passare questi permessi, avvia le app tramite Explorer. Windows concede il primo piano solo al processo che ha ricevuto l'ultimo input (il dock): Explorer non aveva quel permesso da trasmettere.

**Soluzione.** Subito prima dell'avvio il dock chiama `AllowSetForegroundWindow` per Explorer (ripiego: `ASFW_ANY`, come già avveniva per le app aperte). Il permesso scade al clic o tasto successivo, quindi nessun programma può «rubare» il primo piano più tardi.

**File:** `src/shell_thread.rs`.

## 7. Pannello del volume personalizzabile — `7af0432`

**Novità.**
- Le sezioni **Volume generale, Uscita, Audio spaziale, Mixer** hanno un'intestazione con freccetta ▾/▸: clic per aprirle o chiuderle. Lo stato viene salvato.
- La **matita** in fondo al pannello attiva la modalità *Personalizza*:
  - **occhio** su ogni sezione per mostrarla o nasconderla (le nascoste restano visibili attenuate, così si possono riattivare);
  - **clicca e trascina** l'intestazione per cambiare l'ordine, compreso il Volume generale. Il trascinamento parte dopo 4 px; una linea bianca mostra dove andrà la sezione.
- La rotella del mouse continua a cambiare il volume generale anche se la sua sezione è chiusa o nascosta.

**Memoria.** Una sezione chiusa o nascosta **non carica nulla** (niente elenco dispositivi, niente controllo dei formati spaziali, niente elenco app): i dati vengono letti all'apertura e liberati alla chiusura. La disposizione occupa 6 byte; nel file è una stringa corta, chiave `audio_layout` (predefinito `"MOSX"`).

**Limite noto.** Una volta aperto l'audio spaziale, i componenti di Windows che servono a controllare i formati restano caricati: Windows non permette di scaricarli.

**File:** `src/config.rs`, `src/popup_audio.rs`, `src/glyphs.rs`, `src/i18n.rs`.

## 8. Start orizzontale con importazione — `143a69a`

**Novità.** Impostazioni → Aspetto → **Menu Start**: *Verticale* (predefinito, lo Start attuale, **invariato al pixel**) oppure *Orizzontale*. Chiave `start_style`: `"vertical"` / `"horizontal"`.

**Start orizzontale** (stile organizzativo di Windows 11, con colori, font e stile del dock):
- ricerca in alto;
- griglia «Aggiunte» 6×3, con pagine scorribili con la rotella;
- «Tutte le app ›» con elenco alfabetico, ritorno con «‹ Indietro» o Esc;
- in basso il nome utente e il pulsante di accensione;
- il pulsante di accensione apre un **riquadro in sovrimpressione** dentro lo Start (Sospendi, Blocca, Riavvia, Arresta, Impostazioni, Chiudi dock), senza chiudere lo Start; Esc o un clic altrove chiudono solo il riquadro.

**App fissate condivise.** I due stili usano lo stesso elenco (`start_pins`): fissare o togliere un'app in uno vale anche per l'altro.

**Importazione.** Nella sotto-pagina *Menu Start ›* delle Impostazioni c'è **«Importa dallo Start di Windows»**. Esegue una volta, nascosto, il comando ufficiale `Export-StartLayout` (con limite di 30 s), converte le voci nelle app del dock e le mette nello stesso ordine di Windows. Il file temporaneo viene cancellato; in memoria non resta nulla. Il file `start2.bin` di Windows non viene letto perché è cifrato e non documentato.

**Limiti noti.** Il riquadro di accensione si usa solo col mouse e non contiene «Avvia con Windows» (resta nel pannello di accensione del dock).

**File:** `src/popup_startmenu.rs`, `src/start_import.rs` (nuovo), `src/popup_settings.rs`, `src/popup_power.rs`, `src/config.rs`, `src/preview.rs`, `src/i18n.rs`.

## 9. Tasto Win nei giochi a schermo intero — `645471f`

**Problema.** In un gioco a schermo intero, premendo Win lo schermo passava più volte fra gioco e desktop e alla fine lo Start non si apriva.

**Causa.** Il dock prendeva il primo piano per lo Start; molti giochi se lo riprendono subito. Il dock interpretava quel ritorno come «l'utente è tornato al gioco», chiudeva Start e dock e restituiva il primo piano al gioco.

**Soluzione.**
- Se il gioco si riprende il primo piano entro 0,8 s da quando il dock l'ha preso, il dock lo riprende per lo Start (al massimo 2 volte) invece di chiudere tutto.
- Se il gioco insiste, il dock si nasconde come prima: nessuna lotta infinita.
- Gli avvisi di primo piano arrivati in ritardo, quando il primo piano è già tornato al dock, vengono ignorati.
- Fuori dai giochi e per un clic volontario sul gioco nulla cambia.

**Limite noto.** Con i giochi in schermo intero *esclusivo* resta un singolo cambio di schermata, inevitabile, ma lo Start resta aperto.

**File:** `src/ui.rs`, `src/popup.rs`.

---

## Nuove chiavi di configurazione (`config.json`)

| Chiave | Valori | Predefinito |
|--------|--------|-------------|
| `language` | `"auto"`, `"en"`, `"it"` | `"auto"` |
| `win_opens_start` | `true` / `false` | `false` |
| `start_style` | `"vertical"` / `"horizontal"` | `"vertical"` |
| `audio_layout` | stringa, una lettera per sezione (M, O, S, X; minuscola = chiusa, `-` davanti = nascosta) | `"MOSX"` |

Tutte le chiavi sono facoltative: un `config.json` vecchio continua a funzionare.

## Verifiche

- `cargo build --release --locked` senza avvisi.
- `cargo test`: 128 test superati (1 ignorato, come nel progetto originale).
- Ogni funzione provata a mano sul dock in esecuzione prima del rispettivo commit.
- `cargo fmt` non è stato applicato: il progetto originale non è formattato con rustfmt e la PR si sarebbe riempita di modifiche estranee.
