mod support;

use ha_utilities::{
    imports::{
        BalanceInput, BillInput, BillReference, BillScope, BillTotals, DeliveryInput, ExactDecimal,
        ImportAccount, ImportAccountConsolidation, ImportAccountPurge, ImportBillPurge,
        ImportBillPurgeReceipt, ImportError, ImportProvider, ImportRecord, ImportRequest,
        MeterIntervalInput, PaymentInput, PaymentStatus, Quality, Quantity, QuantityUnit,
        SourceRef, SourceRole, digest,
    },
    models::{ProviderId, Service, ServiceKind},
    store::Store,
};
use rust_decimal::Decimal;

fn source(digest: &str, role: SourceRole) -> SourceRef {
    SourceRef {
        sha256: digest.to_owned(),
        role,
        pages: Vec::new(),
    }
}

fn bill(reference: &str, document: &str) -> ImportRecord {
    ImportRecord::Bill(BillInput {
        reference: BillReference::InvoiceNumber(reference.to_owned()),
        scope: BillScope::Account,
        issued_on: "2025-02-20".parse().unwrap(),
        due_on: Some("2025-03-10".parse().unwrap()),
        period: None,
        totals: BillTotals {
            new_charges_minor: Some(12500),
            amount_due_minor: Some(12500),
            ..BillTotals::default()
        },
        services: Vec::new(),
        charges: Vec::new(),
        sources: vec![source(document, SourceRole::Invoice)],
        replaces_revision: None,
    })
}

fn account(id: &str) -> ImportAccount {
    ImportAccount {
        id: id.to_owned(),
        provider: ImportProvider::Spectrum,
        start_on: "2025-02-01".parse().unwrap(),
        provider_account_sha256: Some(digest(id.as_bytes())),
        services: vec![Service {
            id: format!("{id}-internet"),
            kind: ServiceKind::Internet,
        }],
    }
}

struct Fixture {
    original: ImportRequest,
    correction: ImportRequest,
    other: ImportRequest,
    bill_id: String,
    request: ImportBillPurge,
    shared: String,
    exclusive: Vec<String>,
}

