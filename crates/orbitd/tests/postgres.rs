//! Provisioning against a real PostgreSQL. Skipped unless `ORBIT_CLOUD_TEST_POSTGRES_URL` holds a superuser URL, e.g.
//! `postgres://postgres:secret@127.0.0.1:5432/postgres` (CI starts a postgres:17 service for it).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use orbitd::api::{AppState, router};
use orbitd::executor::Executor;
use orbitd::node::Node;
use orbitd::provision::{Postgres, names};
use serde_json::{Value, json};
use tower::ServiceExt;

fn url() -> Option<String> {
    std::env::var("ORBIT_CLOUD_TEST_POSTGRES_URL")
        .ok()
        .filter(|u| !u.is_empty())
}

async fn connect(url: &str) -> Result<tokio_postgres::Client, tokio_postgres::Error> {
    let (c, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls).await?;
    tokio::spawn(conn);
    Ok(c)
}

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", oc_store::random_id(4).to_lowercase())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provisioning_is_idempotent_isolated_and_purgeable() {
    let Some(admin_url) = url() else {
        eprintln!("skipped: ORBIT_CLOUD_TEST_POSTGRES_URL is not set");
        return;
    };
    let store = Arc::new(oc_store::Store::open_in_memory().unwrap());
    let pg = Postgres::from_url(&admin_url).unwrap();
    let (a, b) = (unique("pga"), unique("pgb"));

    let da = pg.ensure(&store, &a).await.unwrap();
    let again = pg.ensure(&store, &a).await.unwrap();
    assert_eq!(
        da, again,
        "a second ensure changes nothing and keeps the password"
    );
    assert_eq!(da.role, names(&a).role);
    let db = pg.ensure(&store, &b).await.unwrap();
    assert_ne!(da.password, db.password);

    // The app role can use its database: create a table, insert, read.
    let c = connect(&pg.database_url(&da, true)).await.unwrap();
    c.batch_execute("create table t (id int primary key, v text); insert into t values (1, 'x')")
        .await
        .unwrap();
    let rows = c.query("select v from t", &[]).await.unwrap();
    assert_eq!(rows[0].get::<_, String>(0), "x");

    // It cannot log into another app's database, nor is it a superuser or able to create roles.
    let cross = pg
        .database_url(&da, true)
        .replace(&format!("/{}", da.db_name), &format!("/{}", db.db_name));
    assert!(
        connect(&cross).await.is_err(),
        "app a must not connect to app b's database"
    );
    let e = c.batch_execute("create role sneaky").await.unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("42501"), "{e}");

    // A password that was never stored (interrupted run) is replaced; the old one stops working.
    store.delete_app_database(&a).unwrap();
    let redone = pg.ensure(&store, &a).await.unwrap();
    assert_ne!(redone.password, da.password);
    assert!(
        connect(&pg.database_url(&da, true)).await.is_err(),
        "old password is dead"
    );
    let c2 = connect(&pg.database_url(&redone, true)).await.unwrap();
    assert_eq!(
        c2.query("select count(*) from t", &[]).await.unwrap()[0].get::<_, i64>(0),
        1,
        "data kept"
    );
    drop((c, c2));

    // Status: no password unless asked.
    let st = pg.status(&store, &a, false, true).await.unwrap();
    assert_eq!(st["provisioned"], true);
    assert_eq!(st["reachable"], true);
    assert!(st["size_bytes"].as_i64().unwrap() > 0);
    assert!(st.get("password").is_none());
    assert!(!st["url"].as_str().unwrap().contains(&redone.password));
    let st = pg.status(&store, &a, true, true).await.unwrap();
    assert!(st["url"].as_str().unwrap().contains(&redone.password));

    // A foreign role of the same name is refused, not taken over.
    let admin = connect(&admin_url).await.unwrap();
    let foreign = unique("pgf");
    admin
        .batch_execute(&format!("create role \"{}\" login", names(&foreign).role))
        .await
        .unwrap();
    let err = pg.ensure(&store, &foreign).await.unwrap_err();
    assert!(
        err.to_string().contains("not created by orbit-cloud"),
        "{err}"
    );
    assert!(
        !pg.purge(&store, &foreign).await.unwrap_or(false),
        "a foreign role is not dropped"
    );
    admin
        .batch_execute(&format!("drop role \"{}\"", names(&foreign).role))
        .await
        .unwrap();

    // Purge drops database and role, and is repeatable.
    assert!(pg.purge(&store, &a).await.unwrap());
    assert!(pg.purge(&store, &b).await.unwrap());
    assert!(
        !pg.purge(&store, &a).await.unwrap(),
        "nothing left to purge"
    );
    let left = admin
        .query("select 1 from pg_database where datname = any($1) union all select 1 from pg_roles where rolname = any($1)", &[&vec![names(&a).database, names(&b).database]])
        .await
        .unwrap();
    assert!(left.is_empty());
    assert!(store.app_database(&a).unwrap().is_none());
}

