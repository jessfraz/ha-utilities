//! Explicit fictional data for local UI verification, never run during normal startup.
use chrono::{Duration, Utc};
use ha_utilities::{config::Config, models::*, store::Store, utility::SyncPage};
use rust_decimal::Decimal;
use std::path::Path;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    anyhow::ensure!(args.len() == 3, "usage: seed DATABASE CONFIG");
    let config: Config = toml::from_str(&std::fs::read_to_string(&args[2])?)?;
    anyhow::ensure!(
        config.deployment == "demo" && config.wm.is_none() && config.bayou.is_none(),
        "seed requires the fictional, unconnected example configuration"
    );
    let store = Store::open(Path::new(&args[1])).await?;
    store.register(&config).await?;
    let now = Utc::now();
    let today = now.date_naive();
    for binding in &config.accounts {
        let mut observations = Vec::new();
        for index in 0..35 {
            observations.push(Observation::Bill(Bill {
                id: format!("{}:bill:demo-{index:03}", binding.id),
                account_id: binding.id.clone(),
                provider: binding.provider,
                issued_on: today - Duration::days(2 + i64::from(index) * 7),
                due_on: Some(today + Duration::days(5)),
                period_from: Some(today - Duration::days(32 + i64::from(index) * 7)),
                period_to: Some(today - Duration::days(3 + i64::from(index) * 7)),
                total: Money {
                    minor_units: 8742 + i64::from(index) * 113,
                    currency: "USD".to_owned(),
                },
                outstanding_balance: if binding.provider == ProviderId::Bayou {
                    Some(Money {
                        minor_units: 1200,
                        currency: "USD".to_owned(),
                    })
                } else {
                    None
                },
                source_status: if binding.provider == ProviderId::Bayou {
                    Some("unlocked".to_owned())
                } else {
                    None
                },
                observed_at: now,
            }));
        }
        if binding.provider == ProviderId::WasteManagement {
            observations.push(Observation::Balance(Balance {
                id: "demo-wm:balance:current".to_owned(),
                account_id: binding.id.clone(),
                amount: Money {
                    minor_units: 8742,
                    currency: "USD".to_owned(),
                },
                as_of: now,
                observed_at: now,
            }));
            for index in 0..4 {
                let day = today + Duration::days(index * 7 + 1);
                observations.push(Observation::Collection(Collection {
                    id: format!("demo-wm:collection:demo-waste:{day}"),
                    account_id: binding.id.clone(),
                    service_id: "demo-waste".to_owned(),
                    scheduled_date: day,
                    status: CollectionStatus::Scheduled,
                    completed_at: None,
                    eta: None,
                    exception: None,
                    observed_at: now,
                }));
            }
            let day = today - Duration::days(6);
            observations.push(Observation::Collection(Collection {
                id: format!("demo-wm:collection:demo-waste:{day}"),
                account_id: binding.id.clone(),
                service_id: "demo-waste".to_owned(),
                scheduled_date: day,
                status: CollectionStatus::Exception,
                completed_at: None,
                eta: None,
                exception: Some("Container not accessible".to_owned()),
                observed_at: now,
            }));
        } else {
            let midnight = today
                .and_hms_opt(0, 0, 0)
                .ok_or_else(|| anyhow::anyhow!("invalid seed date"))?
                .and_utc();
            for index in 0..72 {
                let start = midnight - Duration::hours(72 - index);
                observations.push(Observation::Usage(Usage {
                    id: format!("demo-energy:usage:demo-{index:03}"),
                    account_id: binding.id.clone(),
                    service_id: "demo-electric".to_owned(),
                    start,
                    end: start + Duration::hours(1),
                    quantity: Decimal::from(350 + index * 11),
                    unit: UsageUnit::Wh,
                    observed_at: now,
                }));
            }
        }
        let coverage = [
            Capability::Bills,
            Capability::Balances,
            Capability::Payments,
            Capability::Services,
            Capability::Collections,
            Capability::Usage,
        ]
        .into_iter()
        .map(|capability| {
            let unsupported = match binding.provider {
                ProviderId::WasteManagement => capability == Capability::Usage,
                ProviderId::Bayou => {
                    matches!(capability, Capability::Payments | Capability::Collections)
                }
            };
            Coverage {
                account_id: binding.id.clone(),
                capability,
                state: if unsupported {
                    CoverageState::Unsupported
                } else {
                    CoverageState::Partial
                },
                reason: Some(
                    if unsupported {
                        "provider_not_supported"
                    } else {
                        "fictional_demo"
                    }
                    .to_owned(),
                ),
                checked_at: now,
                from: Some(now - Duration::days(366)),
                to: Some(now),
            }
        })
        .collect();
        store
            .ingest(
                binding,
                &SyncPage {
                    observations,
                    coverage,
                    continuation: None,
                },
            )
            .await?;
        let job = store.start_job(&binding.id).await?;
        store
            .finish_job(&job.id, SyncState::Partial, 0, None)
            .await?;
    }
    store.pool().close().await;
    Ok(())
}