async fn fixture(store: &Store) -> Fixture {
    let bytes = [
        b"Fictional shared source".as_slice(),
        b"Fictional original invoice",
        b"Fictional corrected invoice",
        b"Fictional payment confirmation",
        b"Fictional unrelated orphan",
    ];
    let mut docs = Vec::new();
    for bytes in bytes {
        let key = digest(bytes);
        store.put_document("demo", &key, bytes).await.unwrap();
        docs.push(key);
    }
    let mut removed_bill = bill("EXAMPLE-REMOVE", &docs[1]);
    let ImportRecord::Bill(value) = &mut removed_bill else {
        panic!("bill fixture")
    };
    value.sources.push(source(&docs[0], SourceRole::Supporting));
    let original = ImportRequest {
        account: account("fixture-construction"),
        currency: "USD".to_owned(),
        records: vec![
            removed_bill,
            bill("EXAMPLE-KEEP", &docs[0]),
            ImportRecord::Payment(PaymentInput {
                reference: "EXAMPLE-PAYMENT".to_owned(),
                bill: Some(BillReference::InvoiceNumber("EXAMPLE-REMOVE".to_owned())),
                paid_on: "2025-03-01".parse().unwrap(),
                amount_minor: 12500,
                status: PaymentStatus::Confirmed,
                sources: vec![source(&docs[3], SourceRole::PaymentConfirmation)],
                replaces_revision: None,
            }),
            ImportRecord::BalanceSnapshot(BalanceInput {
                as_of: "2025-03-01T00:00:00Z".parse().unwrap(),
                amount_minor: 12500,
                sources: vec![source(&docs[0], SourceRole::Statement)],
                replaces_revision: None,
            }),
            ImportRecord::MeterInterval(MeterIntervalInput {
                service_id: "fixture-construction-internet".to_owned(),
                start: "2025-02-20T00:00:00Z".parse().unwrap(),
                end: "2025-02-21T00:00:00Z".parse().unwrap(),
                quantity: Quantity {
                    value: ExactDecimal(Decimal::from(512)),
                    unit: QuantityUnit::Byte,
                },
                quality: Quality::Unknown,
                sources: vec![source(&docs[0], SourceRole::UsageExport)],
                replaces_revision: None,
            }),
        ],
    };
    store.import("demo", original.clone()).await.unwrap();
    let (bill_id, revision): (String,String) = sqlx::query_as("SELECT id,revision FROM imported_records WHERE kind='bill' AND json_extract(payload,'$.reference.value')='EXAMPLE-REMOVE'")
        .fetch_one(store.pool()).await.unwrap();
    let mut corrected = original.records[0].clone();
    let ImportRecord::Bill(value) = &mut corrected else {
        panic!("bill fixture")
    };
    value.sources = vec![source(&docs[2], SourceRole::Invoice)];
    value.totals.new_charges_minor = Some(15000);
    value.totals.amount_due_minor = Some(15000);
    value.replaces_revision = Some(revision);
    let correction = ImportRequest {
        records: vec![corrected],
        ..original.clone()
    };
    store.import("demo", correction.clone()).await.unwrap();
    let revision = store
        .bill_details("demo", &bill_id)
        .await
        .unwrap()
        .bill
        .revision;
    let other = ImportRequest {
        account: account("fixture-recurring"),
        currency: "USD".to_owned(),
        records: vec![bill("EXAMPLE-RECURRING", &docs[0])],
    };
    store.import("demo", other.clone()).await.unwrap();
    Fixture {
        request: ImportBillPurge {
            origin_id: original.account.id.clone(),
            expected_revision: revision,
            remove_unused_services: Vec::new(),
        },
        original,
        correction,
        other,
        bill_id,
        shared: docs[0].clone(),
        exclusive: vec![docs[1].clone(), docs[2].clone(), docs[3].clone()],
    }
}

async fn consolidate(store: &Store, fixture: &Fixture, source_records: u64) {
    store
        .consolidate_import_accounts(
            "demo",
            ImportAccountConsolidation {
                source: fixture.original.account.clone(),
                target: fixture.other.account.clone(),
                currency: "USD".to_owned(),
                expected_source_records: source_records,
                expected_target_records: 1,
            },
        )
        .await
        .unwrap();
}

#[derive(Debug, PartialEq, Eq)]
struct Unrelated {
    records: Vec<(String, String, String, String, String)>,
    revisions: Vec<(String, String, String, String, String)>,
    observations: Vec<(String, String, String, String)>,
    receipts: Vec<(String, String)>,
    accounts: Vec<(String, String, String, Option<String>)>,
    origins: Vec<(String, String, Option<String>)>,
}

async fn unrelated(store: &Store, bill_id: &str) -> Unrelated {
    Unrelated {
        records: sqlx::query_as("SELECT account_id,kind,id,revision,payload FROM imported_records WHERE id<>? AND (bill_id IS NULL OR bill_id<>?) ORDER BY account_id,kind,id")
            .bind(bill_id).bind(bill_id).fetch_all(store.pool()).await.unwrap(),
        revisions: sqlx::query_as("SELECT r.account_id,r.kind,r.id,r.revision,r.facts FROM imported_revisions r JOIN imported_records raw USING(account_id,kind,id) WHERE raw.id<>? AND (raw.bill_id IS NULL OR raw.bill_id<>?) ORDER BY r.account_id,r.kind,r.id,r.revision")
            .bind(bill_id).bind(bill_id).fetch_all(store.pool()).await.unwrap(),
        observations: sqlx::query_as("SELECT o.account_id,o.id,o.payload,o.observed_at FROM observations o JOIN imported_records raw ON raw.account_id=o.account_id AND raw.id=o.id WHERE raw.id<>? AND (raw.bill_id IS NULL OR raw.bill_id<>?) ORDER BY o.account_id,o.kind,o.id")
            .bind(bill_id).bind(bill_id).fetch_all(store.pool()).await.unwrap(),
        receipts: sqlx::query_as("SELECT origin_id,digest FROM import_receipts ORDER BY origin_id,digest").fetch_all(store.pool()).await.unwrap(),
        accounts: sqlx::query_as("SELECT id,binding_hash,binding,imported_at FROM accounts ORDER BY id").fetch_all(store.pool()).await.unwrap(),
        origins: sqlx::query_as("SELECT id,account_id,binding FROM import_origins ORDER BY id").fetch_all(store.pool()).await.unwrap(),
    }
}

