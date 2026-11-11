mod support;
use ha_utilities::models::*;
use reqwest::StatusCode;

#[tokio::test]
async fn real_http_auth_pagination_and_sync_outcomes() {
    let app = support::App::start().await;
    let client = reqwest::Client::new();
    let rename = client
        .post(format!(
            "{}/v1/connections/fictional-connection/rename",
            app.url
        ))
        .json(&ConnectionRename {
            name: "Example utility".to_owned(),
            expected_name: "Original label".to_owned(),
        });
    assert_eq!(
        rename.send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
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
