# Quadra — esempio esteso

*I conti quadrano.* Un ERP classico (anagrafiche, vendite, acquisti, magazzino, produzione,
contabilità, fatturazione, pianificazione, permessi, metriche certificate) scritto in Quadra.
È il **caso di test** del linguaggio: se regge questo, regge un ERP vero.

```bash
quadra check examples/extended                 # nomi, tipi, cicli, flussi, metriche certificate
quadra test examples/extended                  # esegue i 13 scenari in memoria
quadra trace Invoice.due examples/extended     # da cosa dipende un valore e chi lo scrive
```

## File

```
core/base.qd        paesi, indirizzi, estensioni di Company, trait Document, eventi
partners.qd         partner con ruoli cliente/fornitore, termini di pagamento
products.qd         prodotti (prezzi e costi per società), categorie, fornitori, listini
accounting.qd       piano dei conti, registri, IVA, scritture, ledger GL, trait InvoiceableLine
inventory.qd        magazzini, ubicazioni, lotti, movimenti, trasferimenti, ledger Stock
sales.qd            offerte/ordini di vendita, consegne, vista, report
purchasing.qd       ordini d'acquisto, approvazione, ricevimenti, costi accessori
manufacturing.qd    centri di lavoro, distinte multilivello, cicli, ordini di produzione
invoicing.qd        fatture attive/passive, note di credito, incassi, differenze cambio, fido
planning.qd         punti di riordino e riordino automatico
metrics.qd          metriche certificate e controlli di audit
metrics.lock        impronte e approvazioni delle metriche (generato da `quadra certify`)
security.qd         ruoli, permessi, separazione dei compiti
migrations/         migrazione di schema, modifica massiva, correzione di documenti registrati
tests/              fixture, scenari order-to-cash e procure/make, output atteso delle analisi
MAPPING.md          corrispondenze con SAP S/4HANA, Odoo, Business Central
PLATFORM.md         cloud, motore, Postgres/SQLite, deploy, integrazioni, audit
```

Valute (`EUR`, `USD`…), unità (`pcs`, `kg`, `h`…), `Currency`, `ExchangeRate`, `Uom`,
`Company`, `User`, `Sequence` e `next_number` vengono dalla **libreria standard**
([std/](../../std)), visibile ovunque senza import.

Gli strati vanno dal basso verso l'alto (`core` → `partners` → `products` → `accounting` →
`inventory` → `sales`/`purchasing`/`manufacturing` → `invoicing` → `planning` → `security`).
Gli import circolari sono vietati: per aggiungere campi a un'entità di uno strato inferiore
si usa `extend`.

---

## Cosa NON si scrive (implicito)

Su ogni `entity`: `id`, `created_at`, `created_by`, `updated_at`, `updated_by`, `version`,
lo storico completo di ogni campo e la causa di ogni modifica.

```python
why(order)                       # catena di cause: chi, cosa, perché
current.user · current.company   # contesto della sessione
```

---

## Cheat sheet

### Dichiarazioni

```python
from inventory import StockMove, Stock    # import (niente cicli tra moduli)

entity Nome is Trait:     # entità persistente e versionata
record Nome:              # valore composto senza identità propria (indirizzo, ruolo)
trait Nome:               # contratto comune (es. Document, InvoiceableLine)
enum Nome: A, B, C        # valori chiusi; anche con dati: OneProduct(product: Product)
extend Nome:              # un altro modulo aggiunge campi, formule, azioni
ledger Nome:              # registro solo-aggiunta alimentato da `post to`
event Nome(campo: Tipo)   # messaggio verso l'esterno (emit)
fn nome(a: T) -> R:       # funzione pura
seed:                     # dati di riferimento versionati col codice
const NOME: T = expr      # costante
```

### Campi

```python
campo: T                          # obbligatorio
campo: T?                         # opzionale
campo: T? required when cond      # opzionale finché cond è falsa
campo: T default expr             # VALORE INIZIALE: fotografato alla creazione, poi modificabile
campo = expr                      # FORMULA: sempre aggiornata, sola lettura
campo: T = expr                   # formula con tipo dichiarato
campo default expr                # (senza tipo) nuovo default di un campo ereditato dal trait
campo: T per company              # un valore per ogni società: prodotto.costo[società]
campo: T where > 0                # dominio: confronto, intervallo (0..4) o lambda (v => …)
unique (a, b)                     # unicità composta, anche parziale: `unique (a) when cond`
check cond, "msg"                 # invariante bloccante, verificato a ogni commit
warn cond, "msg"                  # regola morbida: non blocca, resta tracciata
frozen when cond                  # dopo, il record vede il mondo com'era in quel momento
frozen when cond except a live b  # `except`: campi ancora modificabili; `live`: formule vive
```

`=` (formula) contro `default` (fotografia) è la distinzione più importante:
`amount = qty * price` segue sempre i dati, `price default product.price` fotografa il listino.
Una fotografia spezza anche i cicli tra formule (vedi `StockMove.unit_cost`).

### Relazioni e percorsi

```python
customer: Partner                    # molti-a-uno
orders: [SalesOrder]                 # uno-a-molti: inverso dedotto (o `via campo`)
order: SalesOrder parent             # composizione: la riga vive e muore con l'ordine
tags: {PartnerTag}                   # molti-a-molti semplice
vendors: {Partner} through SupplierInfo   # molti-a-molti con attributi
origin: Document?                    # riferimento a un trait: qualsiasi entità `is Document`

o.customer.address.country           # uno → uno
p.as_customer?.credit_limit          # `?.` obbligatorio su un opzionale (altrimenti non compila)
c.closure(x => x.parent)             # chiusura transitiva: alberi, distinte multilivello
```