#[tokio::test]
async fn bill_purge_preserves_unrelated_state_and_rejects_original_and_unlinked_replays() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("bills.sqlite3"))
        .await
        .unwrap();
    let f = fixture(&store).await;
    consolidate(&store, &f, 5).await;
    let before = unrelated(&store, &f.bill_id).await;
    // The canonical owner is shared, so the immutable origin must still guard the bill.
    let mut wrong_origin = f.request.clone();
    wrong_origin.origin_id.clone_from(&f.other.account.id);
    assert!(matches!(
        store
            .purge_import_bill("demo", &f.bill_id, wrong_origin)
            .await,
        Err(ImportError::Binding)
    ));
    let (first, second, old_import) = tokio::join!(
        store.purge_import_bill("demo", &f.bill_id, f.request.clone()),
        store.purge_import_bill("demo", &f.bill_id, f.request.clone()),
        store.import("demo", f.original.clone())
    );
    let results = [first.unwrap(), second.unwrap()];
    assert_eq!(results.iter().filter(|r| r.removed).count(), 1);
    assert_eq!(results.iter().map(|r| r.records).sum::<u64>(), 2);
    assert_eq!(results.iter().map(|r| r.documents).sum::<u64>(), 3);
    assert!(
        matches!(old_import,Ok(ref r) if r.replayed)
            || matches!(old_import, Err(ImportError::PurgedBill))
    );
    assert_eq!(unrelated(&store, &f.bill_id).await, before);
    assert!(store.bill_details("demo", &f.bill_id).await.is_err());
    assert_eq!(
        store.document("demo", &f.shared).await.unwrap(),
        b"Fictional shared source"
    );
    for document in &f.exclusive {
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM import_documents WHERE digest=?")
                .bind(document)
                .fetch_one(store.pool())
                .await
                .unwrap(),
            0
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM import_documents")
            .fetch_one(store.pool())
            .await
            .unwrap(),
        2
    );
    for stale in [f.original.clone(), f.correction.clone()] {
        assert!(matches!(
            store.import("demo", stale).await,
            Err(ImportError::PurgedBill)
        ));
    }
    let mut payment = f.original.clone();
    payment.records = vec![f.original.records[2].clone()];
    let ImportRecord::Payment(value) = &mut payment.records[0] else {
        panic!("payment fixture")
    };
    value.reference = "EXAMPLE-NEW-LINKED-PAYMENT".to_owned();
    assert!(matches!(
        store.import("demo", payment.clone()).await,
        Err(ImportError::PurgedBill)
    ));
    let ImportRecord::Payment(value) = &mut payment.records[0] else {
        panic!("payment fixture")
    };
    value.reference = "EXAMPLE-PAYMENT".to_owned();
    value.bill = None;
    assert!(matches!(
        store.import("demo", payment).await,
        Err(ImportError::PurgedBill)
    ));
    // A mixed old receipt must be rejected before legacy unknown-usage repair.
    sqlx::query("DELETE FROM observations WHERE kind='usage'")
        .execute(store.pool())
        .await
        .unwrap();
    assert!(matches!(
        store.import("demo", f.original.clone()).await,
        Err(ImportError::PurgedBill)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM observations WHERE kind='usage'")
            .fetch_one(store.pool())
            .await
            .unwrap(),
        0
    );
    assert!(
        store
            .import("demo", f.other.clone())
            .await
            .unwrap()
            .replayed
    );
    let mut wrong_revision = f.request.clone();
    wrong_revision.expected_revision = digest(b"wrong revision");
    assert!(matches!(
        store
            .purge_import_bill("demo", &f.bill_id, wrong_revision)
            .await,
        Err(ImportError::Conflict)
    ));
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.pool())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn bill_purge_rejects_scope_revision_and_native_ownership_and_rolls_back_failed_gc() {
    let directory = tempfile::tempdir().unwrap();
    let store = support::populated(&directory.path().join("bills.sqlite3")).await;
    let f = fixture(&store).await;
    let before = unrelated(&store, &f.bill_id).await;
    let mut wrong = f.request.clone();
    wrong.expected_revision = digest(b"wrong");
    assert!(matches!(
        store.purge_import_bill("demo", &f.bill_id, wrong).await,
        Err(ImportError::Conflict)
    ));
    assert!(matches!(
        store
            .purge_import_bill("other", &f.bill_id, f.request.clone())
            .await,
        Err(ImportError::Binding)
    ));
    let mut wrong = f.request.clone();
    wrong.origin_id = "demo-wm".to_owned();
    assert!(matches!(
        store
            .purge_import_bill("demo", "demo-wm:bill:0", wrong)
            .await,
        Err(ImportError::Binding)
    ));
    sqlx::query("UPDATE accounts SET import_only=0 WHERE id=?")
        .bind(&f.original.account.id)
        .execute(store.pool())
        .await
        .unwrap();
    assert!(matches!(
        store
            .purge_import_bill("demo", &f.bill_id, f.request.clone())
            .await,
        Err(ImportError::Binding)
    ));
    sqlx::query("UPDATE accounts SET import_only=1 WHERE id=?")
        .bind(&f.original.account.id)
        .execute(store.pool())
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER fixture_gc_failure BEFORE DELETE ON import_documents BEGIN SELECT RAISE(ABORT,'fictional GC failure'); END")
        .execute(store.pool()).await.unwrap();
    assert!(matches!(
        store
            .purge_import_bill("demo", &f.bill_id, f.request.clone())
            .await,
        Err(ImportError::Store(_))
    ));
    assert_eq!(unrelated(&store, &f.bill_id).await, before);
    assert!(store.bill_details("demo", &f.bill_id).await.is_ok());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM import_bill_tombstones")
            .fetch_one(store.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM import_bill_removed_records")
            .fetch_one(store.pool())
            .await
            .unwrap(),
        0
    );
    for document in &f.exclusive {
        assert!(store.document("demo", document).await.is_ok());
    }
    sqlx::query("DROP TRIGGER fixture_gc_failure")
        .execute(store.pool())
        .await
        .unwrap();
    assert!(
        store
            .purge_import_bill("demo", &f.bill_id, f.request)
            .await
            .unwrap()
            .removed
    );
}

