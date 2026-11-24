# Piattaforma: cloud, motore, database, integrazioni

Proposta di architettura per far girare il linguaggio come servizio: modifichi il codice,
salvi, e l'ERP sul tuo dominio si aggiorna. Le scelte sono motivate da un requisito
sopra tutti: **i numeri devono essere affidabili al punto da reggere una revisione contabile**.

---

## 1. Le garanzie, prima della tecnologia

| Garanzia | Come si ottiene |
|---|---|
| Nessuna modifica nascosta | Log degli eventi solo-aggiunta: ogni commit registra utente, causa, versione del codice. Lo storico non si riscrive mai: le correzioni sono nuovi eventi. |
| Storico non manomissibile | Ogni evento contiene l'hash del precedente (catena di hash). Periodicamente l'ultimo hash viene marcato temporalmente da terzi (RFC 3161). Un revisore verifica la catena da solo. |
| Numeri riproducibili | Un valore dipende solo da (versione del codice, eventi fino all'istante t). `quadra replay` ricalcola qualsiasi numero a qualsiasi data e deve ottenere lo stesso risultato. |
| Definizioni certificate | Metriche e controlli legati all'impronta della loro definizione completa (`metrics.lock`). Un cambio di significato blocca il deploy finché il responsabile non riapprova. |
| Controlli | I `control` (bilancio che quadra, partitario = contabilità, numerazione senza buchi) sono verificati nei tipi, legati all'impronta della loro definizione e certificati come le metriche. Il motore non li valuta ancora da solo: valutarli a ogni commit rilevante e conservarne l'esito è il passo successivo. |
| Cambiamenti governati | Chi scrive il codice non è chi approva il deploy in produzione (quattro occhi, configurabile). Ogni deploy ha test, `quadra check` e report d'impatto allegati. |

Per una revisione formale questo copre i controlli IT generali che i revisori verificano:
gestione dei cambiamenti, accessi logici, operatività, integrità dei dati. La
certificazione vera (ISAE 3402 / SOC 1 e SOC 2 Type II sul servizio cloud) resta un audit
del fornitore. Il linguaggio fa in modo che le **prove** siano prodotte automaticamente,
invece di essere ricostruite a mano.

---

## 2. Dal salvataggio al deploy, senza conflitti

```
editor web (LSP) ─▶ compilazione ─▶ quadra check + test ─▶ diff IR vN → vN+1
                                                           │
           ┌───────────────────────────────────────────────┘
           ▼
   piano di migrazione (automatico + `migration` scritte a mano)
           ▼
   prova generale su una copia dei dati di produzione (branch del database)
           ▼
   report: record che diventerebbero invalidi · valori provvisori · metriche
           certificate toccate · viste e API cambiate · permessi cambiati
           ▼
   approvazione ─▶ cambio di versione atomico ─▶ i client si aggiornano
```

- **IR con impronta**: ogni versione del modello ha un hash. Il database registra quale
  versione ha scritto ogni evento.
- **Client sempre allineati**: l'interfaccia e ogni chiamata API dichiarano l'hash della
  versione. Se il server è più avanti risponde "versione superata": il client si ricarica
  e le bozze non salvate vengono conservate e rivalidate sul nuovo schema. Non esistono
  scritture fatte con regole vecchie.
- **Migrazioni senza fermo**: le modifiche piccole avvengono nella stessa transazione del
  cambio di versione (Postgres supporta il DDL transazionale). Le tabelle grandi seguono
  "espandi → riempi in background → passa → contrai", così l'ERP resta sempre online.
- **Solo in avanti**: tornare indietro significa fare un nuovo deploy con la versione
  precedente più una migrazione inversa. Lo storico resta lineare e verificabile.
- **Sandbox per tenant**: un ambiente di prova con copia (eventualmente anonimizzata) dei
  dati, per provare le modifiche prima della produzione.

---

## 3. Motore in Rust

| Parte | Scelta | Perché |
|---|---|---|
| Parser | tree-sitter per l'editor, parser dedicato per il compilatore | evidenziazione e errori in tempo reale; messaggi d'errore curati |
| Type checker | tipi dipendenti "leggeri" (`Money<order.currency>`, `Qty<product.uom>`), cardinalità, narrowing | è il cuore della promessa "se compila, i dati sono coerenti" |
| IR | serializzabile, hash stabile | contratto tra linguaggio, runtime e interfacce |
| Calcolo | grafo delle dipendenze incrementale (approccio tipo Salsa / differential dataflow) | a ogni commit si ricalcola solo ciò che dipende dai campi cambiati |
| Aggregati | mantenuti in modo incrementale per chiave (es. saldo per conto, giacenza per ubicazione) | `sum` su milioni di righe senza ricalcolo completo |
| Numeri | decimali esatti, mai float | nessun centesimo perso |
| Transazioni | serializzabili; un writer per tenant, letture concorrenti | semplice da ragionare e da certificare; un ERP raramente supera qualche centinaio di commit al secondo per azienda |

Le formule (`campo = …`) vengono **materializzate** quando servono a vincoli, indici, permessi o
sono lette spesso. Altrimenti si calcolano su richiesta. La scelta la fa il runtime, ma
il risultato è identico.

---

## 4. Postgres o SQLite

**Proposta: Postgres in cloud, SQLite in locale e nei test, dietro un'unica interfaccia di storage.**
(Implementata in `crates/quadra-store`: gli stessi scenari girano su entrambi e producono
la stessa catena di hash. Dettagli in [docs/IMPLEMENTATION.md](../../docs/IMPLEMENTATION.md).)

| | Postgres | SQLite |
|---|---|---|
| Backup continuo | archiviazione del WAL (pgBackRest, WAL-G): ripristino a qualsiasi secondo | Litestream: buono, ma replica asincrona |
| Nessun dato perso in failover | replica sincrona (RPO = 0) | non nativo |
| DDL transazionale | sì | sì |
| Più processi che scrivono | sì | un solo writer |
| Sviluppo, test, offline | più pesante | perfetto: migliaia di test in memoria in pochi secondi |

"Il database non si rompe mai" non lo garantisce nessun database da solo. Lo garantiscono
insieme:
- replica sincrona più failover automatico;
- point-in-time recovery;
- **ripristino di prova automatico ogni giorno**, perché un backup mai ripristinato non è un backup;
- checksum delle pagine;
- verifica periodica della catena di hash e del replay.

Obiettivi realistici: RPO 0 e RTO sotto il minuto.

Multi-tenant: un database (o uno schema) per tenant. Isola dati, backup, ripristino e
prestazioni, e semplifica l'audit del singolo cliente.

---

## 5. Interfacce

- **Renderer generico in React + Tailwind** che legge l'IR (viste, tipi, permessi) e
  disegna form, liste, pivot e azioni. Non si genera codice per ogni entità: cambia l'IR e
  cambia l'interfaccia, all'istante.
- **Componenti personalizzati** come estensioni con tipi TypeScript generati dall'IR, per
  le poche schermate speciali.
- **Dominio personalizzato** per tenant con certificato TLS automatico.

---

## 6. Integrazioni (fuori dal linguaggio, ma tipizzate dal linguaggio)

Il linguaggio resta puro. Le integrazioni gli stanno attorno:

| Direzione | Meccanismo |
|---|---|
| In uscita | `emit` → outbox transazionale → webhook firmati o connettori. Partono solo dopo il commit, con chiavi di idempotenza e ritentativi. |
| In entrata | API (REST/GraphQL + OpenAPI) generate dall'IR. Possono solo creare, modificare ed eseguire azioni, con gli stessi permessi e vincoli dell'interfaccia. Nessun accesso diretto alle tabelle. |
| Connettori | Scritti in TypeScript o Python con un SDK tipizzato generato dall'IR (`Money`, `Qty`, stati e azioni come tipi). Girano isolati (WASM o container) con segreti gestiti. Ogni scrittura porta come causa `connettore@versione`. |
| Import massivi | Tabelle di staging tipizzate (`temporary entity`), validazione riga per riga, promozione atomica: è lo stesso meccanismo delle migrazioni. |

Connettori da avere presto per il mercato italiano: fattura elettronica SDI, estratti
conto bancari (CBI / camt.053), e-commerce, corrieri.

---

## 7. Migrazioni, correzioni, modifiche massive

Gli esempi sono in [migrations/](migrations/).

| Caso | Strumento | Garanzia |
|---|---|---|
| Cambio di schema (rinomina, cambio tipo, nuovo campo obbligatorio) | `migration` con `rename` / `convert` / `backfill` / `verify` | prova generale obbligatoria; deploy bloccato se restano dati invalidi |
| Dato che non si conosce ancora | `provisional(valore, reason, until)` | il record è valido ma marcato ed elencato; alla scadenza diventa un errore |
| Modifica massiva | `change` con motivazione, richiedente, approvazione, `expect` | anteprima record per record; una transazione; annullabile con un nuovo change |
| Correzione di documenti registrati | solo tramite le azioni del processo (nota di credito + nuova fattura) | contabilità, numerazione e metriche restano coerenti |
| Correzione retrodatata | `effective:` (tempo di validità) distinto dal tempo di registrazione | il revisore vede sia "cosa era vero il 15/11" sia "cosa sapevamo il 15/11" |

---

## 8. Rischi e decisioni aperte

- **Adozione del linguaggio.** Per farlo conoscere servono: playground web, LSP,
  formattatore, documentazione con esempi, e questo spec come modello ERP di partenza.
- **Multivaluta**: supportata (prezzi e costi `per company`); il consolidamento di gruppo è ancora da progettare.
- **Tenant molto grandi** (decine di milioni di movimenti all'anno): da validare presto
  con un benchmark del calcolo incrementale con storico completo.
- **Cosa resta fuori dal linguaggio**: integrazioni, stampe e documenti (PDF), componenti
  UI personalizzati. Va deciso quanto fornire "di serie" e quanto lasciare agli SDK.
