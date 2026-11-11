mod support;
use ha_utilities::models::*;
use reqwest::StatusCode;

#[tokio::test]
async fn nameless_bindings_preserve_identity_and_existing_bills() {
    let app = support::App::start().await;
    let original: String =
        sqlx::query_scalar("SELECT binding_hash FROM accounts WHERE id='demo-wm'")
            .fetch_one(app.store.pool())
            .await
            .unwrap();
    let bill = app
        .store
        .bill("demo-wm:bill:0", &["demo-wm".to_owned()])
        .await
        .unwrap();
    let configuration = include_str!("../config/example.toml");
    let config: ha_utilities::config::Config = toml::from_str(configuration).unwrap();
    app.store.register(&config).await.unwrap();
    let updated: String =
        sqlx::query_scalar("SELECT binding_hash FROM accounts WHERE id='demo-wm'")
            .fetch_one(app.store.pool())
            .await
            .unwrap();
    assert_eq!(updated, original);
    assert_eq!(
        app.store
            .bill(&bill.id, &["demo-wm".to_owned()])
            .await
            .unwrap(),
        bill
    );
    let client = ha_utilities::client::Client::new(
        &app.url,
        ha_utilities::client::Transport::Native,
        support::TOKEN,
    )
    .unwrap();
    let accounts = client.accounts().await.unwrap();
    let wm = accounts
        .items
        .iter()
        .find(|account| account.id == "demo-wm")
        .unwrap();
    assert_eq!(wm.services[0].kind, ServiceKind::Waste);
    assert_eq!(wm.connection_id, None);
}

#[tokio::test]
async fn summary_names_only_stale_manual_accounts_and_clears_removed_accounts() {
    let app = support::App::start().await;
    let fresh = &app.config.accounts[0];
    let stale = &app.config.accounts[1];
    let now = chrono::Utc::now();
    let last_synced_at = now - chrono::Duration::hours(25);
    for (account, state, at) in [
        (fresh, SyncState::Complete, now),
        (stale, SyncState::Partial, last_synced_at),
        (stale, SyncState::Failed, now),
    ] {
        let job = app.store.start_job(&account.id).await.unwrap();
        app.store.finish_job(&job.id, state, 0, None).await.unwrap();
        sqlx::query("UPDATE sync_jobs SET finished_at=? WHERE id=?")
            .bind(at.to_rfc3339())
            .bind(&job.id)
            .execute(app.store.pool())
            .await
            .unwrap();
    }
    let client = reqwest::Client::new();
    let summary: Summary = client
        .get(format!("{}/v1/summary", app.url))
        .bearer_auth(support::TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        summary.stale_manual_accounts,
        vec![StaleManualAccount {
            account_id: stale.id.clone(),
            last_synced_at: Some(last_synced_at),
        }]
    );
    assert!(summary.manual_accounts_stale);
    assert!(summary.stale_automatic_accounts.is_empty());

    let mut config = (*app.config).clone();
    config.accounts.retain(|account| account.id == fresh.id);
    app.store.register(&config).await.unwrap();
    let summary: Summary = client
        .get(format!("{}/v1/summary", app.url))
        .bearer_auth(support::TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(summary.stale_manual_accounts.is_empty());
    assert!(!summary.manual_accounts_stale);
    app.close().await;
}

#[tokio::test]
async fn real_http_auth_pagination_and_sync_outcomes() {
    let app = support::App::start().await;
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{}/v1/bills", app.url))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let first: Page<Bill> = client
        .get(format!("{}/v1/bills?limit=2", app.url))
        .bearer_auth(support::TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first.items.len(), 2);
    let next = first.next_page.unwrap();
    let second: Page<Bill> = client
        .get(format!("{}/v1/bills", app.url))
        .query(&[("limit", "2"), ("page", next.as_str())])
        .bearer_auth(support::TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(second.items.len(), 2);
    assert!(
        second
            .items
            .iter()
            .all(|b| !first.items.iter().any(|old| old.id == b.id))
    );
    assert_eq!(
        client
            .get(format!("{}/v1/bills", app.url))
            .query(&[
                ("limit", "2"),
                ("account", "demo-energy"),
                ("page", next.as_str())
            ])
            .bearer_auth(support::TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );
    for query in [
        "limit=0",
        "limit=201",
        "account=unbound",
        "from=2026-03-02&to=2026-03-01",
        "page=garbage",
    ] {
        assert_eq!(
            client
                .get(format!("{}/v1/bills?{query}", app.url))
                .bearer_auth(support::TOKEN)
                .send()
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let bill: Bill = client
        .get(format!("{}/v1/bills/{}", app.url, first.items[0].id))
        .bearer_auth(support::TOKEN)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(bill, first.items[0]);
    assert!(bill.source_status.is_none());
    let cookie = format!("utilities_session={}", support::TOKEN);
    let input = SyncInput {
        account: "demo-wm".to_owned(),
        from: chrono::Utc::now().date_naive(),
        to: chrono::Utc::now().date_naive(),
    };
    assert_eq!(
        client
            .post(format!("{}/v1/sync", app.url))
            .header("Cookie", &cookie)
            .json(&input)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let mut job: SyncJob = client
        .post(format!("{}/v1/sync", app.url))
        .bearer_auth(support::TOKEN)
        .json(&input)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    for _ in 0..100 {
        job = client
            .get(format!("{}/v1/sync/{}", app.url, job.id))
            .bearer_auth(support::TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if job.state != SyncState::Running {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(job.state, SyncState::Failed);
    assert_eq!(job.error.as_deref(), Some("credentials_not_configured"));
    let health: Health = client
        .get(format!("{}/health", app.url))
        .bearer_auth(support::TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health.status, "degraded");
    assert!(
        health
            .coverage
            .iter()
            .any(|c| c.state == CoverageState::NotConfigured)
    );
    assert_eq!(
        client
            .post(format!("{}/v1/payments", app.url))
            .bearer_auth(support::TOKEN)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    app.close().await;
}

#[tokio::test]
async fn native_login_cookie_is_http_only_and_cross_origin_mutations_fail() {
    let app = support::App::start().await;
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let result = client.get(&app.url).send().await.unwrap();
    assert_eq!(result.status(), StatusCode::SEE_OTHER);
    assert_eq!(result.headers()["location"], "login");
    #[derive(serde::Serialize)]
    struct Login<'a> {
        token: &'a str,
    }
    assert_eq!(
        client
            .post(format!("{}/login", app.url))
            .json(&Login {
                token: support::TOKEN
            })
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    let response = client
        .post(format!("{}/login", app.url))
        .header("x-utilities-request", "1")
        .json(&Login {
            token: support::TOKEN,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let cookie = response.headers()["set-cookie"].to_str().unwrap();
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
    let page = client
        .get(&app.url)
        .header("Cookie", format!("utilities_session={}", support::TOKEN))
        .send()
        .await
        .unwrap();
    assert_eq!(page.status(), StatusCode::OK);
    assert_eq!(page.headers()["cache-control"], "no-store");
    app.close().await;
}