#[tokio::test]
async fn bill_tombstones_survive_consolidation_and_cascade_when_the_canonical_account_is_purged() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("bills.sqlite3"))
        .await
        .unwrap();
    let f = fixture(&store).await;
    store
        .purge_import_bill("demo", &f.bill_id, f.request.clone())
        .await
        .unwrap();
    consolidate(&store, &f, 3).await;
    assert_eq!(
        store
            .purge_import_bill("demo", &f.bill_id, f.request.clone())
            .await
            .unwrap(),
        ImportBillPurgeReceipt::default()
    );
    assert!(matches!(
        store.import("demo", f.original).await,
        Err(ImportError::PurgedBill)
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM import_bill_tombstones")
            .fetch_one(store.pool())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM import_bill_removed_records")
            .fetch_one(store.pool())
            .await
            .unwrap(),
        2
    );
    assert!(
        store
            .purge_import_account(
                "demo",
                &f.other.account.id,
                ImportAccountPurge {
                    provider: ProviderId::Spectrum,
                    expected_records: 4
                }
            )
            .await
            .unwrap()
            .removed
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM import_bill_tombstones")
            .fetch_one(store.pool())
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM import_bill_removed_records")
            .fetch_one(store.pool())
            .await
            .unwrap(),
        0
    );
    assert!(
        sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(store.pool())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn linked_propane_delivery_is_removed_and_its_original_id_stays_retired() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("propane.sqlite3"))
        .await
        .unwrap();
    let document = digest(b"Fictional propane source");
    store
        .put_document("demo", &document, b"Fictional propane source")
        .await
        .unwrap();
    let original = ImportRequest {
        account: ImportAccount {
            id: "fictional-propane".to_owned(),
            provider: ImportProvider::SuperiorPropane,
            start_on: "2025-02-01".parse().unwrap(),
            provider_account_sha256: None,
            services: vec![Service {
                id: "fictional-propane-service".to_owned(),
                kind: ServiceKind::Propane,
            }],
        },
        currency: "USD".to_owned(),
        records: vec![
            bill("EXAMPLE-REMOVE", &document),
            ImportRecord::PropaneDelivery(DeliveryInput {
                ticket: "EXAMPLE-DELIVERY".to_owned(),
                service_id: "fictional-propane-service".to_owned(),
                delivered_on: "2025-02-20".parse().unwrap(),
                volume: Quantity {
                    value: ExactDecimal(Decimal::from(100)),
                    unit: QuantityUnit::UsGallon,
                },
                bill: Some(BillReference::InvoiceNumber("EXAMPLE-REMOVE".to_owned())),
                sources: vec![source(&document, SourceRole::DeliveryTicket)],
                replaces_revision: None,
            }),
        ],
    };
    store.import("demo", original.clone()).await.unwrap();
    let id: String = sqlx::query_scalar("SELECT id FROM imported_records WHERE kind='bill'")
        .fetch_one(store.pool())
        .await
        .unwrap();
    let request = ImportBillPurge {
        origin_id: original.account.id.clone(),
        expected_revision: store.bill_details("demo", &id).await.unwrap().bill.revision,
        remove_unused_services: Vec::new(),
    };
    assert_eq!(
        store.purge_import_bill("demo", &id, request).await.unwrap(),
        ImportBillPurgeReceipt {
            removed: true,
            records: 2,
            documents: 1
        }
    );
    let mut unlinked = original.clone();
    let mut delivery = original.records[1].clone();
    let ImportRecord::PropaneDelivery(value) = &mut delivery else {
        panic!("delivery fixture")
    };
    value.bill = None;
    unlinked.records = vec![delivery];
    assert!(matches!(
        store.import("demo", unlinked).await,
        Err(ImportError::PurgedBill)
    ));
}