### Collezioni: funzioni esplicite

```python
lines.where(l => l.qty > 0).sum(l => l.subtotal)
moves.where(m => m.product == product)        # `product` senza prefisso = campo dell'entità
lines.group_by(l => l.tax).map(g => (tax: g.key, base: g.items.sum(l => l.subtotal)))
moves.allocate(100.00 EUR, by=m => m.value.raw)   # → coppie (item, share), somma esatta
```

### Tipi "intelligenti"

```python
Money<C>      # importo in valuta C (anche da un campo: Money<order.currency>)
Price<C, U>   # prezzo unitario: Money per unità U
Qty<U>        # quantità nell'unità U (anche da un campo: Qty<product.uom>)
Exact<T>      # risultato non ancora arrotondato
enum(A, B)    # insieme chiuso di valori (niente unioni anonime)

10.50 EUR · 3 pcs · 2.5 kg · 9.90 EUR/pcs · 60 EUR/h · 22% · 2026-11-21 · 30 days
```

| Il compilatore rifiuta | Perché |
|---|---|
| `10.00 EUR + 5.00 USD` | valute diverse: si passa da `convert()` |
| `SalesLine.all().sum(l => l.subtotal)` | ogni riga ha la sua valuta: va filtrato o convertito |
| `2 kg + 3 pcs`, `2 kg * 10.00 EUR/pcs` | unità di categorie diverse |
| `price default price * (1 - discount)` | un importo calcolato va arrotondato (`.round()`) prima di essere memorizzato |
| `p.as_customer.credit_limit` | `as_customer` è opzionale: `?.` |
| `age = now() - date` | le formule devono dare sempre lo stesso risultato |
| `match state:` senza un caso | i `match` sugli enum sono esaustivi |
| formule che si richiamano in cerchio | salvo ricorsione su dati aciclici (garantita da un `check` con `closure`) |

Il compilatore usa anche ciò che il codice dichiara: i filtri (`g.company == company`) e i
vincoli dei campi (`invoice where i => i.currency == payment.currency`) gli dicono che due
valute coincidono.

### Stati e flussi

```python
state: enum(Draft, Confirmed, Done, Cancelled) default Draft
flow state:
    Draft -> Confirmed           by confirm              # transizione manuale (azione)
    Draft -> ToApprove           by confirm when cond    # stessa azione, destinazione diversa
    Confirmed -> Done            when cond               # transizione automatica
    Draft | Confirmed -> Cancelled by cancel unless cond # guardia

state: enum(Empty, Done, Waiting) = when:                # stato come riepilogo dei figli
    moves.count() == 0                => Empty
    moves.all(m => m.state is Done)   => Done
    else                              => Waiting
```

### Azioni, reazioni, effetti

```python
action confirm:                   # transazione atomica
    require cond, "msg"
    emit SendEmail(...)           # effetto esterno: parte DOPO il commit (outbox)

on SalesOrder enters Confirmed as o:                  # reazione a una transizione
on ReorderRule becomes to_order > 0 as r converges:   # quando la condizione diventa vera
before SalesOrder.confirm as o:                       # precondizione aggiunta da un altro modulo
```

### Ledger

```python
ledger Stock:
    key company: Company
    key product: Product
    measure qty: Qty<product.uom>
    measure value: Money<company.currency>
    check per (product): g => g.sum(r => r.qty) >= 0, "Giacenza negativa"

entity StockMove:
    post to Stock when state is Done:
        add(company=company, product=product, qty=qty_base, value=value)
```

Se la condizione di `post` torna falsa, lo **storno è automatico**. Giacenze e saldi sono
letture del ledger, non campi che qualcuno aggiorna a mano.

### Viste, report, ruoli, test

```python
view SalesOrderForm(o: SalesOrder):
    table o.lines as l: l.product, l.qty, l.price
report SalesAnalysis:
    from SalesLine as l where l.order.state is Confirmed
role Salesperson is Employee:
    edit SalesOrder where o => o.salesperson == me
deny run PurchaseOrder.approve where o => o.created_by == me   # vale per tutti
test "nome":
    use italy                              # fixture (una transazione)
    clock 2026-11-21 09:00                 # fissa now()/today()
    as giulia: so.confirm()                # con i permessi di un utente
    expect so.state is Confirmed
    expect_error "Fido superato": so.confirm()
    expect_compile_error "different currencies": let x = …
```

### Audit e manutenzione

```python
metric revenue(company: Company, period: DateRange) -> Money<company.currency>:
control trial_balance:                 # verifica continua
migration customs_code:                # accompagna un cambio di schema
change price_list_2027:                # modifica massiva motivata e approvata
provisional("00000000", reason=…, until=…)
```

---

## Domande ancora aperte

1. **Etichette e traduzioni** dei campi (`name: Text "Ragione sociale"`).
2. **Vista unificata** di un'entità sparsa su più file con `extend`: l'hover dell'editor la
   mostra già. Serve anche un comando (`quadra show Product`)?
3. **Permessi sui campi derivati**: se un ruolo non vede `Product.cost`, deve vedere
   `SalesLine.margin`, che ne deriva?