const NOOP: &str = r#"(module (memory (export "memory") 1) (func (export "_start")))"#;

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    spec: Option<&Value>,
    body: Vec<u8>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if let Some(s) = spec {
        b = b.header("x-orbit-spec", s.to_string());
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deploy_provisions_the_database_and_delete_keeps_it_until_purged() {
    let Some(admin_url) = url() else {
        eprintln!("skipped: ORBIT_CLOUD_TEST_POSTGRES_URL is not set");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(oc_store::Store::open(&dir.path().join("t.db")).unwrap());
    let exec = Executor::new(store, dir.path(), false)
        .unwrap()
        .with_postgres(Arc::new(Postgres::from_url(&admin_url).unwrap()));
    let node = Node::new(Arc::new(exec));
    let app = router(AppState { node, token: None });
    let name = unique("pgapp");
    let wasm = wat::parse_str(NOOP).unwrap();

    let (s, st) = call(
        &app,
        "GET",
        &format!("/v1/apps/{name}/database"),
        None,
        vec![],
    )
    .await;
    assert_eq!(
        s,
        StatusCode::NOT_FOUND,
        "an unknown app has no database: {st}"
    );

    let (s, _) = call(
        &app,
        "POST",
        &format!("/v1/apps/{name}/deployments"),
        Some(&json!({ "postgres": true })),
        wasm,
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, st) = call(
        &app,
        "GET",
        &format!("/v1/apps/{name}/database"),
        None,
        vec![],
    )
    .await;
    assert_eq!(
        (st["provisioned"].clone(), st["enabled"].clone()),
        (json!(true), json!(true)),
        "{st}"
    );
    assert!(
        st["url"]
            .as_str()
            .unwrap()
            .starts_with(&format!("postgres://oc_{}@", name.replace('-', "_")))
    );
    assert!(st.get("password").is_none());
    let (_, with) = call(
        &app,
        "GET",
        &format!("/v1/apps/{name}/database?password=true"),
        None,
        vec![],
    )
    .await;
    let url_with_pw = with["url"].as_str().unwrap().to_owned();
    assert!(
        connect(&url_with_pw).await.is_ok(),
        "the advertised URL logs in"
    );

    // Deleting the app keeps the database (and its data); a redeploy finds it again.
    let (s, _) = call(&app, "DELETE", &format!("/v1/apps/{name}"), None, vec![]).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(
        connect(&url_with_pw).await.is_ok(),
        "database survives the app"
    );
    let (_, st) = call(
        &app,
        "GET",
        &format!("/v1/apps/{name}/database"),
        None,
        vec![],
    )
    .await;
    assert_eq!(
        (st["provisioned"].clone(), st["enabled"].clone()),
        (json!(true), json!(false))
    );

    // ?purge=true on the (now app-less) name removes the orphaned database.
    let (s, _) = call(
        &app,
        "DELETE",
        &format!("/v1/apps/{name}?purge=true"),
        None,
        vec![],
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(connect(&url_with_pw).await.is_err(), "purged");
    let (s, _) = call(
        &app,
        "GET",
        &format!("/v1/apps/{name}/database"),
        None,
        vec![],
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "nothing left of the app");

    // Without --postgres-url the spec is refused.
    let node2 = Node::new(Arc::new(
        Executor::new(
            Arc::new(oc_store::Store::open_in_memory().unwrap()),
            dir.path(),
            false,
        )
        .unwrap(),
    ));
    let app2 = router(AppState {
        node: node2,
        token: None,
    });
    let (s, e) = call(
        &app2,
        "POST",
        "/v1/apps/x/deployments",
        Some(&json!({ "postgres": true })),
        wat::parse_str(NOOP).unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
}