async fn installation_fixture(
    store: &Store,
) -> (ImportRequest, ImportRequest, String, ImportBillPurge) {
    let document = digest(b"Fictional installation and regular sources");
    store
        .put_document(
            "demo",
            &document,
            b"Fictional installation and regular sources",
        )
        .await
        .unwrap();
    let original = ImportRequest {
        account: account("fictional-installation"),
        currency: "USD".to_owned(),
        records: vec![bill("EXAMPLE-INSTALLATION", &document)],
    };
    let target = ImportRequest {
        account: account("fictional-regular"),
        currency: "USD".to_owned(),
        records: vec![bill("EXAMPLE-REGULAR", &document)],
    };
    store.import("demo", original.clone()).await.unwrap();
    store.import("demo", target.clone()).await.unwrap();
    let id: String = sqlx::query_scalar("SELECT id FROM imported_records WHERE account_id=?")
        .bind(&original.account.id)
        .fetch_one(store.pool())
        .await
        .unwrap();
    let request = ImportBillPurge {
        origin_id: original.account.id.clone(),
        expected_revision: store.bill_details("demo", &id).await.unwrap().bill.revision,
        remove_unused_services: vec![original.account.services[0].id.clone()],
    };
    store
        .consolidate_import_accounts(
            "demo",
            ImportAccountConsolidation {
                source: original.account.clone(),
                target: target.account.clone(),
                currency: "USD".to_owned(),
                expected_source_records: 1,
                expected_target_records: 1,
            },
        )
        .await
        .unwrap();
    (original, target, id, request)
}

