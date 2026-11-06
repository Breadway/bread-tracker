use std::fs::File;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bread_tracker_server::{auth, db};

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
            .env("BREADTRACKER_REQUESTS_PER_MINUTE", "5")
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

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local address").port()
}

async fn me(client: &reqwest::Client, base: &str, token: &str) -> u16 {
    client
        .get(format!("{base}/me"))
        .bearer_auth(token)
        .send()
        .await
        .expect("GET /me")
        .status()
        .as_u16()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_processes_share_one_budget_and_a_restart_keeps_the_window() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("quota.db");
    let url = format!("sqlite://{}", db_path.display());
    let setup = db::connect(&url).await.unwrap();
    let code = auth::create_pairing_code(&setup).await.unwrap();
    let (device, token) = auth::redeem(&setup, &code, "phone").await.unwrap().unwrap();

    let client = reqwest::Client::new();
    let first = ServerProcess::start(&client, &db_path, directory.path().join("first.log")).await;
    let mut second =
        ServerProcess::start(&client, &db_path, directory.path().join("second.log")).await;

    // Ten concurrent authenticated requests, split across two OS processes that
    // share the database and the bearer token. A process-local budget admits all
    // ten; the shared budget admits exactly five.
    let mut tasks = Vec::new();
    for index in 0..10 {
        let client = client.clone();
        let base = if index % 2 == 0 {
            first.base.clone()
        } else {
            second.base.clone()
        };
        let token = token.clone();
        tasks.push(tokio::spawn(
            async move { me(&client, &base, &token).await },
        ));
    }
    let mut admitted = 0;
    let mut refused = 0;
    for task in tasks {
        match task.await.unwrap() {
            200 => admitted += 1,
            429 => refused += 1,
            other => panic!("unexpected GET /me status {other}"),
        }
    }
    assert_eq!(
        (admitted, refused),
        (5, 5),
        "two processes must share one five-request budget"
    );
    let stored: i64 =
        sqlx::query_scalar("SELECT requests FROM authenticated_rate_limits WHERE device_id = ?")
            .bind(&device)
            .fetch_one(&setup)
            .await
            .unwrap();
    assert_eq!(stored, 5, "denied requests saturate the stored counter");

    // The exhausted token stays refused, while a second device token on the same
    // server still has its own full budget.
    assert_eq!(me(&client, &first.base, &token).await, 429);
    let other_code = auth::create_pairing_code(&setup).await.unwrap();
    let (_, other_token) = auth::redeem(&setup, &other_code, "tablet")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(me(&client, &first.base, &other_token).await, 200);

    // A fresh process opening the same database must honor the spent budget.
    second.stop();
    let restarted =
        ServerProcess::start(&client, &db_path, directory.path().join("restarted.log")).await;
    assert_eq!(
        me(&client, &restarted.base, &token).await,
        429,
        "restarting a process must not reset the shared window"
    );

    // Expiry reopens the budget in the same shared row, not in process memory.
    sqlx::query("UPDATE authenticated_rate_limits SET window_started_at = 0 WHERE device_id = ?")
        .bind(&device)
        .execute(&setup)
        .await
        .unwrap();
    assert_eq!(me(&client, &restarted.base, &token).await, 200);
    let stored: i64 =
        sqlx::query_scalar("SELECT requests FROM authenticated_rate_limits WHERE device_id = ?")
            .bind(&device)
            .fetch_one(&setup)
            .await
            .unwrap();
    assert_eq!(stored, 1, "the expired window restarts at one");
}
