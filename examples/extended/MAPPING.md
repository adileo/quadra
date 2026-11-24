# Mappatura con SAP S/4HANA, Odoo e Business Central

Ogni entità dello spec accanto agli oggetti equivalenti dei tre ERP più diffusi.
Serve a verificare che lo spec copra i processi classici e a dare un vocabolario comune
a chi viene da quei mondi.

Fonti: per SAP, tabelle standard di S/4HANA (BUT000, MARA, VBAK/VBAP, EKKO/EKPO, MATDOC,
ACDOCA); per Odoo, i modelli tecnici (`res.partner`, `sale.order`, `stock.move`, `mrp.bom`,
`account.move`…); per Business Central, gli ID delle tabelle standard. Gli ID BC di
Customer (18), Item (27), Sales Header (36) e Item Ledger Entry (32) sono verificati, gli
altri vengono da memoria e vanno ricontrollati.

| Spec | File | SAP S/4HANA | Odoo | Business Central |
|---|---|---|---|---|
| Company | core/base | T001 (società) | res.company | Company Information (79) |
| Sequence | core/base | NRIV (intervalli di numerazione) | ir.sequence | No. Series (308) |
| Currency | core/money | TCURC | res.currency | Currency (4) |
| ExchangeRate | core/money | TCURR | res.currency.rate | Currency Exchange Rate (330) |
| Uom, UomCategory | core/uom | T006, T006D | uom.uom, uom.category | Unit of Measure (204), Item Unit of Measure (5404) |
| Partner (+ ruoli) | partners | BUT000 + KNA1/KNVV (cliente), LFA1/LFM1 (fornitore) | res.partner | Customer (18), Vendor (23), Contact (5050) |
| PaymentTerm | partners | T052 | account.payment.term | Payment Terms (3) |
| Category | products | T023 (gruppo merci) | product.category | Item Category (5722) |
| Product | products | MARA, MAKT, MARC, MBEW | product.template / product.product | Item (27) |
| SupplierInfo | products | EINA/EINE (info record) | product.supplierinfo | Item Vendor (99) |
| Pricelist, PricelistRule | products | condizioni KONP / A0xx, PRCD_ELEMENTS | product.pricelist(.item) | Price List Header/Line (7000/7001) |
| Account | accounting | SKA1/SKB1 | account.account | G/L Account (15) |
| Journal | accounting | tipi documento T003 | account.journal | Gen. Journal Template/Batch (80/232) |
| Tax | accounting | codici IVA T007A | account.tax | VAT Posting Setup (325) |
| JournalEntry, JournalLine | accounting | BKPF + ACDOCA | account.move (entry) / account.move.line | Gen. Journal Line (81) → G/L Entry (17) |
| ledger GL | accounting | ACDOCA (Universal Journal) | account.move.line registrate | G/L Entry (17) |
| Warehouse | inventory | T001W (divisione) | stock.warehouse | Location (14) |
| Location | inventory | T001L (magazzino), ubicazioni EWM | stock.location | Bin (7354) |
| Lot | inventory | MCH1/MCHA | stock.lot | Lot No. Information (6505) |
| StockMove | inventory | MATDOC (documento materiale) | stock.move / stock.move.line | Item Journal Line (83) → Item Ledger Entry (32) |
| ledger Stock | inventory | MATDOC (aggregati), MARD | stock.quant | Item Ledger Entry (32), Value Entry (5802) |
| Transfer | inventory | LIKP/LIPS (consegna), consegna in entrata | stock.picking | Warehouse Shipment/Receipt (7320/7316) |
| InventoryCount | inventory | IKPF/ISEG (inventario fisico) | stock.quant (rettifica) | Phys. Invt. Order Header (5875) |
| SalesOrder, SalesLine | sales | VBAK/VBAP/VBEP | sale.order / sale.order.line | Sales Header/Line (36/37) |
| PurchaseOrder, PurchaseLine | purchasing | EKKO/EKPO/EKET | purchase.order / purchase.order.line | Purchase Header/Line (38/39) |
| approvazione acquisti | purchasing | strategia di rilascio (release strategy) | purchase: doppia validazione | Approval Entry (454) |
| LandedCost | purchasing | condizioni di costo accessorio / LCM | stock.landed.cost | Item Charge (5800), Item Charge Assignment (5805) |
| WorkCenter | manufacturing | CRHD | mrp.workcenter | Work Center (99000754) |
| Bom, BomLine | manufacturing | STKO/STPO, MAST | mrp.bom / mrp.bom.line | Production BOM Header/Line (99000771/99000772) |
| Operation | manufacturing | PLKO/PLPO (ciclo) | mrp.routing.workcenter | Routing Header/Line (99000763/99000764) |
| ProductionOrder | manufacturing | AUFK/AFKO/AFPO, RESB | mrp.production | Production Order (5405), Prod. Order Line/Component (5406/5407) |
| WorkOrder, TimeLog | manufacturing | AFVC, AFRU (conferme) | mrp.workorder, mrp.workcenter.productivity | Prod. Order Routing Line (5409), Capacity Ledger Entry (5832) |
| Invoice (attiva) | invoicing | VBRK/VBRP + documento FI | account.move (out_invoice/out_refund) | Sales Invoice/Cr. Memo Header (112/114) |
| Invoice (passiva) | invoicing | RBKP/RSEG (MIRO) | account.move (in_invoice/in_refund) | Purch. Inv. Header (122) |
| Payment | invoicing | pareggio FI (F-28/F-53), Payment Run | account.payment | Cust./Vendor Ledger Entry (21/25) |
| PaymentAllocation | invoicing | pareggio (AUGBL) | account.partial.reconcile | Detailed Cust./Vendor Ledg. Entry (379/380) |
| ReorderRule | planning | dati MRP in MARC (punto di riordino) | stock.warehouse.orderpoint | Stockkeeping Unit (5700) |
| role, deny | security | ruoli PFCG (AGR_*) | res.groups, ir.model.access, ir.rule | Permission Set |
| metric, control | metrics | Embedded Analytics / GRC | (report) | (report) |
| why(), storico | implicito | VBFA (flusso documenti), CDHDR/CDPOS (documenti di modifica) | mail.tracking.value | Change Log Entry (405) |

## Concetti dello spec senza un equivalente diretto

- **Ledger con storno automatico**: negli altri ERP lo storno è un documento creato a mano o
  da una funzione dedicata; qui segue la condizione di `post`.
- **Impronta delle metriche certificate**: negli ERP classici la definizione di un KPI vive
  in un report o in un documento, non è collegata al codice che la calcola.
- **`why()` sulle cause**: VBFA e i change document di SAP tracciano rispettivamente il
  flusso dei documenti e le modifiche ai campi, ma non la catena causale tra automazioni.

## Non ancora coperti

Varianti di prodotto (product.product / configurazione SAP), valorizzazione FIFO, resi,
backorder e consegne parziali, MRP completo con calcolo dei fabbisogni, intercompany,
cespiti, HR/paghe, CRM, progetti, fattura elettronica XML (SDI).