#[tokio::test]
async fn explicit_unused_service_removal_preserves_regular_service_and_retry_intent() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("installation.sqlite3"))
        .await
        .unwrap();
    let (original, target, id, request) = installation_fixture(&store).await;
    assert!(
        store
            .purge_import_bill("demo", &id, request.clone())
            .await
            .unwrap()
            .removed
    );
    let binding = store.binding("demo", &target.account.id).await.unwrap();
    assert_eq!(binding.services.len(), 1);
    assert_eq!(binding.services[0].id, target.account.services[0].id);
    assert!(store.import("demo", target).await.unwrap().replayed);
    assert!(matches!(
        store.import("demo", original.clone()).await,
        Err(ImportError::PurgedBill)
    ));
    let future = ImportRequest {
        records: vec![ImportRecord::MeterInterval(MeterIntervalInput {
            service_id: original.account.services[0].id.clone(),
            start: "2025-02-20T00:00:00Z".parse().unwrap(),
            end: "2025-02-21T00:00:00Z".parse().unwrap(),
            quantity: Quantity {
                value: ExactDecimal(Decimal::from(512)),
                unit: QuantityUnit::Byte,
            },
            quality: Quality::Estimated,
            sources: original.records[0].sources().to_vec(),
            replaces_revision: None,
        })],
        ..original.clone()
    };
    assert!(matches!(
        store.import("demo", future).await,
        Err(ImportError::Binding)
    ));
    let future_bill = ImportRequest {
        records: vec![bill(
            "EXAMPLE-RETIRED-ACCOUNT",
            &original.records[0].sources()[0].sha256,
        )],
        ..original
    };
    assert!(matches!(
        store.import("demo", future_bill).await,
        Err(ImportError::Binding)
    ));
    assert_eq!(
        store
            .purge_import_bill("demo", &id, request.clone())
            .await
            .unwrap(),
        ImportBillPurgeReceipt::default()
    );
    let mut changed = request.clone();
    changed.remove_unused_services.clear();
    assert!(matches!(
        store.purge_import_bill("demo", &id, changed).await,
        Err(ImportError::Conflict)
    ));
    let mut duplicate = request;
    duplicate
        .remove_unused_services
        .push(duplicate.remove_unused_services[0].clone());
    assert!(matches!(duplicate.validate(), Err(ImportError::Invalid)));
}

