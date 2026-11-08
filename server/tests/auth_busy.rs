use std::fs::File;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bread_tracker_server::{auth, db};
use serde_json::Value;

struct ServerProcess {
    child: Option<Child>,
    base: String,
    log: PathBuf,
}

impl ServerProcess {
    async fn start(client: &reqwest::Client, db_path: &Path, log: PathBuf) -> Self {
        let port = free_port();
        let out = File::create(&log).expect("create server log");
        let err = out.try_clone().expect("clone server log");
        let child = Command::new(env!("CARGO_BIN_EXE_bread-tracker-server"))
            .env("BREADTRACKER_DATABASE_PATH", db_path)
            .env("BREADTRACKER_LISTEN", format!("127.0.0.1:{port}"))
            .env("BREADTRACKER_REQUESTS_PER_MINUTE", "1000")
            // This test waits out two busy timeouts. Pin the production value so a CI-wide
            // override (for example the 30 s used by the loaded test job) cannot push the
            // request past the 30 s deadline below.
            .env("BREADTRACKER_BUSY_TIMEOUT_MS", "5000")
            .env("RUST_LOG", "warn")
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("spawn bread-tracker-server");
        let server = Self {
            child: Some(child),
            base: format!("http://127.0.0.1:{port}"),
            log,
        };
        server.wait_ready(client).await;
        server
    }

    async fn wait_ready(&self, client: &reqwest::Client) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if Instant::now() > deadline {
                panic!(
                    "server at {} did not become ready; log:\n{}",
                    self.base,
                    std::fs::read_to_string(&self.log).unwrap_or_default()
                );
            }
            let ready = client
                .get(format!("{}/healthz", self.base))
                .timeout(Duration::from_millis(500))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            if ready {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local address").port()
}

async fn me(client: &reqwest::Client, base: &str, token: &str) -> (u16, Value) {
    let response = client
        .get(format!("{base}/me"))
        .bearer_auth(token)
        .send()
        .await
        .expect("GET /me");
    let status = response.status().as_u16();
    let body = response.json().await.unwrap_or(Value::Null);
    (status, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_writer_outlasting_busy_timeout_refuses_admission_with_503_and_spends_no_budget() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("busy.db");
    let url = format!("sqlite://{}", db_path.display());
    let setup = db::connect(&url).await.unwrap();
    let code = auth::create_pairing_code(&setup).await.unwrap();
    let (device, token) = auth::redeem(&setup, &code, "phone").await.unwrap().unwrap();
    // Make the informational last_seen update eligible so the test also notices if the
    // authentication path starts writing before admission has succeeded.
    sqlx::query("UPDATE devices SET last_seen = 0 WHERE id = ?")
        .bind(&device)
        .execute(&setup)
        .await
        .unwrap();

    let client = reqwest::Client::new();
    let server = ServerProcess::start(&client, &db_path, directory.path().join("busy.log")).await;

    // A second connection takes the writer exactly as another process would. It stays open
    // across both admission attempts, so each `BEGIN IMMEDIATE` has to wait out the 5 s
    // busy_timeout before SQLite reports BUSY.
    let mut holder = setup.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *holder)
        .await
        .unwrap();

    let base = server.base.clone();
    let request_client = client.clone();
    let request_token = token.clone();
    let request = tokio::spawn(async move { me(&request_client, &base, &request_token).await });
    let (status, body) = tokio::time::timeout(Duration::from_secs(30), request)
        .await
        .expect("authenticated request must finish within two busy timeouts")
        .unwrap();

    assert_eq!(
        status, 503,
        "a lock held past busy_timeout must be a retryable refusal, not 500: {body}"
    );
    assert_eq!(body["error"], "server is busy, try again");

    // The request never got past admission, so the shared counter is untouched rather than
    // left half-incremented by the failed transaction.
    let budget_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM authenticated_rate_limits WHERE device_id = ?")
            .bind(&device)
            .fetch_one(&setup)
            .await
            .unwrap();
    assert_eq!(budget_rows, 0, "no budget row was created");
    let last_seen: i64 = sqlx::query_scalar("SELECT last_seen FROM devices WHERE id = ?")
        .bind(&device)
        .fetch_one(&setup)
        .await
        .unwrap();
    assert_eq!(last_seen, 0, "the informational write did not run");

    // Both attempts and the final mapping are visible in the server log.
    let log = server.log();
    assert!(
        log.contains("database is busy; retrying once"),
        "the shared retry policy ran; log:\n{log}"
    );
    assert!(
        log.contains("request admission stayed busy after retry"),
        "the exhausted retry mapped to 503; log:\n{log}"
    );

    sqlx::query("ROLLBACK").execute(&mut *holder).await.unwrap();
    drop(holder);

    // Once the writer is gone the same token is admitted normally, with exactly one
    // request charged and the last_seen write applied.
    let (status, body) = me(&client, &server.base, &token).await;
    assert_eq!(status, 200, "{body}");
    let requests: i64 =
        sqlx::query_scalar("SELECT requests FROM authenticated_rate_limits WHERE device_id = ?")
            .bind(&device)
            .fetch_one(&setup)
            .await
            .unwrap();
    assert_eq!(requests, 1, "the refused request spent no budget");
    let last_seen: i64 = sqlx::query_scalar("SELECT last_seen FROM devices WHERE id = ?")
        .bind(&device)
        .fetch_one(&setup)
        .await
        .unwrap();
    assert!(last_seen > 0, "the successful request updated last_seen");
}