#[tokio::test]
async fn removing_a_service_owned_by_another_origin_is_rejected_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(&directory.path().join("shared-service.sqlite3"))
        .await
        .unwrap();
    let (original, target, id, request) = installation_fixture(&store).await;
    let before = unrelated(&store, &id).await;
    let mut wrong = request.clone();
    wrong.remove_unused_services = vec![target.account.services[0].id.clone()];
    assert!(matches!(
        store.purge_import_bill("demo", &id, wrong).await,
        Err(ImportError::Binding)
    ));
    assert_eq!(unrelated(&store, &id).await, before);
    let mut legacy = store.binding("demo", &target.account.id).await.unwrap();
    legacy.id = "fictional-shared-origin".to_owned();
    legacy.provider_account = digest(legacy.id.as_bytes());
    legacy
        .services
        .retain(|service| service.id == original.account.services[0].id);
    sqlx::query(
        "INSERT INTO import_origins(id,account_id,binding,start_on) VALUES(?,?,?,'2025-02-01')",
    )
    .bind(&legacy.id)
    .bind(&target.account.id)
    .bind(serde_json::to_string(&legacy).unwrap())
    .execute(store.pool())
    .await
    .unwrap();
    assert!(matches!(
        store.purge_import_bill("demo", &id, request).await,
        Err(ImportError::Binding)
    ));
    assert!(store.bill_details("demo", &id).await.is_ok());
    assert_eq!(
        store
            .binding("demo", &target.account.id)
            .await
            .unwrap()
            .services
            .len(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM import_bill_tombstones")
            .fetch_one(store.pool())
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn surviving_estimates_and_account_wide_bills_prevent_service_removal() {
    for estimated in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("service-references.sqlite3"))
            .await
            .unwrap();
        let (original, target, id, request) = installation_fixture(&store).await;
        let document = &original.records[0].sources()[0].sha256;
        // More than one query page must be checked before the final meter record.
        let bulk = ImportRequest {
            records: (0..200)
                .map(|i| bill(&format!("EXAMPLE-REGULAR-{i}"), document))
                .collect(),
            ..target.clone()
        };
        store.import("demo", bulk).await.unwrap();
        let record = if estimated {
            ImportRecord::MeterInterval(MeterIntervalInput {
                service_id: original.account.services[0].id.clone(),
                start: "2025-02-20T00:00:00Z".parse().unwrap(),
                end: "2025-02-21T00:00:00Z".parse().unwrap(),
                quantity: Quantity {
                    value: ExactDecimal(Decimal::from(512)),
                    unit: QuantityUnit::Byte,
                },
                quality: Quality::Estimated,
                sources: vec![source(document, SourceRole::UsageExport)],
                replaces_revision: None,
            })
        } else {
            bill("EXAMPLE-SURVIVING-ACCOUNT-BILL", document)
        };
        store
            .import(
                "demo",
                ImportRequest {
                    records: vec![record],
                    ..original
                },
            )
            .await
            .unwrap();
        let before = unrelated(&store, &id).await;
        assert!(matches!(
            store.purge_import_bill("demo", &id, request).await,
            Err(ImportError::Binding)
        ));
        assert_eq!(unrelated(&store, &id).await, before);
        assert!(store.bill_details("demo", &id).await.is_ok());
    }
}

#[tokio::test]
async fn native_bill_purge_requires_owner_auth_and_reports_removed_intake_as_a_clear_conflict() {
    let app = support::App::start().await;
    let f = fixture(&app.store).await;
    let http = reqwest::Client::new();
    let url = format!("{}/v1/imports/bills/{}/purge", app.url, f.bill_id);
    assert_eq!(
        http.post(&url)
            .json(&f.request)
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let response = http
        .post(&url)
        .bearer_auth(support::TOKEN)
        .json(&f.request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let receipt: ImportBillPurgeReceipt = response.json().await.unwrap();
    assert_eq!(
        receipt,
        ImportBillPurgeReceipt {
            removed: true,
            records: 2,
            documents: 3
        }
    );
    let response = http
        .post(format!("{}/v1/imports", app.url))
        .bearer_auth(support::TOKEN)
        .json(&f.original)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    #[derive(serde::Deserialize)]
    struct ApiError {
        error_code: String,
    }
    assert_eq!(
        response.json::<ApiError>().await.unwrap().error_code,
        "import_bill_removed"
    );
    assert_eq!(
        http.post(&url)
            .bearer_auth(support::TOKEN)
            .json(&f.request)
            .send()
            .await
            .unwrap()
            .json::<ImportBillPurgeReceipt>()
            .await
            .unwrap(),
        ImportBillPurgeReceipt::default()
    );
    app.close().await;
}
